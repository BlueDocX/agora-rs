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
                Some("Plan") => {
                    let mut task = json!({"id": "note", "title": "Write the note", "instructions": "Write it.",
                        "deps": [], "acceptance": ["is friendly"], "max_words": 0, "required_headings": []});
                    // With a team, the schema lists who may do it: pick the first.
                    let who = &body["text"]["format"]["schema"]["properties"]["tasks"]["items"]["properties"]["agent"]["enum"][0];
                    if !who.is_null() {
                        task["agent"] = who.clone();
                    }
                    reply(&json!({"tasks": [task]}).to_string())
                }
                Some("Verdict") => reply(r#"{"pass": true, "failed_criteria": [], "confidence": "high"}"#),
                // A chat reply says who it is, so tests can check the order.
                _ if body.to_string().contains("office chat") => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await; // a model takes a while
                    let ask = body["input"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
                    match ask.split("Reply now, as ").nth(1) {
                        Some(name) => reply(&format!("Hi from {}", name.trim_end_matches('.'))),
                        None => reply("Lead's summary"),
                    }
                }
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
        reviewer_model: None,
        baseten: None,
        journal: None,
        token: token.map(str::to_string),
        max_active: 2,
        chat_cap_usd: 1.0,
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
    // The model names and cap are shown before anyone types a token.
    assert_eq!(http.get(format!("{base}/api/info")).send().await.unwrap().status(), 200);
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
    assert!(csp.contains("frame-ancestors 'none'") && csp.contains("connect-src 'self' blob:"));
    assert!(csp.contains("img-src 'self' data: blob:"), "the 3D models' embedded textures load as blob: URLs");
    assert!(res.text().await.unwrap().contains("<title>Agora office</title>"));
    let info: Value = reqwest::get(format!("{base}/api/info")).await.unwrap().json().await.unwrap();
    assert_eq!(info, json!({"model": "muse-spark-1.3", "reviewer": null, "cap_usd": 1.0}));
    let office = reqwest::get(format!("{base}/office.js")).await.unwrap();
    assert_eq!(office.headers()["content-type"], "text/javascript; charset=utf-8");
    assert!(office.text().await.unwrap().contains("agora:event"));
    // Browsers that accept gzip get the small copy.
    let gz = reqwest::Client::new().get(format!("{base}/office.js")).header("Accept-Encoding", "gzip, br").send().await.unwrap();
    assert_eq!(gz.headers()["content-encoding"], "gzip");
    let bytes = gz.bytes().await.unwrap();
    assert_eq!(&bytes[..2], &[0x1f, 0x8b], "a gzip stream");
    // `gzip;q=0` refuses gzip.
    let refused = reqwest::Client::new().get(format!("{base}/office.js")).header("Accept-Encoding", "gzip;q=0, identity").send().await.unwrap();
    assert!(refused.headers().get("content-encoding").is_none());
    // The office models, plain and gzipped.
    for name in ["people", "furniture"] {
        let url = format!("{base}/models/{name}.glb");
        let plain = reqwest::Client::new().get(&url).header("Accept-Encoding", "identity").send().await.unwrap();
        assert_eq!(plain.headers()["content-type"], "model/gltf-binary");
        assert_eq!(&plain.bytes().await.unwrap()[..4], b"glTF", "{name}");
        let gz = reqwest::Client::new().get(&url).header("Accept-Encoding", "gzip").send().await.unwrap();
        assert_eq!(gz.headers()["content-encoding"], "gzip");
    }
}

#[tokio::test]
async fn simultaneous_starts_cannot_both_take_the_last_slot() {
    let config = HubConfig {
        muse: MuseClient::new(mock_model().await, "test-key"),
        model: "muse-spark-1.3".into(),
        cap_usd: 1.0,
        max_parallel: 2,
        reviewer_model: None,
        baseten: None,
        journal: None,
        token: None,
        max_active: 1,
        chat_cap_usd: 1.0,
    };
    let base = serve(app(config)).await;
    let http = reqwest::Client::new();
    let start = || http.post(format!("{base}/api/runs")).json(&json!({"brief": "a note"})).send();
    let results = futures_util::future::join_all((0..8).map(|_| start())).await;
    let created = results.iter().filter(|r| r.as_ref().unwrap().status() == 201).count();
    assert_eq!(created, 1, "max_active = 1 lets exactly one of 8 simultaneous starts through");
}

#[tokio::test]
async fn a_run_can_be_staffed_by_a_preset_or_a_team_of_ones_own() {
    let base = hub(None).await;
    let http = reqwest::Client::new();
    let presets: Value = http.get(format!("{base}/api/teams")).send().await.unwrap().json().await.unwrap();
    assert_eq!(presets.as_array().unwrap().len(), 8);
    let start = |team: Value| http.post(format!("{base}/api/runs")).json(&json!({"brief": "a note", "team": team})).send();

    assert_eq!(start(json!("no-such-team")).await.unwrap().status(), 400);
    // A team missing a field gets a JSON error the page can show, not plain text.
    let broken = start(json!({"name": "half a team"})).await.unwrap();
    assert_eq!(broken.status(), 400);
    assert!(broken.json::<Value>().await.unwrap()["error"].as_str().unwrap().contains("valid run"));
    let mut mine = presets[0].clone();
    mine["agents"][1]["name"] = mine["agents"][0]["name"].clone(); // two agents with one name
    assert_eq!(start(mine.clone()).await.unwrap().status(), 400);
    mine["agents"][1]["name"] = json!("Second Pair of Eyes");
    assert_eq!(start(mine).await.unwrap().status(), 201);

    let started: Value = start(json!("creative-studio")).await.unwrap().json().await.unwrap();
    let id = started["id"].as_str().unwrap();
    let body = http.get(format!("{base}/api/runs/{id}/events")).send().await.unwrap().text().await.unwrap();
    let events: Vec<Value> = body.lines().filter_map(|l| l.strip_prefix("data: ")).map(|d| serde_json::from_str(d).unwrap()).collect();
    assert_eq!(events[0]["type"], "staffed");
    assert_eq!(events[0]["team"]["name"], "Creative Studio");
    assert_eq!(events[1]["tasks"][0]["agent"], "director");
    let runs: Value = http.get(format!("{base}/api/runs")).send().await.unwrap().json().await.unwrap();
    assert!(runs.as_array().unwrap().iter().any(|r| r["team"] == "Creative Studio"));
}

