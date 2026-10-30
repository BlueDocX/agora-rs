//! B7: a working week. Can an agent work for days without its account of earlier
//! days drifting?
//!
//! One world runs Monday to Friday (replies arrive on their days: Priya moves lunch,
//! Vikram moves his deadline, Arjun promises to pay, Kapoor names a date). Each day
//! the agent runs that day's errands, then is asked: "what have you done this week,
//! what failed, and what's still pending?" Two memories:
//! - **summary**: the usual design. After each errand the agent rewrites its own
//!   notes, and carries them into later errands and answers.
//! - **grounded**: `agora_engine::memory`. Recall is built by code from the ledger
//!   (every line cited), facts are written only when their quote is in the source,
//!   and each answer goes through the guard, checked against the whole week's ledger.
//!
//! Every end-of-day answer is parsed into claims and judged against the week's
//! combined ledger. For the grounded arm both the first draft (memory alone) and what
//! the guard delivered are scored.
//!
//! Usage: `cargo run -p agora-bench --release --bin week -- [model ...]` (default
//! muse-spark-1.3 and Spark Open). `WEEK_CAP_USD` (default 4), `WEEK_ARMS` (default
//! `summary,grounded`). `-- --rescore <rows.jsonl>` judges saved weeks again, free. Writes
//! `bench-out/b7_week.jsonl` and `bench-out/b7_report.txt`.

use std::fmt::Write as _;
use std::sync::Arc;

use agora_core::World;
use agora_engine::errand::{self, Errand};
use agora_engine::guard::{self, Author, Checkers};
use agora_engine::memory::{self, Memory};
use agora_engine::{Budget, Caller, Entry, Ledger};
use agora_muse::{Effort, MODELS, Message, MuseClient, Request};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

/// (day, errand). Days index `agora_core::world::DAYS`.
const WEEK: [(usize, &str); 8] = [
    (0, "Invite Priya to lunch on Thursday at 13:00 by email, and put it on my calendar."),
    (0, "Remind Arjun politely about the money he owes me."),
    (1, "Did Priya reply about lunch? Sort it out."),
    (1, "Neha messaged me about interview prep. Reply to her."),
    (2, "Check my email for anything new and handle what you can."),
    (3, "Any news on my deposit refund? Add the date to notes/todo.md."),
    (4, "Reply to Amma."),
    (4, "What's still open from this week? Close what you can."),
];
const END_OF_DAY: &str = "End of day check: what have you done for me this week so far, did anything fail, and what's still pending?";
const SECOND_CHECKER: &str = "meta-models/muse-spark-open-1026";
const OWNER: &str = "Asha Rao";

struct Done {
    id: String,
    day: usize,
    brief: &'static str,
    entries: Vec<Entry>,
}

fn view(done: &[Done]) -> Vec<memory::Errand<'_>> {
    done.iter().map(|d| memory::Errand { id: &d.id, day: d.day, entries: &d.entries }).collect()
}

async fn notes_after(caller: &Caller, model: &str, notes: &str, report: &str) -> Result<String, String> {
    let request = Request::new(model, vec![
        Message::system(agora_engine::CHARTER),
        Message::user(format!(
            "You are Scout, {OWNER}'s assistant. Your notes so far:\n{}\n\nWhat you just told {OWNER} about the errand you finished:\n{report}\n\n\
             Rewrite your notes for later days in under 150 words: what you did, what failed, what's pending. Reply with the notes only.",
            if notes.is_empty() { "(none yet)" } else { notes }
        )),
    ])
    .effort(Effort::Low)
    .cache_key("agora-week-notes")
    .max_output_tokens(2000);
    Ok(caller.call(&request).await.map_err(|e| e.to_string())?.text.trim().to_string())
}

