//! Every prompt and output schema the office sends a model, rendered to one reviewed
//! file (`tests/golden/prompts.txt`). B9 showed wording alone moves job scores ~20
//! points, so a prompt change should show up in review as a diff, not slip in with
//! code. And each call site has a size budget: prompts don't grow unnoticed.
//!
//! After an intended change: `UPDATE_GOLDEN=1 cargo test -p agora-engine golden`,
//! then review the diff of the file like code.

#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;

use agora_core::World;
use agora_muse::Message;
use serde_json::Value;

use crate::errand::Errand;
use crate::{CHARTER, alibi, chat, errand, memory, planner, run};

fn text(messages: &[Message]) -> String {
    messages.iter().map(|m| format!("[{:?}]\n{}", m.role, m.content)).collect::<Vec<_>>().join("\n\n")
}

fn json(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap()
}

/// (call site, what it sends before any data of the run: system prompts, the opening
/// for a fixed errand, the output schema), in a fixed order.
fn sites() -> Vec<(&'static str, String)> {
    let world = World::demo();
    let errand = Errand { agent: "Scout", model: "muse-spark-1.3", brief: "Get in touch with Arjun about dinner next week.", persona: "", memory: "", attachments: &[], documents: "", gateway: None };
    vec![
        ("errand turn", format!("{}\n\n[schema]\n{}", text(&errand::opening(&world, &errand)), json(&errand::turn_schema(&[])))),
        ("claim parser", format!("{}\n\n[schema]\n{}", alibi::EXTRACT_ROLE, json(&alibi::claims_schema(&[])))),
        ("planner", format!("{CHARTER}\n\n{}\n\n[schema]\n{}", planner::PLANNER_ROLE, json(&planner::schema(None)))),
        ("worker", format!("{CHARTER}\n\n{}", run::WORKER_ROLE)),
        ("verifier", format!("{CHARTER}\n\n{}\n\n[schema]\n{}", run::VERIFIER_ROLE, json(&run::verdict_schema()))),
        ("memory", format!("{}\n\n[schema]\n{}", memory::LEARN_ROLE, json(&memory::learn_schema()))),
        ("office chat", format!("{CHARTER}\n\n{}", chat::CHAT_ROLE)),
    ]
}

/// Characters each site may send before the run's own data: today's size plus ~15%.
/// Raise one only on purpose, with the prompt change that needs it.
const BUDGETS: [(&str, usize); 7] = [
    ("errand turn", 3900),
    ("claim parser", 3000),
    ("planner", 3200),
    ("worker", 700),
    ("verifier", 1200),
    ("memory", 3100),
    ("office chat", 800),
];

#[test]
fn every_prompt_is_in_the_reviewed_file_and_within_its_budget() {
    let sites = sites();
    let mut file = String::new();
    for (name, body) in &sites {
        let _ = write!(file, "===== {name} ({} chars) =====\n{body}\n\n", body.chars().count());
    }
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/prompts.txt");
    if std::env::var("UPDATE_GOLDEN").as_deref() == Ok("1") {
        std::fs::write(path, &file).unwrap();
    }
    let saved = std::fs::read_to_string(path).unwrap_or_default();
    if saved != file {
        let line = saved.lines().zip(file.lines()).position(|(a, b)| a != b).unwrap_or_else(|| saved.lines().count().min(file.lines().count()));
        panic!(
            "a prompt or schema changed (first difference at line {}: now {:?}). If intended: UPDATE_GOLDEN=1 cargo test -p agora-engine golden, and review the diff of tests/golden/prompts.txt",
            line + 1,
            file.lines().nth(line).unwrap_or("(end)")
        );
    }
    for (name, body) in &sites {
        let size = body.chars().count();
        let budget = BUDGETS.iter().find(|(n, _)| n == name).map_or(0, |b| b.1);
        eprintln!("{name}: {size} chars (budget {budget})");
        assert!(size <= budget, "{name} sends {size} characters, over its budget of {budget}");
    }
}