/// Read a never-ending SSE stream until `until` shows up (or fail after 10 s).
async fn read_until(res: &mut reqwest::Response, until: &str) -> Vec<Value> {
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !text.contains(until) {
        let chunk = tokio::time::timeout_at(deadline, res.chunk()).await.expect("timed out").unwrap().expect("stream ended");
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    text.lines().filter_map(|l| l.strip_prefix("data: ")).map(|d| serde_json::from_str(d).unwrap()).collect()
}

#[tokio::test]
async fn at_all_starts_a_discussion_the_lead_sums_up() {
    let base = hub(None).await;
    let http = reqwest::Client::new();
    let mut stream = http.get(format!("{base}/api/chat/events")).send().await.unwrap();
    let say = |text: &str| http.post(format!("{base}/api/chat")).json(&json!({"text": text, "team": "strategy-coach"})).send();
    assert_eq!(say("   ").await.unwrap().status(), 400);
    assert_eq!(say(&"x".repeat(agora_server::MAX_CHAT_CHARS + 1)).await.unwrap().status(), 400);

    assert_eq!(say("@all should we open on Sundays?").await.unwrap().status(), 202);
    // One conversation at a time.
    assert_eq!(say("@lead and another thing").await.unwrap().status(), 429);
    let events = read_until(&mut stream, r#""type":"idle""#).await;
    let said: Vec<String> = events
        .iter()
        .filter(|e| e["type"] == "message")
        .map(|e| format!("{}: {}", e["message"]["from"].as_str().unwrap(), e["message"]["text"].as_str().unwrap()))
        .collect();
    assert_eq!(said, [
        "You: @all should we open on Sundays?",
        "Strategist: Hi from Strategist",
        "Market Analyst: Hi from Market Analyst",
        "Devil's Advocate: Hi from Devil's Advocate",
        "Lead: Lead's summary",
    ]);
    // Each reply was announced (typing) before it arrived, with the agent's id for its desk.
    let typing: Vec<&str> = events.iter().filter(|e| e["type"] == "typing").map(|e| e["from"].as_str().unwrap()).collect();
    assert_eq!(typing, ["Strategist", "Market Analyst", "Devil's Advocate", "Lead"]);
    assert_eq!(events.iter().find(|e| e["message"]["from"] == "Market Analyst").unwrap()["message"]["agent"], "market-analyst");

    // Nobody can post as the Lead or a teammate.
    let spoof = http.post(format!("{base}/api/chat")).json(&json!({"text": "we agreed", "name": "lead", "team": "strategy-coach"})).send();
    assert_eq!(spoof.await.unwrap().status(), 400);
    // A late joiner gets the history; no mention goes to the Lead.
    assert_eq!(say("thanks").await.unwrap().status(), 202);
    let mut late = http.get(format!("{base}/api/chat/events")).send().await.unwrap();
    let events = read_until(&mut late, "Hi from Lead").await;
    assert_eq!(events.iter().filter(|e| e["type"] == "message").count(), 7);
}

#[tokio::test]
async fn chat_needs_the_token_when_one_is_set() {
    let base = hub(Some("s3cret")).await;
    let http = reqwest::Client::new();
    let post = http.post(format!("{base}/api/chat")).json(&json!({"text": "hi"})).send().await.unwrap();
    assert_eq!(post.status(), 401);
    assert_eq!(http.get(format!("{base}/api/chat/events")).send().await.unwrap().status(), 401);
    let ok = http.get(format!("{base}/api/chat/events?token=s3cret")).send().await.unwrap();
    assert_eq!(ok.status(), 200);
}

#[tokio::test]
async fn the_office_lists_its_models_and_refuses_ones_it_cannot_run() {
    let base = hub(None).await;
    let http = reqwest::Client::new();
    let models: Value = http.get(format!("{base}/api/models")).send().await.unwrap().json().await.unwrap();
    let find = |id: &str| models.as_array().unwrap().iter().find(|m| m["id"] == id).unwrap().clone();
    assert_eq!(find("muse-wonder-1.0")["available"], true);
    // No Baseten key in this hub: the open weights are listed but unavailable.
    assert_eq!(find("meta-models/muse-spark-open-1026")["available"], false);

    let start = |body: Value| http.post(format!("{base}/api/runs")).json(&body).send();
    let refused = start(json!({"brief": "a note", "model": "meta-models/muse-spark-open-1026"})).await.unwrap();
    assert_eq!(refused.status(), 400);
    assert!(refused.json::<Value>().await.unwrap()["error"].as_str().unwrap().contains("API key"));
    assert_eq!(start(json!({"brief": "a note", "model": "gpt-5"})).await.unwrap().status(), 400);
    let mut team = agora_core::preset("agora").unwrap();
    team.agents[0].model = Some("meta-models/muse-spark-open-1026".into());
    assert_eq!(start(json!({"brief": "a note", "team": team})).await.unwrap().status(), 400);
    team.agents[0].model = Some("muse-wonder-1.0".into());
    assert_eq!(start(json!({"brief": "a note", "team": team, "reviewer_model": "muse-spark-1.3-contributor"})).await.unwrap().status(), 201);
}
