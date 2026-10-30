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
                // An errand: text Priya, then say so.
                Some("tool_call") => {
                    let turns = body["input"].as_array().unwrap().len();
                    if turns <= 3 {
                        reply(r#"{"tool": "messages.send", "target": "Priya", "when": "", "text": "Lunch on Thursday?"}"#)
                    } else {
                        reply(r#"{"tool": "finish", "target": "", "when": "", "text": "I texted Priya about lunch."}"#)
                    }
                }
                Some("claims") => {
                    let said = body["input"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
                    let claims = if said.contains("I texted Priya") {
                        json!([{"span": "I texted Priya", "tool": "messages.send", "target": "Priya", "polarity": "did", "outcome": "succeeded"}])
                    } else {
                        json!([])
                    };
                    reply(&json!({"claims": claims}).to_string())
                }
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
    // Muse Voice Transcribe: echo the keywords it was given, to show they went along.
    let router = router.route(
        "/v1/asr/transcribe",
        post(|mut form: axum::extract::Multipart| async move {
            let mut keywords = String::new();
            while let Some(field) = form.next_field().await.unwrap() {
                if field.name() == Some("request") {
                    let request: Value = serde_json::from_str(&field.text().await.unwrap()).unwrap();
                    keywords = request["keywords"].as_array().map(|k| k.len().to_string()).unwrap_or_default();
                }
            }
            Json(json!({ "sessionId": "s", "transcript": format!(" Text Amma I'll call Sunday ({keywords} keywords) "), "audioDurationMs": 1000, "turns": [] }))
        }),
    );
    format!("{}/v1", serve(router).await)
}

async fn config() -> HubConfig {
    HubConfig { cap_usd: 1.0, max_parallel: 2, chat_cap_usd: 1.0, ..HubConfig::new(MuseClient::new(mock_model().await, "test-key")) }
}

async fn hub(token: Option<&str>) -> String {
    serve(app(HubConfig { token: token.map(str::to_string), ..config().await })).await
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

    // Reading needs the token too: a header for the list, the session cookie for the
    // stream (never the URL, where it would end up in logs).
    let runs = |auth: &str| http.get(format!("{base}/api/runs")).header("Authorization", auth.to_string()).send();
    assert_eq!(runs("Bearer nope").await.unwrap().status(), 401);
    let listed: Value = runs("Bearer s3cret").await.unwrap().json().await.unwrap();
    let id = listed[0]["id"].as_str().unwrap();
    let stream = |cookie: &str| http.get(format!("{base}/api/runs/{id}/events")).header("Cookie", cookie.to_string()).send();
    assert_eq!(stream("").await.unwrap().status(), 401);
    assert_eq!(http.get(format!("{base}/api/runs/{id}/events?token=s3cret")).send().await.unwrap().status(), 401);
    assert_eq!(http.post(format!("{base}/api/session")).json(&json!({"token": "wrong"})).send().await.unwrap().status(), 401);
    let session = http.post(format!("{base}/api/session")).json(&json!({"token": "s3cret"})).send().await.unwrap();
    let cookie = session.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"), "{cookie}");
    let cookie = cookie.split(';').next().unwrap().to_string();
    assert_eq!(stream(&cookie).await.unwrap().status(), 200);
    let missing = http.get(format!("{base}/api/runs/run-none/events")).header("Cookie", cookie).send().await.unwrap();
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
    assert_eq!((&info["model"], &info["cap_usd"], &info["token_required"], &info["persistent"]), (&json!("muse-spark-1.3"), &json!(1.0), &json!(false), &json!(false)));
    assert_eq!(info["signing_key"].as_str().unwrap().len(), 64);
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
    let base = serve(app(HubConfig { max_active: 1, ..config().await })).await;
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
    let ok = http.get(format!("{base}/api/chat/events")).header("Cookie", "agora_token=s3cret").send().await.unwrap();
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

fn temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agora-hub-{}", agora_engine::random_hex(6)));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn join(http: &reqwest::Client, base: &str, body: Value) -> (u16, Value) {
    let res = http.post(format!("{base}/api/company/join")).json(&body).send().await.unwrap();
    (res.status().as_u16(), res.json().await.unwrap())
}

#[tokio::test]
async fn a_company_joins_by_invite_and_only_the_right_people_can_act_on_an_errand() {
    let dir = temp_dir();
    let config = || async { HubConfig { journal: Some(dir.join("journal.db")), state: Some(dir.join("state.db")), signing_key: Some(dir.join("key")), ..config().await } };
    let base = serve(app(config().await)).await;
    let http = reqwest::Client::new();
    let (status, asha) = join(&http, &base, json!({"name": "Asha"})).await;
    assert_eq!(status, 201);
    let asha_key = asha["key"].as_str().unwrap().to_string();
    // After the owner, joining needs the invite, which only the owner is shown.
    assert_eq!(join(&http, &base, json!({"name": "Priya", "area": "Finance"})).await.0, 403);
    let me: Value = http.get(format!("{base}/api/company/whoami")).header("X-Agora-Member", &asha_key).send().await.unwrap().json().await.unwrap();
    let invite = me["invite"].as_str().unwrap().to_string();
    assert!(me["call_room"].as_str().unwrap().len() >= 16);
    let (_, priya) = join(&http, &base, json!({"name": "Priya", "area": "Finance", "invite": invite})).await;
    let (_, dev) = join(&http, &base, json!({"name": "Dev", "invite": invite})).await;
    let priya_key = priya["key"].as_str().unwrap().to_string();
    let dev_key = dev["key"].as_str().unwrap().to_string();
    let theirs: Value = http.get(format!("{base}/api/company/whoami")).header("X-Agora-Member", &dev_key).send().await.unwrap().json().await.unwrap();
    assert!(theirs.get("invite").is_none(), "only the owner sees the invite");

    // Priya's errand runs on her bot, is checked, and its chain head is signed.
    let started = http.post(format!("{base}/api/errands")).header("X-Agora-Member", &priya_key).json(&json!({"brief": "Ask Priya to lunch"})).send().await.unwrap();
    assert_eq!(started.status(), 201);
    let id = started.json::<Value>().await.unwrap()["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("errand-") && id.len() == "errand-".len() + 16, "a random id: {id}");
    let mut stream = http.get(format!("{base}/api/errands/{id}/events")).send().await.unwrap();
    let events = read_until(&mut stream, r#""type":"idle""#).await;
    let said = events.iter().find(|e| e["type"] == "said").unwrap();
    assert_eq!(said["message"], "I texted Priya about lunch.");
    assert_eq!(said["check"]["false_claims"], json!([]));
    assert_eq!(said["check"]["audit"]["checked"][0]["verdict"], "supported");
    let signed: agora_engine::SignedHead = serde_json::from_value(said["chain"]["signed"].clone()).unwrap();
    assert_eq!(signed.verify(), Ok(()));
    let receipt: agora_engine::SignedHead = http.get(format!("{base}/api/errands/{id}/receipt")).send().await.unwrap().json().await.unwrap();
    assert_eq!((receipt.head.as_str(), receipt.verify()), (signed.head.as_str(), Ok(())));

    // Dev neither asked nor owns the bot: he can't ask about it or forget from it.
    let ask = |key: &str| http.post(format!("{base}/api/errands/{id}/ask")).header("X-Agora-Member", key.to_string()).json(&json!({"question": "what did you do?"})).send();
    assert_eq!(ask(&dev_key).await.unwrap().status(), 403);
    let forget = |key: &str| http.post(format!("{base}/api/errands/{id}/forget")).header("X-Agora-Member", key.to_string()).json(&json!({"seq": 1})).send();
    assert_eq!(forget(&dev_key).await.unwrap().status(), 403);
    let receipt: Value = forget(&priya_key).await.unwrap().json().await.unwrap();
    assert_eq!(receipt["type"], "forgotten");
    assert_eq!((&receipt["scan"]["found_in"], &receipt["scan"]["store_cleared"]), (&json!([]), &json!(true)));

    // A second office on the same files: the people and the errand are back.
    let again = serve(app(config().await)).await;
    let back: Value = http.get(format!("{again}/api/company/whoami")).header("X-Agora-Member", &priya_key).send().await.unwrap().json().await.unwrap();
    assert_eq!(back["member"]["name"], "Priya");
    let errands: Value = http.get(format!("{again}/api/errands")).send().await.unwrap().json().await.unwrap();
    assert_eq!(errands[0]["id"], json!(id));
    assert_eq!(errands[0]["failed"], Value::Null, "a finished errand comes back finished, not cut off");
    assert!(errands[0]["at_ms"].as_u64().is_some_and(|t| t > 0) && errands[0]["poisoned"] == false);
    assert_eq!(errands[0]["said"]["message"], "[Withdrawn: written after reading something you asked to forget.]");
    let ledger: Value = http.get(format!("{again}/api/errands/{id}/ledger")).send().await.unwrap().json().await.unwrap();
    assert_eq!((&ledger["chain"]["intact"], &ledger["entries"][0]["action"]["target"]), (&json!(true), &json!("[forgotten]")));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn people_talk_in_channels_and_privately_and_a_bot_answers_in_its_channel() {
    let base = hub(None).await;
    let http = reqwest::Client::new();
    let (_, asha) = join(&http, &base, json!({"name": "Asha"})).await;
    let asha_key = asha["key"].as_str().unwrap().to_string();
    let invite = http.get(format!("{base}/api/company/whoami")).header("X-Agora-Member", &asha_key).send().await.unwrap().json::<Value>().await.unwrap()["invite"].as_str().unwrap().to_string();
    let (_, priya) = join(&http, &base, json!({"name": "Priya", "area": "Finance", "invite": invite})).await;
    let (_, dev) = join(&http, &base, json!({"name": "Dev", "invite": invite})).await;
    let key = |m: &Value| m["key"].as_str().unwrap().to_string();
    let say = |k: String, to: &str, text: &str| http.post(format!("{base}/api/company/messages")).header("X-Agora-Member", k).json(&json!({"to": to, "text": text})).send();

    let mut devs = http.get(format!("{base}/api/company/events")).header("Cookie", format!("agora_member={}", key(&dev))).send().await.unwrap();
    assert_eq!(say(key(&asha), "@m2", "salary numbers attached").await.unwrap().status(), 201);
    assert_eq!(say(key(&asha), "#finance", "Q3 close is Friday").await.unwrap().status(), 201);
    assert_eq!(say(key(&asha), "#nowhere", "hello?").await.unwrap().status(), 400);
    let seen = read_until(&mut devs, "Q3 close").await;
    assert!(!seen.iter().any(|e| e.to_string().contains("salary")), "a direct message reaches only the two people in it");

    // "@Atlas ..." in a channel: Priya's bot does it and reports there, checked.
    assert_eq!(say(key(&dev), "#general", "@Atlas ask Priya to lunch").await.unwrap().status(), 201);
    let seen = read_until(&mut devs, "backed by the ledger").await;
    let bot: Vec<&Value> = seen.iter().filter(|e| e["type"] == "message" && e["message"]["bot"] == true).collect();
    assert_eq!(bot[0]["message"]["name"], "Atlas");
    assert!(bot[0]["message"]["text"].as_str().unwrap().starts_with("On it, Dev"));
    let report = bot.last().unwrap()["message"]["text"].as_str().unwrap();
    assert!(report.contains("I texted Priya about lunch.") && report.contains("1 claim backed by the ledger, 0 false delivered"), "{report}");
    assert_eq!(bot.last().unwrap()["message"]["to"], "#general");
    // Forgetting what that errand read withdraws the report from the channel too.
    let errand = bot.last().unwrap()["message"]["errand"].as_str().unwrap().to_string();
    let forgot: Value = http.post(format!("{base}/api/errands/{errand}/forget")).header("X-Agora-Member", key(&dev)).json(&json!({"seq": 1})).send().await.unwrap().json().await.unwrap();
    assert_eq!(forgot["scan"]["found_in"], json!([]), "{forgot}");
    let seen = read_until(&mut devs, r#""type":"withdrawn""#).await;
    let withdrawn = seen.iter().find(|e| e["type"] == "withdrawn").unwrap();
    assert!(withdrawn["text"].as_str().unwrap().starts_with("[Withdrawn"));
    // Removing someone retires the invite they came in with.
    let old_invite = invite.clone();
    let removed = http.delete(format!("{base}/api/company/members/{}", priya["member"]["id"].as_str().unwrap())).header("X-Agora-Member", key(&asha)).send().await.unwrap();
    assert_eq!(removed.status(), 204);
    assert_eq!(join(&http, &base, json!({"name": "Priya again", "invite": old_invite})).await.0, 403);
}

#[tokio::test]
async fn a_request_for_another_host_name_is_refused() {
    let base = hub(None).await;
    let http = reqwest::Client::new();
    let rebound = http.get(format!("{base}/api/company/whoami")).header("Host", "attacker.example").send().await.unwrap();
    assert_eq!(rebound.status(), 421, "a DNS-rebinding page names its own host");
    assert_eq!(http.get(format!("{base}/healthz")).send().await.unwrap().text().await.unwrap(), "ok");
    let named = serve(app(HubConfig { hosts: vec!["agora.example".into()], ..config().await })).await;
    let ok = http.get(format!("{named}/healthz")).header("Host", "agora.example:443").send().await.unwrap();
    assert_eq!(ok.status(), 200);
}

#[tokio::test]
async fn mcp_clients_list_the_tools_and_call_them() {
    let base = hub(None).await;
    let http = reqwest::Client::new();
    let rpc = |body: Value| http.post(format!("{base}/mcp")).json(&body).send();
    let init: Value = rpc(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})).await.unwrap().json().await.unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    let tools: Value = rpc(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await.unwrap().json().await.unwrap();
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["office_status", "run_errand", "errand_result", "ask_errand", "verify_ledger"]);
    let run: Value = rpc(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "run_errand", "arguments": {"brief": "Ask Priya to lunch"}}})).await.unwrap().json().await.unwrap();
    let id = run["result"]["structuredContent"]["id"].as_str().unwrap().to_string();
    let mut stream = http.get(format!("{base}/api/errands/{id}/events")).send().await.unwrap();
    read_until(&mut stream, r#""type":"idle""#).await;
    let result: Value = rpc(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "errand_result", "arguments": {"errand_id": id}}})).await.unwrap().json().await.unwrap();
    assert_eq!(result["result"]["structuredContent"]["said"]["false_delivered"], 0);
    let verified: Value = rpc(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "verify_ledger", "arguments": {"errand_id": id}}})).await.unwrap().json().await.unwrap();
    assert_eq!(verified["result"]["structuredContent"]["intact"], true);
    // A notification gets no reply; a form post from another page is refused.
    assert_eq!(rpc(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await.unwrap().status(), 202);
    let form = http.post(format!("{base}/mcp")).header("Content-Type", "text/plain").body("{}").send().await.unwrap();
    assert_eq!(form.status(), 403);
}

#[tokio::test]
async fn a_spoken_errand_is_transcribed_on_the_server() {
    let base = hub(None).await;
    let http = reqwest::Client::new();
    let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
    for (n, size) in [(16u32, 4), (1, 2), (1, 2), (24_000, 4), (48_000, 4), (2, 2), (16, 2)] {
        wav.extend_from_slice(&n.to_le_bytes()[..size]);
    }
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&48_000u32.to_le_bytes());
    wav.resize(wav.len() + 48_000, 0);
    let res: Value = http.post(format!("{base}/api/transcribe")).body(wav).send().await.unwrap().json().await.unwrap();
    assert_eq!(res["text"], "Text Amma I'll call Sunday (9 keywords)", "{res}");
    let bad = http.post(format!("{base}/api/transcribe")).body("not audio").send().await.unwrap();
    assert_eq!(bad.status(), 400);
}