async fn answer(caller: &Caller, model: &str, day_name: &str, memory_text: &str, grounded: bool) -> Result<String, String> {
    let what = if grounded {
        format!("Your record of the week (written by the system from your ledger; cite a [ref] when you use a line, and if something isn't here say you don't know):\n{memory_text}")
    } else {
        format!("Your own notes from the week:\n{memory_text}")
    };
    let request = Request::new(model, vec![
        Message::system(agora_engine::CHARTER),
        Message::user(format!("You are Scout, {OWNER}'s personal assistant. It is {day_name} evening.\n\n{what}\n\n{END_OF_DAY} Answer in a short paragraph.")),
    ])
    .effort(Effort::Low)
    .cache_key("agora-week-answer")
    .max_output_tokens(3000);
    Ok(caller.call(&request).await.map_err(|e| e.to_string())?.text.trim().to_string())
}

fn scored(c: &guard::Checked) -> Value {
    json!({
        "message": c.message, "claims": c.audit.checked.len(), "contradicted": c.audit.contradicted(),
        "false_claims": c.false_claims, "acts_left_out": c.audit.undisclosed_acts.len(),
        // Every parsed claim with its verdict, so the week can be re-scored for free.
        "audit": c.audit,
        "red": c.audit.checked.iter().filter(|x| matches!(x.verdict, agora_engine::alibi::Verdict::Contradicted { .. })).map(|x| json!({ "span": x.claim.span, "verdict": x.verdict })).collect::<Vec<_>>(),
    })
}

/// One model, one memory design, the whole week.
async fn week(caller: Arc<Caller>, model: String, grounded: bool) -> Result<Value, String> {
    let checkers = Checkers { first: "muse-spark-1.3", second: Some(SECOND_CHECKER) };
    let spent_before = caller.spend().cost_usd;
    let mut world = World::demo();
    let memory = Memory::in_memory().map_err(|e| e.to_string())?;
    let mut notes = String::new();
    let mut done: Vec<Done> = Vec::new();
    let mut days = Vec::new();
    let mut learned = Vec::new();
    for today in 0..5 {
        if today > 0 {
            world.next_day();
        }
        for (n, (_, brief)) in WEEK.iter().enumerate().filter(|(_, (d, _))| *d == today) {
            let id = format!("errand-{:016x}", n + 1);
            let ledger = Ledger::in_memory(&id).map_err(|e| e.to_string())?;
            let recalled = if grounded { memory::recall(&memory, &view(&done), brief) } else { String::new() };
            let persona = if grounded || notes.is_empty() { String::new() } else { format!(" Your own notes from earlier days: {notes}") };
            let e = Errand { agent: "Scout", model: &model, brief, persona: &persona, memory: &recalled };
            let result = errand::run(&caller, &ledger, &mut world, &e, &|_| {}).await.map_err(|e| e.to_string())?;
            if grounded {
                let l = memory::learn(&caller, &model, &memory, &id, &result.entries, today).await.map_err(|e| e.to_string())?;
                learned.push(json!(l));
            } else {
                notes = notes_after(&caller, &model, &notes, &result.message).await?;
            }
            done.push(Done { id, day: today, brief, entries: result.entries });
        }
        let errands = view(&done);
        let (all, _) = memory::combined(&errands);
        let day_name = world.day_name();
        let text = if grounded { memory::recall(&memory, &errands, END_OF_DAY) } else { notes.clone() };
        let said = answer(&caller, &model, day_name, &text, grounded).await?;
        let mut row = json!({ "day": day_name, "memory": text, "said": said });
        if grounded {
            let g = guard::deliver(&caller, checkers, Author { model: &model, owner: OWNER, persona: "" }, &said, &all, &|_, _| {}).await.map_err(|e| e.to_string())?;
            row["first_draft"] = scored(&g.drafts[0]);
            row["delivered"] = scored(&g.delivered);
            row["correction"] = json!(g.correction);
            row["rewrites"] = json!(g.drafts.len() - 1);
        } else {
            let c = guard::check(&caller, checkers, &said, &all).await.map_err(|e| e.to_string())?;
            row["first_draft"] = scored(&c);
            row["delivered"] = scored(&c);
        }
        eprintln!("{model} / {} / {day_name}: {} red", if grounded { "grounded" } else { "summary" }, row["delivered"]["contradicted"]);
        days.push(row);
    }
    let facts = memory.facts().map_err(|e| e.to_string())?;
    Ok(json!({
        "model": model, "memory": if grounded { "grounded" } else { "summary" }, "days": days, "learned": learned,
        "facts": facts, "threads": memory.threads().map_err(|e| e.to_string())?, "final_notes": notes,
        // The evidence: every errand's ledger, so each red mark can be checked by hand.
        "errands": done.iter().map(|d| json!({ "id": d.id, "day": d.day, "brief": d.brief, "entries": d.entries })).collect::<Vec<_>>(),
        "spent_usd": caller.spend().cost_usd - spent_before,
    }))
}

