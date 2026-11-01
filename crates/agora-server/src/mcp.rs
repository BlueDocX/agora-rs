//! Agora as an MCP server: `POST /mcp`, JSON-RPC 2.0 over Streamable HTTP (each
//! request answered as one JSON response; no server-initiated stream).
//!
//! This is how coding CLIs (Claude Code, Codex, `OpenCode`, Gemini CLI), Meta AI
//! Connectors and any MCP client reach the office: the same tools the glasses register
//! with `WebMCP`. A member's CLI key (`Authorization: Bearer agk_…`, see [`crate::cli`])
//! says who acts. (Or, as before: the hub token as the bearer and `X-Agora-Member`.)
//!
//! There is no tool to approve a call: an agent in someone's terminal never answers
//! for the person. An errand waiting for approval says who must answer, in the office.

use std::sync::Arc;

use axum::Json;
use axum::body::to_bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::{Hub, authorized, errands, unauthorized};

const PROTOCOL: &str = "2025-11-25";
/// Versions this server speaks; any other request gets [`PROTOCOL`] back, so the
/// client can decide whether to go on.
const SUPPORTED: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
/// The longest a tool waits for an errand before answering "still working" (under
/// common request limits, such as Cloud Run's 300 s).
const MAX_WAIT_S: u64 = 240;
/// The default wait: under the shortest common MCP client timeout (60 s).
const DEFAULT_WAIT_S: u64 = 45;

fn tools() -> Value {
    json!([
        {
            "name": "office_status",
            "description": "Who is in the Agora office (and who is working from a CLI), who leads which area, each person's bot, the agents people added, the connected tools that are on, and what the bots did lately.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "run_errand",
            "description": "Give a bot an errand on the person's contacts, calendar, email, chats and files, and the office's connected tools (GitHub, Slack, …). Every call it makes goes into a hash-chained ledger, and every claim it then makes is checked against that ledger before delivery. Runs on your bot, a bot or agent named in `bot`, or the bot of the lead of `area`. Waits up to wait_seconds for the checked report; if a call needs a person's approval, says who must approve it in the office (this tool can't). Relay the report as given: it is what the ledger backs.",
            "inputSchema": { "type": "object", "properties": {
                "brief": { "type": "string", "description": "What to do, in your words. Name any address, repository or channel it should go to." },
                "bot": { "type": "string", "description": "Optional: a person's bot or an added agent, by name (see office_status)." },
                "area": { "type": "string", "description": "Optional: an area (e.g. Finance) whose lead's bot should do it." },
                "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 240, "description": "How long to wait for the report (default 45; 0 = return the id at once). If it is still working, call errand_result with its id: never start the errand again." }
            }, "required": ["brief"] },
            "annotations": { "readOnlyHint": false }
        },
        {
            "name": "errand_result",
            "description": "An errand's latest delivered message and its receipts: claims backed by the ledger, claims the ledger contradicts that were delivered (with the guard on, any still wrong after rewrites are cut and corrected from the ledger), rewrites, and any correction; or that it is still working, or waiting for someone's approval. Without an id, the newest errand.",
            "inputSchema": { "type": "object", "properties": {
                "errand_id": { "type": "string" },
                "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 240, "description": "Wait this long for it to finish (default 0)." }
            } },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "ask_errand",
            "description": "Ask the bot about an errand (e.g. 'did you read my private chats?'). It answers from its ledger and the answer is checked the same way. Waits up to wait_seconds for the answer.",
            "inputSchema": { "type": "object", "properties": {
                "errand_id": { "type": "string" },
                "question": { "type": "string" },
                "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 240, "description": "Default 45." }
            }, "required": ["errand_id", "question"] },
            "annotations": { "readOnlyHint": false }
        },
        {
            "name": "ask_about_week",
            "description": "Ask the bot about anything from this week (what it did, what failed, what's pending). It answers from its ledgers and the office's memory, every line cited, and the answer is checked against every errand's ledger before it is returned.",
            "inputSchema": { "type": "object", "properties": { "question": { "type": "string" } }, "required": ["question"] },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "post_message",
            "description": "Post in the office's chat as you: to a channel (\"#general\") or a person by name. In a channel, starting with @Name (a bot or agent) gives it the errand; its checked report comes back in that channel.",
            "inputSchema": { "type": "object", "properties": {
                "to": { "type": "string", "description": "A channel like #general, or a person's name." },
                "text": { "type": "string" }
            }, "required": ["to", "text"] },
            "annotations": { "readOnlyHint": false }
        },
        {
            "name": "verify_ledger",
            "description": "Recompute an errand's hash chain: its entries, whether it is intact, and its head hash.",
            "inputSchema": { "type": "object", "properties": { "errand_id": { "type": "string" } }, "required": ["errand_id"] },
            "annotations": { "readOnlyHint": true }
        }
    ])
}

