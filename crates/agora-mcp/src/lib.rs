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

/// A server to reach. (`Debug` never shows the token.)
#[derive(Clone)]
pub struct Server {
    pub url: String,
    /// A bearer token (sent as `Authorization: Bearer …`), if the server needs one.
    pub token: Option<String>,
    /// Allow private, loopback and link-local addresses (a laptop office only).
    pub allow_local: bool,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").field("url", &redacted(&self.url)).field("token", &self.token.as_ref().map(|_| "(sealed)")).field("allow_local", &self.allow_local).finish()
    }
}

/// A URL as it may be shown or logged: no user info, no query (some servers put a
/// secret there).
#[must_use]
pub fn redacted(url: &str) -> String {
    reqwest::Url::parse(url).map_or_else(|_| "(not a URL)".into(), |u| format!("{}://{}{}{}", u.scheme(), u.host_str().unwrap_or_default(), u.port().map(|p| format!(":{p}")).unwrap_or_default(), u.path()))
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
            let [a, b, c, _] = v4.octets();
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() || v4.is_documentation() || v4.is_multicast()
                || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
                || a == 0
                || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
                || (a == 198 && (18..20).contains(&b)) // benchmarking
                || a >= 240 // reserved
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            let embedded = |hi: u16, lo: u16| is_private(IpAddr::V4(std::net::Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo))));
            v6.is_loopback() || v6.is_unspecified() || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link-local
                || (s[0] & 0xffc0) == 0xfec0 // site-local
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private(IpAddr::V4(v4)))
                || (s[0..6] == [0; 6] && embedded(s[6], s[7])) // IPv4-compatible
                || (s[0] == 0x64 && s[1] == 0xff9b && embedded(s[6], s[7])) // NAT64
                || (s[0] == 0x2002 && embedded(s[1], s[2])) // 6to4
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
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Refused("put credentials in the token, not the URL".into()));
    }
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
    // No redirects (one could point at a private address, or replay the body to
    // another host) and no proxy (it would resolve names where the resolver can't see).
    let mut http = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).redirect(reqwest::redirect::Policy::none()).no_proxy();
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
        .map_err(|e| Error::Unreachable(scrub(&e.to_string(), url.as_str())))
}