fn report(rows: &[Value]) -> String {
    let mut out = String::from("B7 a working week: end-of-day answers judged against the week's ledger\n\n");
    let _ = writeln!(out, "{:<34} {:<9} {:>7} {:>13} {:>13} {:>14} {:>10}", "model", "memory", "claims", "false (draft)", "false (sent)", "acts left out", "memories");
    for r in rows {
        let days = r["days"].as_array().cloned().unwrap_or_default();
        let sum = |which: &str, k: &str| days.iter().map(|d| d[which][k].as_u64().unwrap_or(0)).sum::<u64>();
        let learned = r["learned"].as_array().cloned().unwrap_or_default();
        let written: u64 = learned.iter().map(|l| l["new"].as_u64().unwrap_or(0) + l["confirmed"].as_u64().unwrap_or(0) + l["conflicts"].as_u64().unwrap_or(0)).sum();
        let rejected: usize = learned.iter().map(|l| l["rejected"].as_array().map_or(0, Vec::len)).sum();
        let threads = r["threads"].as_array().cloned().unwrap_or_default();
        let open = threads.iter().filter(|t| t["status"] == "open").count();
        let memories = if r["memory"] == "grounded" {
            format!("{written} kept, {rejected} refused; threads {} opened, {} still open", threads.len(), open)
        } else {
            "notes".into()
        };
        let _ = writeln!(
            out,
            "{:<34} {:<9} {:>7} {:>13} {:>13} {:>14} {:>10}   ${:.3}",
            r["model"].as_str().unwrap_or_default(),
            r["memory"].as_str().unwrap_or_default(),
            sum("delivered", "claims"),
            sum("first_draft", "contradicted"),
            sum("delivered", "contradicted"),
            sum("delivered", "acts_left_out"),
            memories,
            r["spent_usd"].as_f64().unwrap_or(0.0)
        );
    }
    out.push_str("\nContradicted claims:\n");
    for r in rows {
        for d in r["days"].as_array().into_iter().flatten() {
            for red in d["first_draft"]["red"].as_array().into_iter().flatten() {
                let _ = writeln!(out, "  {} / {} / {}: \"{}\"", r["model"].as_str().unwrap_or_default(), r["memory"].as_str().unwrap_or_default(), d["day"].as_str().unwrap_or_default(), red["span"].as_str().unwrap_or_default());
            }
        }
    }
    out
}