/// Run one of the office's own handlers and return its JSON (or its error text).
async fn call_handler(response: Response) -> Result<Value, String> {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.map_err(|e| e.to_string())?;
    let value: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).map_err(|e| e.to_string())? };
    if status.is_success() { Ok(value) } else { Err(value["error"].as_str().unwrap_or("the office refused that").to_string()) }
}

/// `wait_seconds` from the arguments (`default` if absent), at most [`MAX_WAIT_S`].
fn wait(args: &Value, default: u64) -> u64 {
    args.get("wait_seconds").and_then(Value::as_u64).unwrap_or(default).min(MAX_WAIT_S)
}

/// The errand's summary once it has finished its turn, is waiting for an approval, or
/// `seconds` have passed, with a `status` line for the CLI's agent to act on.
async fn settled(hub: &Hub, id: &str, seconds: u64) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        let summary = hub.errand(id).ok_or("no such errand")?.summary();
        let status = if let Some(p) = &summary.awaiting_approval {
            Some(format!("waiting: {} needs approval from {} in the Agora office (this CLI can't approve). Call errand_result with wait_seconds to keep waiting.", p.tool, p.approver.as_deref().unwrap_or("the errand's owner")))
        } else if !summary.busy {
            // A failed turn is an error, with its reason: never an older turn's report.
            if let Some(why) = &summary.failed {
                return Err(format!("errand {id} failed: {why}"));
            }
            Some("done: relay `said.message` as is; it is what the ledger backs".to_string())
        } else {
            None
        };
        let timed_out = tokio::time::Instant::now() >= deadline;
        if status.is_some() || timed_out {
            let status = status.unwrap_or_else(|| format!("still working on {id}: call errand_result with wait_seconds"));
            let mut value = json!(summary);
            value["status"] = json!(status);
            return Ok(value);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

async fn call_tool(hub: &Arc<Hub>, headers: &HeaderMap, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "office_status" => {
            let (people, recent) = {
                let company = hub.company();
                (company.members_view(), company.recent(10))
            };
            let agents = hub.agents().all();
            Ok(json!({ "people": people, "agents": agents, "connected_tools": hub.connected_tool_ids(), "recent": recent, "errands": hub.errand_summaries().into_iter().take(5).collect::<Vec<_>>() }))
        }
        "run_errand" => {
            let brief = arg(args, "brief").ok_or("brief is required")?;
            let body = json!({ "brief": brief, "area": arg(args, "area"), "bot": arg(args, "bot") });
            let body = serde_json::from_value(body).map_err(|e| e.to_string())?;
            let started = call_handler(errands::start(State(Arc::clone(hub)), headers.clone(), Ok(Json(body))).await).await?;
            let id = started["id"].as_str().ok_or("the office didn't say which errand started")?.to_string();
            settled(hub, &id, wait(args, DEFAULT_WAIT_S)).await
        }
        "errand_result" => {
            let id = match arg(args, "errand_id") {
                Some(id) => id.to_string(),
                None => hub.newest_errand().ok_or("no errands yet")?,
            };
            settled(hub, &id, wait(args, 0)).await
        }
        "ask_errand" => {
            let id = arg(args, "errand_id").ok_or("errand_id is required")?.to_string();
            let question = arg(args, "question").ok_or("question is required")?;
            let body = serde_json::from_value(json!({ "question": question })).map_err(|e| e.to_string())?;
            call_handler(errands::ask(State(Arc::clone(hub)), axum::extract::Path(id.clone()), headers.clone(), Ok(Json(body))).await).await?;
            settled(hub, &id, wait(args, DEFAULT_WAIT_S)).await
        }
        "post_message" => {
            let body = json!({ "to": arg(args, "to").ok_or("to is required")?, "text": arg(args, "text").ok_or("text is required")? });
            let body = serde_json::from_value(body).map_err(|e| e.to_string())?;
            call_handler(crate::company::say(State(Arc::clone(hub)), headers.clone(), Ok(Json(body))).await).await
        }
        "ask_about_week" => {
            let question = arg(args, "question").ok_or("question is required")?;
            let body = serde_json::from_value(json!({ "question": question })).map_err(|e| e.to_string())?;
            call_handler(crate::office::ask(State(Arc::clone(hub)), headers.clone(), Ok(Json(body))).await).await
        }
        "verify_ledger" => {
            let id = arg(args, "errand_id").ok_or("errand_id is required")?;
            hub.errand(id).map(|r| json!(r.chain())).ok_or_else(|| "no such errand".to_string())
        }
        other => Err(format!("no tool called {other}")),
    }
}

