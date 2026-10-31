//! The client against a real (local) MCP server speaking the older, handshake-based
//! revision, so the dual-era fallback is exercised too.
#![allow(clippy::unwrap_used)]

use agora_mcp::{Server, call, list_tools};
use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

async fn mcp(headers: HeaderMap, Json(req): Json<Value>) -> Response {
    let id = req["id"].clone();
    let ok = |result: Value| Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response();
    match req["method"].as_str().unwrap_or_default() {
        "initialize" => {
            let mut res = ok(json!({ "protocolVersion": "2025-11-25", "capabilities": { "tools": {} }, "serverInfo": { "name": "fake", "version": "1" } }));
            res.headers_mut().insert("mcp-session-id", "s-1".parse().unwrap());
            res
        }
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" => ok(json!({ "tools": [
            { "name": "create_issue", "description": "Open an issue", "inputSchema": { "type": "object", "properties": { "repo": { "type": "string" } }, "required": ["repo"] }, "annotations": { "readOnlyHint": false, "destructiveHint": false } },
            { "name": "list_issues", "description": "List issues", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } }
        ] })),
        "tools/call" => {
            let authed = headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer t0ken");
            let repo = req["params"]["arguments"]["repo"].as_str().unwrap_or_default().to_string();
            ok(json!({ "content": [{ "type": "text", "text": format!("Created issue #12 in {repo} (authed: {authed})") }], "isError": repo == "bad/repo" }))
        }
        // Anything newer (server/discover) is unknown here: the client must fall back.
        _ => (StatusCode::BAD_REQUEST, Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } }))).into_response(),
    }
}

async fn serve() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/mcp", post(mcp))).await.unwrap() });
    format!("http://{addr}/mcp")
}

#[tokio::test]
async fn lists_and_calls_tools_on_an_older_server_with_a_token() {
    let server = Server { url: serve().await, token: Some("t0ken".into()), allow_local: true };
    let tools = list_tools(&server).await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["create_issue", "list_issues"]);
    assert_eq!((tools[0].read_only, tools[1].read_only), (Some(false), Some(true)));
    assert_eq!(tools[0].def.len(), 64);

    let reply = call(&server, "create_issue", &json!({ "repo": "octo/demo" })).await.unwrap();
    assert_eq!(reply.text, "Created issue #12 in octo/demo (authed: true)");
    assert!(reply.ok);
    // A tool that ran and reported an error is a reply, not a failure to reach it.
    assert!(!call(&server, "create_issue", &json!({ "repo": "bad/repo" })).await.unwrap().ok);
}

#[tokio::test]
async fn a_local_server_is_refused_unless_the_office_allows_it() {
    let server = Server { url: serve().await, token: None, allow_local: false };
    assert!(matches!(list_tools(&server).await, Err(agora_mcp::Error::Refused(_))));
}
