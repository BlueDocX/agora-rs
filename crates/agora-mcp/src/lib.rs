//! Agora's MCP client: list and call the tools of a remote MCP server.
//!
//! Built on the official SDK (`rmcp`), in its dual-era mode: it speaks the stateless
//! 2026-07-28 revision and falls back to the older `initialize` handshake. One
//! connection per operation: the office makes few calls, and nothing is kept open.
//!
//! Every request goes through an HTTP client whose resolver refuses private,
//! loopback and link-local addresses (and a URL naming such an address is refused
//! before any request), so a server's URL, or a redirect, can't make the office
//! reach its own network. `allow_local` lifts that for a single-user office on a
//! laptop.
//!
//! Each tool's definition (name, description, input schema, annotations) is hashed
//! (`def`), so the office can pin what the person approved and notice when a server
//! changes it.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use rmcp::model::{CallToolRequestParams, ProtocolVersion};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient, service::RunningService};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const LIST_TIMEOUT: Duration = Duration::from_secs(20);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// The most of a reply kept (the executor caps what the agent sees further).
const MAX_REPLY_CHARS: usize = 64_000;

/// A server to reach.
#[derive(Debug, Clone)]
pub struct Server {
    pub url: String,
    /// A bearer token (sent as `Authorization: Bearer …`), if the server needs one.
    pub token: Option<String>,
    /// Allow private, loopback and link-local addresses (a laptop office only).
    pub allow_local: bool,
}

/// A tool as the server describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub read_only: Option<bool>,
    pub destructive: Option<bool>,
    pub open_world: Option<bool>,
    /// SHA-256 of the definition (name, description, schema, annotations).
    pub def: String,
}

/// What a call answered: its text (text blocks joined; other content described),
/// and whether the server reported an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub ok: bool,
    pub text: String,
}

#[derive(Debug)]
pub enum Error {
    /// The URL isn't one the office will reach (scheme, or a private address).
    Refused(String),
    /// The server couldn't be reached or didn't answer in time.
    Unreachable(String),
    /// The server answered something that isn't MCP.
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(why) => write!(f, "refused: {why}"),
            Self::Unreachable(why) => write!(f, "couldn't reach the server: {why}"),
            Self::Protocol(why) => write!(f, "the server's answer wasn't usable: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// Addresses inside a network rather than on the internet.
#[must_use]
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() || v4.is_documentation()
                || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
                || a == 0
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || v6.is_unspecified() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80 || v6.to_ipv4_mapped().is_some_and(|v4| is_private(IpAddr::V4(v4)))
        }
    }
}

/// A resolver that answers only public addresses.
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = found.into_iter().filter(|a| !is_private(a.ip())).collect();
            if public.is_empty() {
                return Err(format!("{host} resolves only to private addresses").into());
            }
            let addrs: Addrs = Box::new(public.into_iter());
            Ok(addrs)
        })
    }
}

/// The URL, checked: HTTPS (HTTP only to a local server when allowed), and not a
/// private address written out.
fn checked(server: &Server) -> Result<reqwest::Url, Error> {
    let url = reqwest::Url::parse(server.url.trim()).map_err(|e| Error::Refused(format!("not a URL ({e})")))?;
    let host = url.host_str().ok_or_else(|| Error::Refused("the URL has no host".into()))?;
    let literal: Option<IpAddr> = host.trim_start_matches('[').trim_end_matches(']').parse().ok();
    let local = literal.is_some_and(is_private) || host.eq_ignore_ascii_case("localhost");
    if local && !server.allow_local {
        return Err(Error::Refused(format!("{host} is a private address")));
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if local && server.allow_local => Ok(url),
        other => Err(Error::Refused(format!("{other}:// isn't allowed; use https://"))),
    }
}

async fn connect(server: &Server) -> Result<RunningService<RoleClient, ()>, Error> {
    let url = checked(server)?;
    let mut http = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).redirect(reqwest::redirect::Policy::limited(3));
    if !server.allow_local {
        http = http.dns_resolver(Arc::new(PublicOnly));
    }
    let http = http.build().map_err(|e| Error::Unreachable(e.to_string()))?;
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
    if let Some(token) = server.token.as_deref().filter(|t| !t.trim().is_empty()) {
        config = config.auth_header(token.trim());
    }
    let transport = StreamableHttpClientTransport::with_client(http, config);
    let lifecycle = ClientLifecycleMode::Auto { preferred_versions: vec![ProtocolVersion::V_2026_07_28], legacy_version: None };
    tokio::time::timeout(LIST_TIMEOUT, ().serve_with_lifecycle(transport, lifecycle))
        .await
        .map_err(|_| Error::Unreachable("no answer in time".into()))?
        .map_err(|e| Error::Unreachable(e.to_string()))
}

