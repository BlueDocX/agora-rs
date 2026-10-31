//! A whole run against a scripted mock of `POST /v1/responses`.
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agora_engine::{CallError, Journal, Outcome, ReviewedBy, RunConfig, RunError, RunEvent, TaskError, run};
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
    /// Every message each worker got ("role: text"), by task title.
    worker_systems: HashMap<String, Vec<String>>,
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
                        let input = body["input"].as_array().unwrap();
                        let first = input.iter().find(|m| m["role"] == "user").unwrap()["content"].as_str().unwrap();
                        let title = after(first, "Your task: ").to_string();
                        let systems = input.iter().map(|m| format!("{}: {}", m["role"].as_str().unwrap(), m["content"].as_str().unwrap()));
                        seen.worker_systems.insert(title.clone(), systems.collect());
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
    RunConfig {
        model: "muse-spark-1.3".into(),
        max_parallel: 4,
        cap_usd,
        reviewer_model: None,
        baseten: None,
        events: None,
        team: None,
        attachments: Vec::new(),
        documents: String::new(),
    }
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
    let report = run(client(base), &config(1.0), "launch plan", None).await.unwrap();

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
    let report = run(client(base), &config(1.0), "brief", None).await.unwrap();

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
    let report = run(client(base), &config(1.0), "brief", None).await.unwrap();

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
    let report = run(client(base), &config(1.0), "brief", None).await.unwrap();

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
    let report = run(client(base), &config(0.001), "brief", None).await.unwrap();

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
    let report = run(client(base), &config(1.0), "brief", None).await.unwrap();

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
    let report = run(client(base), &config(1.0), "brief", None).await.unwrap();

    assert!(matches!(&report.outcomes[0], Outcome::Failed(TaskError::Call(CallError::Unfinished { status })) if status.starts_with("incomplete")));
    assert_eq!(seen.lock().unwrap().reviews, 0, "a cut-off draft is never reviewed");
    assert_eq!(report.spend.calls, 2, "the plan and the cut-off draft are both charged");
}

#[tokio::test]
async fn limits_are_checked_by_code_before_any_review() {
    let mut short = task("short", &[]);
    short["max_words"] = json!(1); // "draft 1 of short" is 4 words, over even with the slack
    let (base, seen) = mock(Script {
        plan: Arc::new(move |_| plan_of(&[short.clone()])),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "completed",
    })
    .await;
    let report = run(client(base), &config(1.0), "brief", None).await.unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.reviews, 0, "no model review for a draft that breaks a limit");
    assert!(seen.worker_prompts["short"][0].contains("- at most 1 words"));
    assert!(seen.worker_prompts["short"][1].contains("at most 1 words (it has 4; cut it to about 1)"));
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
    let report = run(client(base), &config(1.0), "brief", None).await.unwrap();

    let deliverable = report.deliverable();
    assert!(deliverable.contains("## research") && deliverable.contains("## write"), "{deliverable}");
    assert!(deliverable.ends_with("_Not included, needs a person: assemble._\n"), "{deliverable}");
}

fn journal_path(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("agora-run-{name}-{nanos}.db"))
}

#[tokio::test]
async fn a_resumed_run_pays_only_for_what_never_finished() {
    let (base, seen) = mock(Script {
        plan: Arc::new(|_| plan_of(&[task("a", &[]), task("b", &["a"])])),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "completed",
    })
    .await;
    let path = journal_path("resume");
    // $0.001675 a call: the plan, a's draft and a's review fit; b's draft does not.
    let journal = Journal::start(&path, "brief", "muse-spark-1.3", None, None).unwrap();
    let first = run(client(base.clone()), &config(0.004), "brief", Some(journal)).await.unwrap();
    assert!(first.outcomes[0].is_done());
    assert!(matches!(first.outcomes[1], Outcome::Failed(TaskError::Call(CallError::OverBudget(_)))));

    let (journal, record) = Journal::resume(&path, first.run_id.as_deref().unwrap()).unwrap();
    let resumed = run(client(base), &config(1.0), &record.brief, Some(journal)).await.unwrap();

    assert!(resumed.outcomes.iter().all(Outcome::is_done));
    assert_eq!(resumed.replayed, 3, "the plan, a's draft and a's review");
    assert_eq!(resumed.spend.calls, 2, "only b's draft and review are paid for");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.plan_requests.len(), 1);
    assert_eq!(seen.worker_prompts["a"].len(), 1);
    remove_db(&path);
}

