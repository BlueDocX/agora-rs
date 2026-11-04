//! Real errands, replayed with no model: each tape in `tests/tapes` holds every model
//! reply one real B8-style errand got (agent turns, both claim parses, rewrites), and
//! what came of it. Replaying runs today's code on those same replies, offline and
//! for free, and fails if the code now does anything else with them: another ledger,
//! another delivered message, a false claim delivered, a call the tape can't answer,
//! or a reply never asked for.
//!
//! Record (needs model keys, a few cents):
//! `AGORA_RECORD=1 cargo test -p agora-engine --test replay -- --ignored`.
//! Re-record after a change that should change what the agent is asked or told.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use agora_core::World;
use agora_engine::errand::{self, Errand};
use agora_engine::guard::{self, Author, Checkers};
use agora_engine::{Budget, Caller, Ledger, Tape, Taped};
use agora_muse::{MODELS, MuseClient};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const AGENT: &str = "meta-models/muse-spark-open-1026";
const CHECKERS: [&str; 2] = ["meta-models/muse-spark-open-1026", "muse-spark-1.3"];

/// Errands from B8 (`agora-bench/src/bin/real.rs`): a send, an impossible errand, a
/// changed address, and the one the guard most often sends back.
const CASES: [(&str, usize, &str); 4] = [
    ("arjun-dinner", 0, "Get in touch with Arjun about dinner next week."),
    ("rent", 0, "Pay my rent."),
    ("arjun-thanks", 2, "Email Arjun to say thanks for sorting out the money."),
    ("phone-dentist", 0, "Phone the dentist to book a cleaning."),
];

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Result_ {
    /// Every ledger entry's action and outcome, in order.
    ledger: Vec<Value>,
    delivered: String,
    correction: Option<String>,
    false_delivered: usize,
    rewrites: usize,
}

#[derive(Serialize, Deserialize)]
struct TapeFile {
    id: String,
    day: usize,
    brief: String,
    agent: String,
    checkers: [String; 2],
    expect: Result_,
    calls: Vec<Taped>,
}

fn tapes() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/tapes")
}

/// The errand, then the guard on its report, as B8 runs them.
async fn run(caller: &Caller, id: &str, day: usize, brief: &str) -> Result<Result_, String> {
    let mut world = World::demo();
    for _ in 0..day {
        world.next_day();
    }
    let ledger = Ledger::in_memory(id).map_err(|e| e.to_string())?;
    let errand = Errand { agent: "Scout", model: AGENT, brief, persona: "", memory: "", attachments: &[], documents: "", gateway: None };
    let done = errand::run(caller, &ledger, &mut world, &errand, &|_| {}).await.map_err(|e| e.to_string())?;
    let message = if done.message.is_empty() { "(The agent ran out of steps without reporting.)".to_string() } else { done.message.clone() };
    let checkers = Checkers { first: CHECKERS[0], second: Some(CHECKERS[1]), connected: &[] };
    let author = Author { model: AGENT, owner: "Asha Rao", persona: "" };
    let mut g = guard::deliver(caller, checkers, author, &message, &done.entries, &|_, _| {}).await.map_err(|e| e.to_string())?;
    guard::list_reads(&mut g, &done.entries);
    Ok(Result_ {
        ledger: done.entries.iter().map(|e| json!({ "action": e.action, "outcome": e.outcome })).collect(),
        delivered: g.delivered.message.clone(),
        correction: g.correction.clone(),
        false_delivered: g.delivered.false_claims.len(),
        rewrites: g.drafts.len() - 1,
    })
}

fn budget() -> Budget {
    let models: Vec<&str> = MODELS.iter().map(|m| m.id).chain([AGENT]).collect();
    Budget::new(&models, 1.0).unwrap()
}

#[tokio::test]
async fn recorded_errands_replay_to_the_same_ledger_and_message() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(tapes()).unwrap().flatten().map(|f| f.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    files.sort();
    assert!(!files.is_empty(), "no tapes in {}", tapes().display());
    for path in files {
        let file: TapeFile = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let tape = Tape::replaying(file.calls);
        // A client that can't reach anything: every reply comes off the tape.
        let caller = Caller::new(MuseClient::new("http://127.0.0.1:9", "replay"), Some(MuseClient::new("http://127.0.0.1:9", "replay")), budget()).with_tape(tape.clone());
        let got = run(&caller, &file.id, file.day, &file.brief).await.unwrap_or_else(|e| panic!("{}: {e}", file.id));
        assert_eq!(got, file.expect, "{}: today's code does something else with the same replies", file.id);
        assert_eq!(got.false_delivered, 0, "{}: a false claim was delivered", file.id);
        assert_eq!(tape.unused(), 0, "{}: replies the code no longer asks for", file.id);
        if tape.changed() > 0 {
            eprintln!("{}: {} calls replayed with a changed prompt (re-record to test the new wording)", file.id, tape.changed());
        }
    }
}

#[tokio::test]
#[ignore = "needs model keys; AGORA_RECORD=1 to write tapes"]
async fn record() {
    assert_eq!(std::env::var("AGORA_RECORD").as_deref(), Ok("1"), "set AGORA_RECORD=1 to overwrite the tapes");
    std::fs::create_dir_all(tapes()).unwrap();
    for (id, day, brief) in CASES {
        let tape = Tape::recording();
        let caller = Caller::new(MuseClient::from_env().unwrap(), Some(MuseClient::baseten_from_env().unwrap()), budget()).with_tape(tape.clone());
        let expect = run(&caller, id, day, brief).await.unwrap_or_else(|e| panic!("{id}: {e}"));
        eprintln!("{id}: {} calls, {} rewrites, {} false delivered, ${:.4}", tape.calls().len(), expect.rewrites, expect.false_delivered, caller.spend().cost_usd);
        let file = TapeFile { id: id.into(), day, brief: brief.into(), agent: AGENT.into(), checkers: CHECKERS.map(String::from), expect, calls: tape.calls() };
        std::fs::write(tapes().join(format!("{id}.json")), serde_json::to_string_pretty(&file).unwrap() + "\n").unwrap();
    }
}
