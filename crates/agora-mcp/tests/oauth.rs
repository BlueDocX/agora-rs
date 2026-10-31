//! Signing in to an MCP server over OAuth 2.1, against a mock provider: discovery,
//! dynamic registration, the code exchange with PKCE, a call with the token, and a
//! refresh once the token has expired.
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use agora_mcp::{Server, list_tools, oauth};
use axum::Json;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

#[derive(Default)]
struct Provider {
    base: String,
    /// Access tokens handed out so far.
    issued: Vec<String>,
    /// Lifetime of the next token, in seconds.
    expires_in: u64,
}

type Shared = Arc<Mutex<Provider>>;

async fn resource(State(p): State<Shared>) -> Json<Value> {
    let base = p.lock().unwrap().base.clone();
    Json(json!({ "resource": format!("{base}/mcp"), "authorization_servers": [base] }))
}

async fn metadata(State(p): State<Shared>) -> Json<Value> {
    let base = p.lock().unwrap().base.clone();
    Json(json!({
        "issuer": base, "authorization_endpoint": format!("{base}/authorize"), "token_endpoint": format!("{base}/token"),
        "registration_endpoint": format!("{base}/register"), "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"], "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"]
    }))
}

async fn register(Json(req): Json<Value>) -> (StatusCode, Json<Value>) {
    (StatusCode::CREATED, Json(json!({ "client_id": "agora-client", "redirect_uris": req["redirect_uris"], "token_endpoint_auth_method": "none" })))
}

async fn token(State(p): State<Shared>, Form(form): Form<Vec<(String, String)>>) -> Json<Value> {
    let get = |k: &str| form.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let mut p = p.lock().unwrap();
    let ok = match get("grant_type").as_str() {
        "authorization_code" => get("code") == "code-1" && !get("code_verifier").is_empty(),
        "refresh_token" => get("refresh_token") == "rt-1",
        _ => false,
    };
    assert!(ok, "a bad token request: {form:?}");
    let access = format!("at-{}", p.issued.len() + 1);
    p.issued.push(access.clone());
    Json(json!({ "access_token": access, "token_type": "Bearer", "expires_in": p.expires_in, "refresh_token": "rt-1" }))
}

async fn mcp(State(p): State<Shared>, headers: HeaderMap, Json(req): Json<Value>) -> Response {
    let given = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().trim_start_matches("Bearer ").to_string();
    if !p.lock().unwrap().issued.contains(&given) {
        let base = p.lock().unwrap().base.clone();
        let mut res = StatusCode::UNAUTHORIZED.into_response();
        res.headers_mut().insert("www-authenticate", format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\"").parse().unwrap());
        return res;
    }
    let id = req["id"].clone();
    let ok = |result: Value| Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response();
    match req["method"].as_str().unwrap_or_default() {
        "initialize" => {
            let mut res = ok(json!({ "protocolVersion": "2025-11-25", "capabilities": { "tools": {} }, "serverInfo": { "name": "p", "version": "1" } }));
            res.headers_mut().insert("mcp-session-id", "s".parse().unwrap());
            res
        }
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" => ok(json!({ "tools": [{ "name": "post_message", "description": "Post to a channel", "inputSchema": { "type": "object" } }] })),
        _ => (StatusCode::BAD_REQUEST, Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } }))).into_response(),
    }
}

async fn provider(expires_in: u64) -> (String, Shared) {
    let shared: Shared = Arc::new(Mutex::new(Provider { expires_in, ..Provider::default() }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    shared.lock().unwrap().base.clone_from(&base);
    let app = axum::Router::new()
        .route("/.well-known/oauth-protected-resource", get(resource))
        .route("/.well-known/oauth-protected-resource/mcp", get(resource))
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/register", post(register))
        .route("/token", post(token))
        .route("/mcp", post(mcp))
        .with_state(Arc::clone(&shared));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, shared)
}

#[tokio::test]
async fn sign_in_call_and_refresh() {
    let (base, shared) = provider(3600).await;
    let url = format!("{base}/mcp");
    let pending = oauth::begin(&url, "http://127.0.0.1:8080/api/connectors/oauth/callback", None, true).await.unwrap();
    assert!(pending.authorize_url.starts_with(&format!("{base}/authorize?")), "{}", pending.authorize_url);
    assert!(pending.authorize_url.contains("client_id=agora-client") && pending.authorize_url.contains("code_challenge="));
    let creds = oauth::finish(pending, "code-1").await.unwrap();
    let (access, renewed) = oauth::token(&url, &creds, true).await.unwrap();
    assert_eq!((access.as_str(), renewed.is_none()), ("at-1", true));
    let tools = list_tools(&Server { url: url.clone(), token: Some(access), allow_local: true }).await.unwrap();
    assert_eq!(tools[0].name, "post_message");

    // A token about to expire is refreshed, and the new credentials come back to keep.
    let (base, _) = provider(1).await;
    let url = format!("{base}/mcp");
    let pending = oauth::begin(&url, "http://127.0.0.1:8080/cb", None, true).await.unwrap();
    let creds = oauth::finish(pending, "code-1").await.unwrap();
    let (access, renewed) = oauth::token(&url, &creds, true).await.unwrap();
    assert_eq!(access, "at-2", "refreshed");
    assert!(renewed.unwrap().contains("at-2"));
    drop(shared);
}
