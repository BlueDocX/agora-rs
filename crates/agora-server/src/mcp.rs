//! Agora as an MCP server: `POST /mcp`, JSON-RPC 2.0 over Streamable HTTP (each
//! request answered as one JSON response; no server-initiated stream).
//!
//! This is how Meta AI Connectors (developer preview, waitlist) and any MCP client
//! reach the office: the same tools the glasses register with `WebMCP`. The hub token,
//! if set, is required as `Authorization: Bearer`; `X-Agora-Member` says which
//! person's bot acts.

use std::sync::Arc;

use axum::Json;
use axum::body::to_bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::{Hub, authorized, errands, unauthorized};

const PROTOCOL: &str = "2025-06-18";
/// Versions this server speaks; any other request gets [`PROTOCOL`] back, so the
/// client can decide whether to go on.
const SUPPORTED: [&str; 2] = ["2025-06-18", "2025-03-26"];

fn tools() -> Value {
    json!([
        {
            "name": "office_status",
            "description": "Who is in the Agora office, who leads which area, each person's bot, and what the bots did lately.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "run_errand",
            "description": "Give a bot an errand on the person's contacts, calendar, email, chats and files. Every call it makes goes into a hash-chained ledger, and every claim it then makes is checked against that ledger before delivery. Runs on the caller's bot, or on the bot of the lead of `area`. Returns an errand id; read the outcome with errand_result.",
            "inputSchema": { "type": "object", "properties": {
                "brief": { "type": "string", "description": "What to do." },
                "area": { "type": "string", "description": "Optional: an area (e.g. Finance) whose lead's bot should do it." }
            }, "required": ["brief"] },
            "annotations": { "readOnlyHint": false }
        },
        {
            "name": "errand_result",
            "description": "An errand's latest delivered message and its receipts: claims backed by the ledger, false claims delivered (the guard keeps this at zero), rewrites, and any correction written from the ledger. Without an id, the newest errand.",
            "inputSchema": { "type": "object", "properties": { "errand_id": { "type": "string" } } },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "ask_errand",
            "description": "Ask the bot about an errand (e.g. 'did you read my private chats?'). It answers from its ledger and the answer is checked the same way; read it with errand_result.",
            "inputSchema": { "type": "object", "properties": { "errand_id": { "type": "string" }, "question": { "type": "string" } }, "required": ["errand_id", "question"] },
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
            Ok(json!({ "people": people, "recent": recent, "errands": hub.errand_summaries().into_iter().take(5).collect::<Vec<_>>() }))
        }
        "run_errand" => {
            let brief = arg(args, "brief").ok_or("brief is required")?;
            let body = json!({ "brief": brief, "area": arg(args, "area") });
            let body = serde_json::from_value(body).map_err(|e| e.to_string())?;
            call_handler(errands::start(State(Arc::clone(hub)), headers.clone(), Ok(Json(body))).await).await
        }
        "errand_result" => {
            let found = match arg(args, "errand_id") {
                Some(id) => hub.errand(id).map(|r| r.summary()),
                None => hub.errand_summaries().into_iter().next(),
            };
            found.map(|e| json!(e)).ok_or_else(|| "no such errand".to_string())
        }
        "ask_errand" => {
            let id = arg(args, "errand_id").ok_or("errand_id is required")?.to_string();
            let question = arg(args, "question").ok_or("question is required")?;
            let body = serde_json::from_value(json!({ "question": question })).map_err(|e| e.to_string())?;
            call_handler(errands::ask(State(Arc::clone(hub)), axum::extract::Path(id), headers.clone(), Ok(Json(body))).await).await.map(|_| json!({ "ok": true, "next": "read the answer with errand_result" }))
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
    if !authorized(&hub, &headers, None) {
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
            let mut out = Vec::new();
            for request in &batch {
                out.extend(handle(&hub, &headers, request).await);
            }
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