#[tokio::test]
async fn a_finished_run_replays_without_touching_the_model() {
    let (base, _) = mock(Script {
        plan: Arc::new(|_| plan_of(&[task("research", &[]), task("write", &["research"])])),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "completed",
    })
    .await;
    let path = journal_path("replay");
    let journal = Journal::start(&path, "brief", "muse-spark-1.3", None, None).unwrap();
    let first = run(client(base), &config(1.0), "brief", Some(journal)).await.unwrap();

    // Nothing listens here: any real call would fail the run.
    let (journal, _) = Journal::resume(&path, first.run_id.as_deref().unwrap()).unwrap();
    let again = run(client("http://127.0.0.1:9/v1".into()), &config(1.0), "brief", Some(journal)).await.unwrap();

    assert_eq!(again.deliverable(), first.deliverable());
    assert_eq!((again.replayed, again.spend.calls), (first.spend.calls, 0));
    remove_db(&path);
}

fn remove_db(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
    }
}

#[tokio::test]
async fn resuming_does_not_reset_the_budget() {
    let (base, _) = mock(Script {
        plan: Arc::new(|_| plan_of(&[task("a", &[]), task("b", &["a"])])),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "completed",
    })
    .await;
    let path = journal_path("cap");
    let journal = Journal::start(&path, "brief", "muse-spark-1.3", None, None).unwrap();
    let first = run(client(base.clone()), &config(0.004), "brief", Some(journal)).await.unwrap();

    // Same cap again: the replayed $0.005 already spent it, so nothing new is paid.
    let (journal, _) = Journal::resume(&path, first.run_id.as_deref().unwrap()).unwrap();
    let resumed = run(client(base), &config(0.004), "brief", Some(journal)).await.unwrap();

    assert_eq!(resumed.spend.calls, 0);
    assert!(matches!(resumed.outcomes[1], Outcome::Failed(TaskError::Call(CallError::OverBudget(_)))));
    remove_db(&path);
}

#[tokio::test]
async fn each_call_goes_to_the_host_that_serves_its_model() {
    // The "Meta" mock refuses reviews; the "open weights" mock only accepts them.
    let (meta, meta_seen) = mock(Script {
        plan: Arc::new(|_| plan_of(&[task("a", &[])])),
        verdict: Arc::new(|_, _| panic!("reviews must not reach the Meta host")),
        worker_status: "completed",
    })
    .await;
    let (open, open_seen) = mock(Script {
        plan: Arc::new(|_| panic!("planning must not reach the reviewer host")),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "completed",
    })
    .await;
    // The reviewer's model is Baseten's (by the catalog); everything else is Meta's.
    let config = RunConfig {
        reviewer_model: Some("meta-models/muse-spark-open-1026".into()),
        baseten: Some(client(open)),
        ..config(1.0)
    };
    let report = run(client(meta), &config, "brief", None).await.unwrap();

    assert!(report.outcomes[0].is_done());
    assert_eq!(meta_seen.lock().unwrap().reviews, 0);
    assert_eq!(open_seen.lock().unwrap().reviews, 1);
    assert!(open_seen.lock().unwrap().worker_prompts.is_empty());
}

#[tokio::test]
async fn reports_each_step_as_it_happens() {
    let (base, _) = mock(Script {
        plan: Arc::new(|_| plan_of(&[task("a", &[]), task("b", &["a"])])),
        verdict: Arc::new(|title, draft| if title == "a" && draft.starts_with("draft 1") { fail("a is done") } else { pass() }),
        worker_status: "completed",
    })
    .await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let config = RunConfig { events: Some(tx), ..config(1.0) };
    run(client(base), &config, "brief", None).await.unwrap();
    drop(config);

    let mut kinds = Vec::new();
    while let Some(event) = rx.recv().await {
        kinds.push(match event {
            RunEvent::Staffed { team } => format!("staffed {}", team.id),
            RunEvent::Planned { tasks, .. } => format!("planned {}", tasks.len()),
            RunEvent::TaskStarted { task } => format!("start {task}"),
            RunEvent::Drafted { task, draft, .. } => format!("draft {task} {draft}"),
            RunEvent::Reviewed { task, unmet, by, .. } => {
                assert_eq!(by, ReviewedBy::Model);
                format!("review {task} unmet={}", unmet.len())
            }
            RunEvent::TaskAccepted { task, drafts, .. } => format!("accepted {task} after {drafts}"),
            RunEvent::TaskFailed { task, .. } => format!("failed {task}"),
            RunEvent::TaskSkipped { task } => format!("skipped {task}"),
            RunEvent::Finished { calls, .. } => format!("finished {calls}"),
            RunEvent::RunFailed { reason } => format!("run failed: {reason}"),
        });
    }
    assert_eq!(kinds, [
        "planned 2", "start a", "draft a 1", "review a unmet=1", "draft a 2", "review a unmet=0", "accepted a after 2",
        "start b", "draft b 1", "review b unmet=0", "accepted b after 1", "finished 7",
    ]);
}