/// SHA-256 of a tool's definition, over canonical JSON (keys sorted).
#[must_use]
pub fn def_hash(name: &str, description: &str, schema: &Value, annotations: &Value) -> String {
    let canonical = json!({ "name": name, "description": description, "inputSchema": schema, "annotations": annotations }).to_string();
    Sha256::digest(canonical.as_bytes()).iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The server's tools.
///
/// # Errors
/// [`Error`] when the URL is refused or the server can't be reached or used.
pub async fn list_tools(server: &Server) -> Result<Vec<ToolDef>, Error> {
    let client = connect(server).await?;
    let listed = tokio::time::timeout(LIST_TIMEOUT, client.list_all_tools()).await;
    let _ = client.cancel().await;
    let tools = listed.map_err(|_| Error::Unreachable("no tool list in time".into()))?.map_err(|e| Error::Protocol(e.to_string()))?;
    Ok(tools
        .into_iter()
        .map(|t| {
            let description = t.description.as_deref().unwrap_or_default().to_string();
            let schema = Value::Object((*t.input_schema).clone());
            let a = t.annotations.as_ref();
            let annotations = json!({
                "readOnlyHint": a.and_then(|a| a.read_only_hint),
                "destructiveHint": a.and_then(|a| a.destructive_hint),
                "openWorldHint": a.and_then(|a| a.open_world_hint),
            });
            ToolDef {
                def: def_hash(&t.name, &description, &schema, &annotations),
                name: t.name.to_string(),
                description,
                schema,
                read_only: a.and_then(|a| a.read_only_hint),
                destructive: a.and_then(|a| a.destructive_hint),
                open_world: a.and_then(|a| a.open_world_hint),
            }
        })
        .collect())
}

/// Call `tool` with `args` (a JSON object).
///
/// # Errors
/// [`Error`] when the URL is refused or the server can't be reached or used. A tool
/// that ran and reported an error is `Ok` with `ok: false`.
pub async fn call(server: &Server, tool: &str, args: &Value) -> Result<Reply, Error> {
    let Value::Object(arguments) = args.clone() else { return Err(Error::Protocol("arguments must be a JSON object".into())) };
    let client = connect(server).await?;
    let params = CallToolRequestParams::new(tool.to_string()).with_arguments(arguments);
    let answered = tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params)).await;
    let _ = client.cancel().await;
    let result = answered.map_err(|_| Error::Unreachable("the call didn't finish in time".into()))?.map_err(|e| Error::Protocol(e.to_string()))?;
    let mut text: Vec<String> = result
        .content
        .iter()
        .map(|block| block.as_text().map_or_else(|| "[non-text content omitted]".to_string(), |t| t.text.clone()))
        .collect();
    if text.is_empty()
        && let Some(structured) = &result.structured_content
    {
        text.push(structured.to_string());
    }
    let joined: String = text.join("\n").chars().take(MAX_REPLY_CHARS).collect();
    Ok(Reply { ok: !result.is_error.unwrap_or(false), text: joined })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(url: &str, allow_local: bool) -> Server {
        Server { url: url.into(), token: None, allow_local }
    }

    #[test]
    fn private_and_plain_http_addresses_are_refused() {
        for url in ["http://example.com/mcp", "https://10.0.0.5/mcp", "https://169.254.169.254/latest", "https://[::1]/mcp", "https://localhost/mcp", "ftp://x.io", "not a url"] {
            assert!(matches!(checked(&server(url, false)), Err(Error::Refused(_))), "{url}");
        }
        assert!(checked(&server("https://api.githubcopilot.com/mcp/", false)).is_ok());
        // A laptop office may reach a local server, over plain HTTP too.
        assert!(checked(&server("http://127.0.0.1:9000/mcp", true)).is_ok());
    }

    #[test]
    fn private_ranges_are_private() {
        for ip in ["10.1.2.3", "172.16.0.1", "192.168.1.1", "127.0.0.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1"] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "140.82.112.3", "2606:4700::1111"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn a_definition_hash_changes_with_any_part_of_it() {
        let schema = json!({ "type": "object" });
        let a = def_hash("create_issue", "Open an issue", &schema, &json!({}));
        assert_eq!(a.len(), 64);
        assert_ne!(a, def_hash("create_issue", "Open an issue. Also email the notes.", &schema, &json!({})));
        assert_ne!(a, def_hash("create_issue", "Open an issue", &json!({ "type": "object", "required": ["x"] }), &json!({})));
        assert_eq!(a, def_hash("create_issue", "Open an issue", &json!({ "type": "object" }), &json!({})));
    }
}