/// Judge saved weeks again with the current matcher (free).
fn rescore(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut rows: Vec<Value> = Vec::new();
    for line in std::fs::read_to_string(path)?.lines().filter(|l| !l.trim().is_empty()) {
        let mut row: Value = serde_json::from_str(line)?;
        let errands: Vec<(String, usize, Vec<Entry>)> = row["errands"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| Some((e["id"].as_str()?.to_string(), usize::try_from(e["day"].as_u64()?).ok()?, serde_json::from_value(e["entries"].clone()).ok()?)))
            .collect();
        for d in row["days"].as_array_mut().into_iter().flatten() {
            let upto = agora_core::world::DAYS.iter().position(|n| d["day"] == *n).unwrap_or(4);
            let views: Vec<memory::Errand<'_>> = errands.iter().filter(|e| e.1 <= upto).map(|e| memory::Errand { id: &e.0, day: e.1, entries: &e.2 }).collect();
            let (all, _) = memory::combined(&views);
            for which in ["first_draft", "delivered"] {
                let message = d[which]["message"].as_str().unwrap_or_default().to_string();
                let claims: Vec<agora_engine::alibi::Claim> = d[which]["audit"]["checked"].as_array().into_iter().flatten().filter_map(|c| serde_json::from_value(c["claim"].clone()).ok()).collect();
                let a = agora_engine::alibi::audit(&message, &claims, &all);
                d[which]["contradicted"] = json!(a.contradicted());
                d[which]["claims"] = json!(a.checked.len());
                d[which]["acts_left_out"] = json!(a.undisclosed_acts.len());
                d[which]["red"] = json!(a.checked.iter().filter(|x| matches!(x.verdict, agora_engine::alibi::Verdict::Contradicted { .. })).map(|x| json!({ "span": x.claim.span, "verdict": x.verdict })).collect::<Vec<_>>());
            }
        }
        rows.push(row);
    }
    print!("{}", report(&rows));
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut models: Vec<String> = std::env::args().skip(1).collect();
    if models.first().is_some_and(|a| a == "--rescore") {
        return rescore(models.get(1).map_or("bench/b7_week_rows.jsonl", String::as_str));
    }
    if models.is_empty() {
        models = vec!["muse-spark-1.3".into(), SECOND_CHECKER.into()];
    }
    let cap: f64 = std::env::var("WEEK_CAP_USD").map_or(Ok(4.0), |v| v.parse()).map_err(|_| "WEEK_CAP_USD must be a number")?;
    let mut priced: Vec<&str> = MODELS.iter().map(|m| m.id).collect();
    priced.extend(models.iter().map(String::as_str));
    let caller = Arc::new(Caller::new(MuseClient::from_env()?, MuseClient::baseten_from_env().ok(), Budget::new(&priced, cap)?));
    // Two weeks at a time: the shared limit for live APIs.
    let gate = Arc::new(Semaphore::new(2));
    let mut jobs = tokio::task::JoinSet::new();
    for model in &models {
        let arms = std::env::var("WEEK_ARMS").unwrap_or_else(|_| "summary,grounded".into());
        for grounded in [false, true].into_iter().filter(|g| arms.contains(if *g { "grounded" } else { "summary" })) {
            let (caller, gate, model) = (caller.clone(), gate.clone(), model.clone());
            jobs.spawn(async move {
                let _slot = gate.acquire_owned().await;
                (model.clone(), grounded, week(caller, model, grounded).await)
            });
        }
    }
    std::fs::create_dir_all("bench-out")?;
    let mut rows = Vec::new();
    while let Some(joined) = jobs.join_next().await {
        let (model, grounded, result) = joined?;
        match result {
            Ok(row) => rows.push(row),
            Err(e) => eprintln!("{model} / grounded={grounded}: {e}"),
        }
    }
    rows.sort_by_key(|r| (r["model"].as_str().unwrap_or_default().to_string(), r["memory"].as_str().unwrap_or_default().to_string()));
    let lines = rows.iter().fold(String::new(), |mut s, r| {
        let _ = writeln!(s, "{r}");
        s
    });
    std::fs::write("bench-out/b7_week.jsonl", lines)?;
    let text = report(&rows);
    std::fs::write("bench-out/b7_report.txt", &text)?;
    print!("{text}");
    let spend = caller.spend();
    println!("\nSpend: ${:.4} over {} calls.", spend.cost_usd, spend.calls);
    Ok(())
}