#[tokio::test]
async fn a_skip_is_reported_when_its_input_fails_not_at_the_end() {
    let (base, _) = mock(Script {
        plan: Arc::new(|_| plan_of(&[task("a", &[]), task("b", &["a"]), task("c", &["b"])])),
        verdict: Arc::new(|title, _| if title == "a" { fail("a is done") } else { pass() }),
        worker_status: "completed",
    })
    .await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let config = RunConfig { events: Some(tx), ..config(1.0) };
    run(client(base), &config, "brief", None).await.unwrap();
    drop(config);

    let mut order = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            RunEvent::TaskFailed { task, .. } => order.push(format!("failed {task}")),
            RunEvent::TaskSkipped { task } => order.push(format!("skipped {task}")),
            RunEvent::Finished { .. } => order.push("finished".into()),
            _ => {}
        }
    }
    assert_eq!(order, ["failed a", "skipped b", "skipped c", "finished"]);
}

#[tokio::test]
async fn a_team_run_gives_each_task_to_a_member_who_works_in_its_role() {
    let team = agora_core::presets().into_iter().find(|t| t.id == "creative-studio").unwrap();
    let (base, seen) = mock(Script {
        plan: Arc::new(|_| {
            let mut tagline = task("tagline", &[]);
            tagline["agent"] = json!("copywriter");
            let mut look = task("look", &[]);
            look["agent"] = json!("art-director");
            plan_of(&[tagline, look])
        }),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "completed",
    })
    .await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let config = RunConfig { events: Some(tx), team: Some(team.clone()), ..config(1.0) };
    let report = run(client(base), &config, "brief", None).await.unwrap();
    drop(config);
    assert!(report.outcomes.iter().all(Outcome::is_done));

    // The team is announced first, then the plan says who does what.
    assert!(matches!(rx.recv().await, Some(RunEvent::Staffed { team: t }) if t == team));
    let Some(RunEvent::Planned { tasks, .. }) = rx.recv().await else { panic!("expected the plan") };
    let who: Vec<_> = tasks.iter().map(|t| t.agent.as_deref().unwrap()).collect();
    assert_eq!(who, ["copywriter", "art-director"]);

    let seen = seen.lock().unwrap();
    // The planner saw the roster and could only pick real members.
    let plan = &seen.plan_requests[0];
    assert!(plan["input"].to_string().contains("copywriter: Copywriter"));
    let schema = &plan["text"]["format"]["schema"]["properties"]["tasks"]["items"]["properties"]["agent"]["enum"];
    assert_eq!(schema.as_array().unwrap().len(), team.agents.len());
    // Each worker was told who it is, as the user (team text is caller-written, so it
    // never gets system authority).
    assert!(seen.worker_systems["tagline"].iter().any(|s| s.starts_with("user: You are Copywriter, on the Creative Studio team")));
    assert!(seen.worker_systems["look"].iter().any(|s| s.starts_with("user: You are Art Director")));
    assert!(!seen.worker_systems["tagline"].iter().any(|s| s.starts_with("system: You are Copywriter")));
    for m in plan["input"].as_array().unwrap() {
        assert!(m["role"] != "system" || !m["content"].as_str().unwrap().contains("copywriter"), "{m}");
    }
}

#[tokio::test]
async fn an_agent_works_on_its_own_model_and_a_missing_host_stops_the_run_first() {
    let mut team = agora_core::preset("creative-studio").unwrap();
    team.agents[1].model = Some("meta-models/muse-spark-open-1026".into()); // the copywriter, on Baseten
    let script = Script {
        plan: Arc::new(|_| {
            let mut tagline = task("tagline", &[]);
            tagline["agent"] = json!("copywriter");
            let mut look = task("look", &[]);
            look["agent"] = json!("art-director");
            plan_of(&[tagline, look])
        }),
        verdict: Arc::new(|_, _| pass()),
        worker_status: "completed",
    };
    let (meta, meta_seen) = mock(script.clone()).await;
    let (open, open_seen) = mock(script).await;

    // Without a Baseten client the run refuses up front, before any call.
    let no_host = RunConfig { team: Some(team.clone()), ..config(1.0) };
    let err = run(client(meta.clone()), &no_host, "brief", None).await.unwrap_err();
    assert!(matches!(err, RunError::NoHost(ref m) if m == "meta-models/muse-spark-open-1026"), "{err}");
    assert_eq!(meta_seen.lock().unwrap().plan_requests.len(), 0, "nothing was called");

    let config = RunConfig { team: Some(team), baseten: Some(client(open)), ..config(1.0) };
    let report = run(client(meta), &config, "brief", None).await.unwrap();
    assert!(report.outcomes.iter().all(Outcome::is_done));
    // The copywriter's task went to Baseten; the art director's stayed on Meta.
    assert_eq!(open_seen.lock().unwrap().worker_prompts.keys().collect::<Vec<_>>(), ["tagline"]);
    assert_eq!(meta_seen.lock().unwrap().worker_prompts.keys().collect::<Vec<_>>(), ["look"]);
}
