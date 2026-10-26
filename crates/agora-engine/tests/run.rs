//! A whole run against a scripted mock of `POST /v1/responses`.
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agora_engine::{CallError, Outcome, RunConfig, TaskError, run};
use agora_muse::MuseClient;
use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};

/// What the mock model does. Each closure gets the request body; drafts are numbered
/// per task (1 = first draft).
type VerdictFn = dyn Fn(&str, &str) -> Value + Send + Sync;

#[derive(Clone)]
struct Script {
    /// The planner's answer for each attempt (1-based).
    plan: Arc<dyn Fn(usize) -> Value + Send + Sync>,
    /// The reviewer's verdict for (task title, draft text).
    verdict: Arc<VerdictFn>,
    /// The `status` of every worker reply ("completed" unless a test says otherwise).
    worker_status: &'static str,
}

#[derive(Default)]
struct Seen {
    plan_requests: Vec<Value>,
    worker_prompts: HashMap<String, Vec<String>>,
    reviews: usize,
}

fn reply(text: &str) -> Json<Value> {
    reply_with_status(text, "completed")
}

fn reply_with_status(text: &str, status: &str) -> Json<Value> {
    Json(json!({
        "id": "r", "status": status, "model": "muse-spark-1.3",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}],
        "usage": {"input_tokens": 1000, "output_tokens": 100}
    }))
}

fn last_user(body: &Value) -> String {
    body["input"].as_array().unwrap().iter().rev().find(|m| m["role"] == "user").unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The text after `prefix` up to the end of that line.
fn after<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.split(prefix).nth(1).unwrap().lines().next().unwrap()
}