/// An error's text without the server's URL (it may carry a secret), as given or as
/// a library prints it (normalised), and without the query of any other URL in it.
fn scrub(text: &str, url: &str) -> String {
    let mut out = text.to_string();
    let normalised = reqwest::Url::parse(url.trim()).map(|u| u.to_string()).unwrap_or_default();
    for u in [url, url.trim(), normalised.as_str()] {
        if !u.is_empty() {
            out = out.replace(u, "the server").replace(u.trim_end_matches('/'), "the server");
        }
    }
    out.split(' ')
        .map(|w| match w.find("://").and_then(|i| w[i..].find(['?', '#']).map(|j| i + j)) {
            Some(q) => format!("{}?…", &w[..q]),
            None => w.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    let tools = listed.map_err(|_| Error::Unreachable("no tool list in time".into()))?.map_err(|e| Error::Protocol(scrub(&e.to_string(), &server.url)))?;
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
    let result = answered.map_err(|_| Error::Unreachable("the call didn't finish in time".into()))?.map_err(|e| Error::Protocol(scrub(&e.to_string(), &server.url)))?;
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
        for url in ["http://example.com/mcp", "https://10.0.0.5/mcp", "https://169.254.169.254/latest", "https://[::1]/mcp", "https://localhost/mcp", "ftp://x.io", "not a url", "https://user:pass@example.com/mcp", "https://2130706433/mcp"] {
            assert!(matches!(checked(&server(url, false)), Err(Error::Refused(_))), "{url}");
        }
        assert!(checked(&server("https://api.githubcopilot.com/mcp/", false)).is_ok());
        // A laptop office may reach a local server, over plain HTTP too.
        assert!(checked(&server("http://127.0.0.1:9000/mcp", true)).is_ok());
    }

    #[test]
    fn private_ranges_are_private() {
        for ip in [
            "10.1.2.3", "172.16.0.1", "192.168.1.1", "127.0.0.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1",
            "64:ff9b::a9fe:a9fe", "2002:a9fe:a9fe::1", "::a9fe:a9fe", "fec0::1", "ff02::1", "192.0.0.8", "198.18.0.1", "224.0.0.1", "240.0.0.1",
        ] {
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

/// Signing in to servers that need OAuth (Slack, Google, Sentry, Linear…), on the
/// SDK's OAuth 2.1 client: discovery, then a pre-registered client if the owner gave
/// one, else the server's client metadata documents or dynamic registration; PKCE;
/// refresh. What comes back is sealed by the caller and handed to [`oauth_token`].
pub mod oauth {
    use rmcp::transport::auth::{AuthorizationManager, AuthorizationRequest, CredentialStore, InMemoryCredentialStore, OAuthClientConfig, OAuthState, StoredCredentials};
    use serde_json::{Value, json};

    use crate::Error;

    fn failed(e: impl std::fmt::Display) -> Error {
        Error::Refused(format!("sign-in failed: {e}"))
    }

    /// A sign-in waiting for the person to come back from the provider.
    pub struct Pending {
        state: OAuthState,
        store: InMemoryCredentialStore,
        redirect: String,
        client_secret: Option<String>,
        /// Where to send the person.
        pub authorize_url: String,
        /// The `state` the provider will hand back (it keys the pending sign-in).
        pub csrf: String,
    }

    async fn manager(url: &str, allow_local: bool) -> Result<AuthorizationManager, Error> {
        let made = if allow_local { AuthorizationManager::new_allowing_private_addresses(url).await } else { AuthorizationManager::new(url).await };
        made.map_err(failed)
    }

    /// Start signing in to the server at `url`; the provider sends the person back to
    /// `redirect`. `client`: a client the owner registered with the provider (Slack and
    /// Google need one); `None` = the server's own registration.
    ///
    /// # Errors
    /// When the server's sign-in can't be discovered or started.
    pub async fn begin(url: &str, redirect: &str, client: Option<(String, Option<String>)>, allow_local: bool) -> Result<Pending, Error> {
        crate::checked(&crate::Server { url: url.to_string(), token: None, allow_local })?;
        let store = InMemoryCredentialStore::default();
        let mut m = manager(url, allow_local).await?;
        m.set_credential_store(store.clone());
        let mut state = OAuthState::Unauthorized(m);
        let mut request = AuthorizationRequest::new(redirect.to_string());
        request.client_name = Some("Agora".into());
        let client_secret = client.as_ref().and_then(|(_, s)| s.clone());
        if let Some((id, secret)) = client {
            request = request.with_preregistered_client(id);
            request.client_secret = secret;
        }
        state.start_authorization(request).await.map_err(failed)?;
        let authorize_url = state.get_authorization_url().await.map_err(failed)?;
        let csrf = reqwest::Url::parse(&authorize_url).ok().and_then(|u| u.query_pairs().find(|(k, _)| k == "state").map(|(_, v)| v.into_owned())).ok_or_else(|| failed("the provider's sign-in URL has no state"))?;
        Ok(Pending { state, store, redirect: redirect.to_string(), client_secret, authorize_url, csrf })
    }

    /// Finish with what the provider sent back: the credentials, as JSON to seal.
    ///
    /// # Errors
    /// When the code can't be exchanged.
    pub async fn finish(mut pending: Pending, code: &str) -> Result<String, Error> {
        pending.state.handle_callback(code, &pending.csrf).await.map_err(failed)?;
        let stored = pending.store.load().await.map_err(failed)?.ok_or_else(|| failed("no credentials came back"))?;
        Ok(json!({ "stored": stored, "client_secret": pending.client_secret, "redirect": pending.redirect }).to_string())
    }

    /// A current access token for the server at `url`, refreshing it if it's (nearly)
    /// expired. Answers the token, and the credentials again if they changed (seal and
    /// keep those instead).
    ///
    /// # Errors
    /// When the credentials can't be read, or the token can't be refreshed (sign in again).
    pub async fn token(url: &str, sealed_json: &str, allow_local: bool) -> Result<(String, Option<String>), Error> {
        let mut saved: Value = serde_json::from_str(sealed_json).map_err(failed)?;
        let stored: StoredCredentials = serde_json::from_value(saved["stored"].clone()).map_err(failed)?;
        let store = InMemoryCredentialStore::default();
        store.save(stored.clone()).await.map_err(failed)?;
        let mut m = manager(url, allow_local).await?;
        m.set_credential_store(store.clone());
        if let Ok(token) = m.get_access_token().await {
            return Ok((token, None));
        }
        // It needs a refresh: that takes the provider's metadata and the client.
        let metadata = m.resolve_metadata().await.map_err(failed)?.metadata;
        m.set_metadata(metadata);
        let mut config = OAuthClientConfig::new(stored.client_id.clone(), saved["redirect"].as_str().unwrap_or_default());
        config.client_secret = saved["client_secret"].as_str().map(str::to_string);
        config.scopes.clone_from(&stored.granted_scopes);
        m.configure_client(config).map_err(failed)?;
        let token = m.get_access_token().await.map_err(|e| Error::Refused(format!("sign in again: {e}")))?;
        let fresh = store.load().await.map_err(failed)?.ok_or_else(|| failed("the refreshed credentials are gone"))?;
        saved["stored"] = serde_json::to_value(fresh).map_err(failed)?;
        Ok((token, Some(saved.to_string())))
    }
}