fn reply(id: &Value, result: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

async fn handle(hub: &Arc<Hub>, headers: &HeaderMap, request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or_default();
    // A notification (no id) gets no reply.
    let id = id?;
    Some(match method {
        "initialize" => {
            let asked = request["params"]["protocolVersion"].as_str().filter(|v| SUPPORTED.contains(v)).unwrap_or(PROTOCOL);
            reply(&id, &json!({
                "protocolVersion": asked,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "agora", "title": "Agora: the office where AI can't lie about what it did", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Run errands with run_errand; read what was delivered, and its ledger receipts, with errand_result."
            }))
        }
        "ping" => reply(&id, &json!({})),
        "tools/list" => reply(&id, &json!({ "tools": tools() })),
        "tools/call" => {
            let name = request["params"]["name"].as_str().unwrap_or_default();
            let args = request["params"].get("arguments").cloned().unwrap_or_else(|| json!({}));
            match call_tool(hub, headers, name, &args).await {
                Ok(value) => reply(&id, &json!({ "content": [{ "type": "text", "text": value.to_string() }], "structuredContent": value, "isError": false })),
                Err(why) => reply(&id, &json!({ "content": [{ "type": "text", "text": why }], "isError": true })),
            }
        }
        other => failure(&id, -32601, &format!("method not found: {other}")),
    })
}

/// A request a web page could have sent cross-site: the spec asks servers to check
/// `Origin`, and requiring a JSON body forces the browser to ask (preflight) first.
fn cross_site(headers: &HeaderMap) -> bool {
    let header = |name| headers.get(name).and_then(|v: &axum::http::HeaderValue| v.to_str().ok()).unwrap_or_default();
    let json = header(axum::http::header::CONTENT_TYPE.as_str()).trim_start().to_ascii_lowercase().starts_with("application/json");
    let origin = header("origin");
    let host = header("host");
    let foreign = !origin.is_empty() && origin.split_once("://").is_none_or(|(_, rest)| rest != host);
    !json || foreign
}

pub(crate) async fn post(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    if cross_site(&headers) {
        return (StatusCode::FORBIDDEN, Json(failure(&Value::Null, -32600, "send JSON (Content-Type: application/json) from this origin"))).into_response();
    }
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, Json(failure(&Value::Null, -32700, "parse error"))).into_response();
    };
    match message {
        Value::Array(batch) => {
            // Side by side: one long wait mustn't hold up the rest.
            let out: Vec<Value> = futures_util::future::join_all(batch.iter().map(|request| handle(&hub, &headers, request))).await.into_iter().flatten().collect();
            if out.is_empty() { StatusCode::ACCEPTED.into_response() } else { Json(Value::Array(out)).into_response() }
        }
        request => match handle(&hub, &headers, &request).await {
            Some(reply) => Json(reply).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

/// No server-initiated stream: GET is not offered (the spec allows 405).
pub(crate) async fn get() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().expect("header"));
        }
        h
    }

    #[test]
    fn only_same_origin_json_gets_in() {
        assert!(!cross_site(&headers(&[("content-type", "application/json"), ("host", "127.0.0.1:8080")])), "a non-browser client");
        assert!(!cross_site(&headers(&[("content-type", "application/json"), ("host", "127.0.0.1:8080"), ("origin", "http://127.0.0.1:8080")])));
        assert!(cross_site(&headers(&[("content-type", "text/plain"), ("host", "127.0.0.1:8080")])), "a no-preflight form post");
        assert!(cross_site(&headers(&[("content-type", "application/json"), ("host", "127.0.0.1:8080"), ("origin", "https://evil.example")])));
    }
}
