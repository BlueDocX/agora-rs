//! The hub over real HTTP, with a mock model host behind it.
#![allow(clippy::unwrap_used)]

use agora_muse::MuseClient;
use agora_server::{HubConfig, MAX_BRIEF_CHARS, app};
use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn reply(text: &str) -> Json<Value> {
    Json(json!({
        "id": "r", "status": "completed", "model": "muse-spark-1.3",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}],
        "usage": {"input_tokens": 100, "output_tokens": 10}
    }))
}

/// A model that plans one task, writes it, and passes every review.
async fn mock_model() -> String {
    let router = axum::Router::new().route(
        "/v1/responses",
        post(|Json(body): Json<Value>| async move {
            match body["text"]["format"]["name"].as_str() {
                Some("Plan") => reply(
                    &json!({"tasks": [{"id": "note", "title": "Write the note", "instructions": "Write it.",
                        "deps": [], "acceptance": ["is friendly"], "max_words": 0, "required_headings": []}]})
                    .to_string(),
                ),
                Some("Verdict") => reply(r#"{"pass": true, "failed_criteria": [], "confidence": "high"}"#),
                _ => reply("Hello, regulars!"),
            }
        }),
    );
    format!("{}/v1", serve(router).await)
}

async fn hub(token: Option<&str>) -> String {
    let config = HubConfig {
        muse: MuseClient::new(mock_model().await, "test-key"),
        model: "muse-spark-1.3".into(),
        cap_usd: 1.0,
        max_parallel: 2,
        reviewer: None,
        journal: None,
        token: token.map(str::to_string),
        max_active: 2,
    };
    serve(app(config)).await
}

#[tokio::test]
async fn starting_a_run_needs_the_token_and_a_sensible_brief() {
    let base = hub(Some("s3cret")).await;
    let http = reqwest::Client::new();
    let post = |auth: Option<&str>, brief: String| {
        let mut req = http.post(format!("{base}/api/runs")).json(&json!({"brief": brief}));
        if let Some(a) = auth {
            req = req.header("Authorization", a);
        }
        req.send()
    };
    assert_eq!(post(None, "hi".into()).await.unwrap().status(), 401);
    assert_eq!(post(Some("Bearer wrong!"), "hi".into()).await.unwrap().status(), 401);
    assert_eq!(post(Some("Bearer s3cret"), "   ".into()).await.unwrap().status(), 400);
    assert_eq!(post(Some("Bearer s3cret"), "x".repeat(MAX_BRIEF_CHARS + 1)).await.unwrap().status(), 400);
    assert_eq!(post(Some("Bearer s3cret"), "a note".into()).await.unwrap().status(), 201);

    // Reading needs the token too: a header for the list, `?token=` for the stream.
    let runs = |auth: &str| http.get(format!("{base}/api/runs")).header("Authorization", auth.to_string()).send();
    assert_eq!(runs("Bearer nope").await.unwrap().status(), 401);
    let listed: Value = runs("Bearer s3cret").await.unwrap().json().await.unwrap();
    let id = listed[0]["id"].as_str().unwrap();
    let stream = |q: &str| http.get(format!("{base}/api/runs/{id}/events{q}")).send();
    assert_eq!(stream("").await.unwrap().status(), 401);
    assert_eq!(stream("?token=s3cret").await.unwrap().status(), 200);
    let missing = http.get(format!("{base}/api/runs/run-none/events?token=s3cret")).send().await.unwrap();
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn a_run_streams_every_step_then_ends() {
    let base = hub(None).await;
    let http = reqwest::Client::new();
    let started: Value =
        http.post(format!("{base}/api/runs")).json(&json!({"brief": "a note"})).send().await.unwrap().json().await.unwrap();
    let id = started["id"].as_str().unwrap();

    // The stream ends when the run does; joining late still gets everything.
    let body = http.get(format!("{base}/api/runs/{id}/events")).send().await.unwrap().text().await.unwrap();
    let kinds: Vec<String> = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str::<Value>(d).unwrap()["type"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, ["planned", "task_started", "drafted", "reviewed", "task_accepted", "finished"]);
    assert!(body.contains("Hello, regulars!"));

    let runs: Value = http.get(format!("{base}/api/runs")).send().await.unwrap().json().await.unwrap();
    assert_eq!(runs[0]["done"], true);
    assert_eq!(runs[0]["brief"], "a note");
}

#[tokio::test]
async fn the_page_is_served_with_a_strict_content_policy() {
    let base = hub(None).await;
    let res = reqwest::get(format!("{base}/")).await.unwrap();
    let csp = res.headers()["content-security-policy"].to_str().unwrap().to_string();
    assert!(csp.contains("frame-ancestors 'none'") && csp.contains("connect-src 'self'"));
    assert!(res.text().await.unwrap().contains("<title>Agora office</title>"));
}

#[tokio::test]
async fn simultaneous_starts_cannot_both_take_the_last_slot() {
    let config = HubConfig {
        muse: MuseClient::new(mock_model().await, "test-key"),
        model: "muse-spark-1.3".into(),
        cap_usd: 1.0,
        max_parallel: 2,
        reviewer: None,
        journal: None,
        token: None,
        max_active: 1,
    };
    let base = serve(app(config)).await;
    let http = reqwest::Client::new();
    let start = || http.post(format!("{base}/api/runs")).json(&json!({"brief": "a note"})).send();
    let results = futures_util::future::join_all((0..8).map(|_| start())).await;
    let created = results.iter().filter(|r| r.as_ref().unwrap().status() == 201).count();
    assert_eq!(created, 1, "max_active = 1 lets exactly one of 8 simultaneous starts through");
}
