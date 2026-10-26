//! The client against a local mock of `POST /v1/responses`.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agora_muse::{Message, MuseClient, MuseError, Request};
use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

/// Serve `reply(call_number, headers, body)` on a random local port; return the base URL.
async fn mock<F>(reply: F) -> String
where
    F: Fn(usize, &HeaderMap, &Value) -> Response + Clone + Send + Sync + 'static,
{
    let calls = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let reply = reply.clone();
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move { reply(n, &headers, &body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/v1")
}

fn ok_body(text: &str) -> Value {
    json!({
        "id": "resp_1", "status": "completed", "model": "muse-spark-1.3",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}],
        "usage": {"input_tokens": 100, "input_tokens_details": {"cached_tokens": 60}, "output_tokens": 20}
    })
}

fn client(base: String) -> MuseClient {
    MuseClient::new(base, "test-key").with_first_backoff(Duration::from_millis(1))
}

#[tokio::test]
async fn sends_the_key_and_reads_text_usage_and_rate_limits() {
    let base = mock(|_, headers, body| {
        assert_eq!(headers["authorization"], "Bearer test-key");
        assert_eq!(body["model"], "muse-spark-1.3");
        let mut res = Json(ok_body("hello")).into_response();
        res.headers_mut().insert("x-ratelimit-remaining-requests", "2999".parse().unwrap());
        res
    })
    .await;
    let res = client(base).respond(&Request::new("muse-spark-1.3", vec![Message::user("hi")])).await.unwrap();
    assert_eq!(res.text, "hello");
    assert_eq!(res.usage.cached_tokens, 60);
    assert_eq!(res.rate_limit.remaining_requests, Some(2999));
}

#[tokio::test]
async fn retries_a_rate_limit_then_succeeds() {
    let base = mock(|n, _, _| {
        if n == 0 {
            (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "0")], "slow down").into_response()
        } else {
            Json(ok_body("second try")).into_response()
        }
    })
    .await;
    let res = client(base).respond(&Request::new("m", vec![Message::user("hi")])).await.unwrap();
    assert_eq!(res.text, "second try");
}

#[tokio::test]
async fn does_not_retry_a_bad_request_and_reports_the_api_message() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let base = mock(move |_, _, _| {
        seen.fetch_add(1, Ordering::SeqCst);
        let body = json!({"error": {"message": "only \"auto\" is supported for tool_choice"}});
        (StatusCode::BAD_REQUEST, Json(body)).into_response()
    })
    .await;
    let err = client(base).respond(&Request::new("m", vec![Message::user("hi")])).await.unwrap_err();
    assert!(
        matches!(&err, MuseError::Api { status: 400, message } if message.contains("tool_choice")),
        "{err:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn gives_up_after_four_server_errors() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let base = mock(move |_, _, _| {
        seen.fetch_add(1, Ordering::SeqCst);
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    })
    .await;
    let err = client(base).respond(&Request::new("m", vec![Message::user("hi")])).await.unwrap_err();
    assert!(matches!(err, MuseError::Api { status: 503, .. }), "{err:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn respond_json_parses_structured_output() {
    #[derive(serde::Deserialize)]
    struct Verdict {
        pass: bool,
    }
    let base = mock(|_, _, body| {
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        Json(ok_body(r#"{"pass": true}"#)).into_response()
    })
    .await;
    let req = Request::new("m", vec![Message::user("check")]).json_schema("Verdict", json!({"type": "object"}));
    let (verdict, _): (Verdict, _) = client(base).respond_json(&req).await.unwrap();
    assert!(verdict.pass);
}

#[test]
fn debug_output_never_shows_the_key() {
    let c = MuseClient::new("http://x", "sk-very-secret");
    assert!(!format!("{c:?}").contains("sk-very-secret"));
}