async fn mock(script: Script) -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let state = Arc::clone(&seen);
    let app = axum::Router::new().route(
        "/v1/responses",
        post(move |Json(body): Json<Value>| {
            let (script, seen) = (script.clone(), Arc::clone(&state));
            async move {
                let mut seen = seen.lock().unwrap();
                match body["text"]["format"]["name"].as_str() {
                    Some("Plan") => {
                        seen.plan_requests.push(body.clone());
                        reply(&(script.plan)(seen.plan_requests.len()).to_string())
                    }
                    Some("Verdict") => {
                        seen.reviews += 1;
                        let prompt = last_user(&body);
                        let draft = prompt.split("Result:\n").nth(1).unwrap();
                        reply(&(script.verdict)(after(&prompt, "Task: "), draft).to_string())
                    }
                    _ => {
                        let first = body["input"][2]["content"].as_str().unwrap();
                        let title = after(first, "Your task: ").to_string();
                        let drafts = seen.worker_prompts.entry(title.clone()).or_default();
                        drafts.push(last_user(&body));
                        reply_with_status(&format!("draft {} of {title}", drafts.len()), script.worker_status)
                    }
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1"), seen)
}

fn task(id: &str, deps: &[&str]) -> Value {
    json!({"id": id, "title": id, "instructions": format!("do {id}"), "deps": deps, "acceptance": [format!("{id} is done")],
           "max_words": 0, "required_headings": []})
}

fn plan_of(tasks: &[Value]) -> Value {
    json!({"tasks": tasks})
}

fn pass() -> Value {
    json!({"pass": true, "failed_criteria": [], "confidence": "high"})
}

fn fail(criterion: &str) -> Value {
    json!({"pass": false, "failed_criteria": [criterion], "confidence": "high"})
}

fn config(cap_usd: f64) -> RunConfig {
    RunConfig { model: "muse-spark-1.3".into(), max_parallel: 4, cap_usd }
}

fn client(base: String) -> MuseClient {
    MuseClient::new(base, "test-key")
}

#[tokio::test]
async fn runs_a_plan_and_feeds_each_task_its_inputs() {
    let (base, seen) = mock(Script {
        plan: Arc::new(|_| plan_of(&[task("research", &[]), task("write", &["research"]), task("design", &[])])),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "completed",
    })
    .await;
    let report = run(client(base), &config(1.0), "launch plan").await.unwrap();

    assert!(report.outcomes.iter().all(Outcome::is_done));
    let seen = seen.lock().unwrap();
    assert!(seen.worker_prompts["write"][0].contains("Input from finished task \"research\":\ndraft 1 of research"));
    assert!(!seen.worker_prompts["design"][0].contains("Input from finished task"));
    // "research" fed "write", so only the final results appear.
    let deliverable = report.deliverable();
    assert!(!deliverable.contains("## research"), "{deliverable}");
    assert!(deliverable.contains("## write\n\ndraft 1 of write") && deliverable.contains("## design"), "{deliverable}");
    assert_eq!(report.spend.calls, 1 + 3 + 3);
}

#[tokio::test]
async fn sends_an_invalid_plan_back_with_the_exact_error() {
    let (base, seen) = mock(Script {
        plan: Arc::new(|attempt| {
            if attempt == 1 {
                plan_of(&[task("a", &["b"]), task("b", &["a"])])
            } else {
                plan_of(&[task("a", &[]), task("b", &["a"])])
            }
        }),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "completed",
    })
    .await;
    let report = run(client(base), &config(1.0), "brief").await.unwrap();

    assert_eq!(report.planned.attempts, 2);
    let retry = last_user(&seen.lock().unwrap().plan_requests[1]);
    assert!(retry.contains("cycle"), "{retry}");
}

#[tokio::test]
async fn retries_with_the_failed_criteria_then_leaves_it_for_a_person() {
    let (base, seen) = mock(Script {
        plan: Arc::new(|_| plan_of(&[task("fixable", &[]), task("hopeless", &[]), task("after", &["hopeless"])])),
        verdict: Arc::new(|title, draft| match title {
            "fixable" if draft.starts_with("draft 1") => fail("fixable is done"),
            "hopeless" => fail("hopeless is done"),
            _ => pass(),
        }),
        worker_status: "completed",
    })
    .await;
    let report = run(client(base), &config(1.0), "brief").await.unwrap();

    let Outcome::Done(fixed) = &report.outcomes[0] else { panic!("{:?}", report.outcomes[0]) };
    assert_eq!(fixed.drafts, 2);
    assert!(seen.lock().unwrap().worker_prompts["fixable"][1].contains("does not meet these criteria yet:\n- fixable is done"));
    assert!(matches!(&report.outcomes[1], Outcome::Failed(TaskError::Rejected { failed_criteria }) if failed_criteria == &["hopeless is done"]));
    assert!(matches!(report.outcomes[2], Outcome::Skipped));
    // A partial run keeps its section titles and says what is missing.
    assert_eq!(
        report.deliverable(),
        "## fixable\n\ndraft 2 of fixable\n\n_Not included, needs a person: hopeless; after._\n"
    );
}

#[tokio::test]
async fn an_unsure_reviewer_triggers_a_majority_vote() {
    let votes = Arc::new(Mutex::new(0));
    let (base, seen) = mock(Script {
        plan: Arc::new(|_| plan_of(&[task("only", &[])])),
        verdict: Arc::new(move |_, _| {
            let mut n = votes.lock().unwrap();
            *n += 1;
            match *n {
                1 => json!({"pass": false, "failed_criteria": ["only is done"], "confidence": "low"}),
                _ => pass(),
            }
        }),
        worker_status: "completed",
    })
    .await;
    let report = run(client(base), &config(1.0), "brief").await.unwrap();

    let Outcome::Done(accepted) = &report.outcomes[0] else { panic!("{:?}", report.outcomes[0]) };
    assert_eq!((accepted.drafts, accepted.reviews), (1, 3));
    assert_eq!(seen.lock().unwrap().reviews, 3);
}

#[tokio::test]
async fn stops_calling_the_model_once_the_budget_is_spent() {
    let (base, _) = mock(Script {
        plan: Arc::new(|_| plan_of(&[task("a", &[]), task("b", &["a"])])),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "completed",
    })
    .await;
    // Each mocked call costs $0.001675; the plan alone spends this cap.
    let report = run(client(base), &config(0.001), "brief").await.unwrap();

    assert!(matches!(report.outcomes[0], Outcome::Failed(TaskError::Call(CallError::OverBudget(_)))));
    assert!(matches!(report.outcomes[1], Outcome::Skipped));
    assert_eq!(report.spend.calls, 1);
}

#[tokio::test]
async fn a_reply_that_is_not_plan_json_gets_another_attempt() {
    let (base, seen) = mock(Script {
        plan: Arc::new(|attempt| if attempt == 1 { json!("sorry, here is a plan in prose") } else { plan_of(&[task("a", &[])]) }),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "completed",
    })
    .await;
    let report = run(client(base), &config(1.0), "brief").await.unwrap();

    assert_eq!(report.planned.attempts, 2);
    assert!(last_user(&seen.lock().unwrap().plan_requests[1]).contains("not valid plan JSON"));
}

#[tokio::test]
async fn a_cut_off_draft_is_refused_but_still_charged() {
    let (base, seen) = mock(Script {
        plan: Arc::new(|_| plan_of(&[task("a", &[])])),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "incomplete",
    })
    .await;
    let report = run(client(base), &config(1.0), "brief").await.unwrap();

    assert!(matches!(&report.outcomes[0], Outcome::Failed(TaskError::Call(CallError::Unfinished { status })) if status == "incomplete"));
    assert_eq!(seen.lock().unwrap().reviews, 0, "a cut-off draft is never reviewed");
    assert_eq!(report.spend.calls, 2, "the plan and the cut-off draft are both charged");
}

#[tokio::test]
async fn limits_are_checked_by_code_before_any_review() {
    let mut short = task("short", &[]);
    short["max_words"] = json!(3); // "draft 1 of short" is 4 words
    let (base, seen) = mock(Script {
        plan: Arc::new(move |_| plan_of(&[short.clone()])),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "completed",
    })
    .await;
    let report = run(client(base), &config(1.0), "brief").await.unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.reviews, 0, "no model review for a draft that breaks a limit");
    assert!(seen.worker_prompts["short"][0].contains("- at most 3 words"));
    assert!(seen.worker_prompts["short"][1].contains("at most 3 words (it has 4; cut 1)"));
    // Code-check misses don't use up the review retry: three drafts, then a person.
    assert_eq!(seen.worker_prompts["short"].len(), 3);
    assert!(matches!(&report.outcomes[0], Outcome::Failed(TaskError::Rejected { .. })));
}

#[tokio::test]
async fn an_input_stays_in_the_deliverable_if_any_task_built_on_it_failed() {
    let (base, _) = mock(Script {
        plan: Arc::new(|_| plan_of(&[task("research", &[]), task("write", &["research"]), task("assemble", &["research"])])),
        verdict: Arc::new(|title, _| if title == "assemble" { fail("assemble is done") } else { pass() }),
        worker_status: "completed",
    })
    .await;
    let report = run(client(base), &config(1.0), "brief").await.unwrap();

    let deliverable = report.deliverable();
    assert!(deliverable.contains("## research") && deliverable.contains("## write"), "{deliverable}");
    assert!(deliverable.ends_with("_Not included, needs a person: assemble._\n"), "{deliverable}");
}
