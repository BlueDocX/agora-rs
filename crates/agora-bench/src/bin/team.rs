//! B9: does each agent on a team do its own job well?
//!
//! One real brief per preset team. The team plans and does it (Spark Open on Baseten:
//! planner, workers and reviewer). Then a judge on a different model (Muse Spark 1.3)
//! reads every accepted task, blind to which version of Agora produced it, and grades
//! it against **that agent's own standards**: the rules of its skill playbooks and its
//! working note. For each task it also names the team member it would have given the
//! task to (role fit).
//!
//! Measured per run and per agent: tasks done, accepted on the first draft, standards
//! met, the judge's score (1-5: would a professional in this role ship it as is?),
//! role fit, and cost and time.
//!
//! Usage: `cargo run -p agora-bench --release --bin team -- [brief ids...]` (default:
//! all). `TEAM_MODEL` (default Spark Open on Baseten), `TEAM_JUDGE` (default
//! `muse-spark-1.3`), `TEAM_REPEATS` (default 2), `TEAM_CAP_USD` (default 6, the whole
//! batch), `TEAM_TAG` (names the output files), `TEAM_RESUME=1` (keep this tag's saved
//! rows, run the rest). One run at a time, two tasks in flight: Baseten sees at most
//! a few requests at once.

use std::fmt::Write as _;
use std::time::Instant;

use agora_core::team::{Team, presets};
use agora_engine::{Budget, Caller, Outcome, RunConfig, run};
use agora_muse::{Effort, MODELS, Message, MuseClient, Request};
use serde::Deserialize;
use serde_json::{Value, json};

const BRIEFS: [(&str, &str, &str); 8] = [
    ("cafe-oat", "agora", "Write a one-page brief for the owner of a small café in Pune on whether to add oat milk: likely costs, signs of demand, and a clear recommendation."),
    ("bottle-launch", "creative-studio", "Launch copy for a reusable steel water bottle aimed at college students: a tagline, three social posts, and a 100-word product description."),
    ("expand-first", "strategy-coach", "A 10-person Bangalore SaaS startup (invoicing for clinics) must choose its first expansion: the US or Southeast Asia. Recommend one, with the main risks."),
    ("library-hours", "pr-agency", "Announce that the city library will open until midnight during exam season: a press release and a short pitch to local reporters."),
    ("recycle-game", "game-studio", "Design a five-minute browser puzzle game about sorting recycling: the core loop, three levels, and scoring."),
    ("photo-threats", "security", "Threat-model a family photo-sharing web app: what it protects, the five most serious threats, and how to mitigate each."),
    ("notes-offline", "product", "Write a short spec for adding offline mode to a notes app: who needs it, scope, requirements, and how we'll know it worked."),
    ("four-day-week", "research-lab", "Explain what is known about four-day work weeks and productivity, with the caveats, and how a 50-person company could test it."),
];

const JUDGE_ROLE: &str = "You review a specialist's work for the person who hired the team. \
Judge it only against the standards given for that specialist's role and the task's \
acceptance criteria, strictly and literally, quoting the result. A standard that \
doesn't apply to this task is \"n/a\". Then score the work 1-5: 5 = a professional in \
this role would ship it as is; 3 = usable after edits; 1 = not usable. Finally name the \
team member (by id) best suited to this task, from the roster.";

#[derive(Debug, Deserialize)]
struct Grade {
    standards: Vec<Judged>,
    score: u8,
    best_fit: String,
    #[serde(default)]
    issues: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Judged {
    standard: String,
    verdict: String,
    #[serde(default)]
    quote: String,
}

fn grade_schema(ids: &[&str]) -> Value {
    json!({
        "type": "object", "additionalProperties": false,
        "required": ["standards", "score", "best_fit", "issues"],
        "properties": {
            "standards": { "type": "array", "items": { "type": "object", "additionalProperties": false,
                "required": ["standard", "verdict", "quote"],
                "properties": { "standard": { "type": "string" }, "verdict": { "type": "string", "enum": ["met", "not met", "n/a"] }, "quote": { "type": "string" } } } },
            "score": { "type": "integer", "enum": [1, 2, 3, 4, 5] },
            "best_fit": { "type": "string", "enum": ids },
            "issues": { "type": "array", "items": { "type": "string" } }
        }
    })
}

/// The standards an agent is held to: its working note and every rule of its skills.
fn standards(team: &Team, agent: &str) -> Vec<String> {
    let Some(a) = team.agent(agent) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    if !a.skills.trim().is_empty() {
        out.push(a.skills.trim().to_string());
    }
    for id in &a.kit {
        if let Some(skill) = agora_core::skills::skill(id) {
            out.extend(skill.playbook.iter().map(|r| (*r).to_string()));
        }
    }
    out
}

async fn judge(caller: &Caller, model: &str, team: &Team, agent: &str, task: &agora_engine::Task, text: &str) -> Result<Grade, String> {
    let a = team.agent(agent).ok_or("no such agent")?;
    let roster: Vec<String> = team.agents.iter().map(|m| format!("- {} ({}): {}", m.id, m.name, m.role)).collect();
    let rules: Vec<String> = standards(team, agent).iter().map(|s| format!("- {s}")).collect();
    let ids: Vec<&str> = team.agents.iter().map(|m| m.id.as_str()).collect();
    let prompt = format!(
        "Team roster:\n{}\n\nThe specialist: {} ({}), role: {}.\nStandards for this role:\n{}\n\nTask: {}\n{}\nAcceptance criteria:\n{}\n\nThe result:\n{}",
        roster.join("\n"),
        a.id,
        a.name,
        a.role,
        rules.join("\n"),
        task.title,
        task.instructions,
        task.acceptance.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n"),
        agora_core::quote(text.trim())
    );
    let request = Request::new(model, vec![Message::system(JUDGE_ROLE), Message::user(prompt)])
        .effort(Effort::Low)
        .json_schema("grade", grade_schema(&ids))
        .cache_key("agora-b9-judge")
        .max_output_tokens(8000);
    let reply = caller.call(&request).await.map_err(|e| e.to_string())?;
    serde_json::from_str(&reply.text).map_err(|e| format!("unparseable grade: {e}"))
}

#[allow(clippy::too_many_lines)]
async fn one(muse: MuseClient, baseten: MuseClient, model: &str, judge_model: &str, id: &str, repeat: usize, cap: f64) -> Result<Value, String> {
    let (_, team_id, brief) = BRIEFS.iter().find(|b| b.0 == id).ok_or("no such brief")?;
    let team = presets().into_iter().find(|t| t.id == *team_id).ok_or("no such team")?;
    let config = RunConfig {
        model: model.to_string(),
        max_parallel: 2,
        cap_usd: cap,
        reviewer_model: Some(model.to_string()),
        baseten: Some(baseten.clone()),
        events: None,
        team: Some(team.clone()),
        attachments: Vec::new(),
        documents: String::new(),
    };
    let started = Instant::now();
    let report = run(muse.clone(), &config, brief, None).await.map_err(|e| e.to_string())?;
    let run_ms = started.elapsed().as_millis();
    let priced: Vec<&str> = MODELS.iter().map(|m| m.id).collect();
    let judge_caller = Caller::new(muse, Some(baseten), Budget::new(&priced, 2.0).map_err(|e| e.to_string())?);
    let mut tasks = Vec::new();
    for (i, task) in report.planned.tasks.iter().enumerate() {
        let agent = task.agent.clone().unwrap_or_default();
        let (status, drafts, text) = match &report.outcomes[i] {
            Outcome::Done(a) => ("done", a.drafts, a.text.clone()),
            Outcome::Failed(e) => {
                tasks.push(json!({ "task": task.id, "title": task.title, "agent": agent, "status": "failed", "why": e.to_string() }));
                continue;
            }
            Outcome::Skipped => {
                tasks.push(json!({ "task": task.id, "title": task.title, "agent": agent, "status": "skipped" }));
                continue;
            }
        };
        let grade = judge(&judge_caller, judge_model, &team, &agent, task, &text).await;
        let mut row = json!({ "task": task.id, "title": task.title, "agent": agent, "status": status, "drafts": drafts, "words": agora_core::checks::word_count(&text), "text": text });
        match grade {
            Ok(g) => {
                let met = g.standards.iter().filter(|s| s.verdict == "met").count();
                let not_met = g.standards.iter().filter(|s| s.verdict == "not met").count();
                row["grade"] = json!({
                    "score": g.score, "met": met, "not_met": not_met, "fit": g.best_fit == agent, "best_fit": g.best_fit, "issues": g.issues,
                    "missed": g.standards.iter().filter(|s| s.verdict == "not met").map(|s| json!({ "standard": s.standard, "quote": s.quote })).collect::<Vec<_>>(),
                });
            }
            Err(e) => row["grade_error"] = json!(e),
        }
        tasks.push(row);
    }
    let judge_spend = judge_caller.spend();
    Ok(json!({
        "id": id, "team": team_id, "repeat": repeat, "brief": brief, "tasks": tasks,
        "run": { "usd": report.spend.cost_usd, "calls": report.spend.calls, "ms": run_ms },
        "judge": { "usd": judge_spend.cost_usd, "calls": judge_spend.calls },
        "deliverable": report.deliverable(),
    }))
}

fn float(n: usize) -> f64 {
    u32::try_from(n).map_or(f64::MAX, f64::from)
}

fn report(rows: &[Value], label: &str) -> String {
    let tasks: Vec<&Value> = rows.iter().flat_map(|r| r["tasks"].as_array().into_iter().flatten()).collect();
    let n = float(rows.len().max(1));
    let done = tasks.iter().filter(|t| t["status"] == "done").count();
    let first = tasks.iter().filter(|t| t["drafts"] == 1).count();
    let graded: Vec<&&Value> = tasks.iter().filter(|t| t["grade"].is_object()).collect();
    let met: u64 = graded.iter().map(|t| t["grade"]["met"].as_u64().unwrap_or(0)).sum();
    let not_met: u64 = graded.iter().map(|t| t["grade"]["not_met"].as_u64().unwrap_or(0)).sum();
    let score: u64 = graded.iter().map(|t| t["grade"]["score"].as_u64().unwrap_or(0)).sum();
    let fit = graded.iter().filter(|t| t["grade"]["fit"] == true).count();
    let shipped = graded.iter().filter(|t| t["grade"]["score"].as_u64().unwrap_or(0) >= 4).count();
    let g = float(graded.len().max(1));
    let pct = |k: usize, of: usize| 100.0 * float(k) / float(of.max(1));
    let usd: f64 = rows.iter().map(|r| r["run"]["usd"].as_f64().unwrap_or(0.0)).sum();
    let secs: f64 = rows.iter().map(|r| r["run"]["ms"].as_f64().unwrap_or(0.0)).sum::<f64>() / 1000.0;
    let mut out = String::new();
    let _ = writeln!(out, "B9 team quality: {label}, {} runs, {} tasks\n", rows.len(), tasks.len());
    let _ = writeln!(out, "tasks done          {done}/{} ({:.0}%)", tasks.len(), pct(done, tasks.len()));
    let _ = writeln!(out, "accepted 1st draft  {first}/{done} ({:.0}%)", pct(first, done));
    let met_pct = 100.0 * float(usize::try_from(met).unwrap_or(0)) / float(usize::try_from(met + not_met).unwrap_or(1).max(1));
    let _ = writeln!(out, "standards met       {met}/{} ({met_pct:.0}%)", met + not_met);
    let _ = writeln!(out, "judge score (1-5)   {:.2} mean; {shipped}/{} ship-ready (4+)", float(usize::try_from(score).unwrap_or(0)) / g, graded.len());
    let _ = writeln!(out, "role fit            {fit}/{} ({:.0}%)", graded.len(), pct(fit, graded.len()));
    let _ = writeln!(out, "per run (mean)      ${:.3}, {:.0}s (the team; judging is extra)", usd / n, secs / n);
    out.push_str("\nPer agent (team/agent: tasks, mean score, standards met):\n");
    let mut agents: Vec<(String, Vec<&&Value>)> = Vec::new();
    for t in &graded {
        let team = rows.iter().find(|r| r["tasks"].as_array().is_some_and(|ts| ts.iter().any(|x| std::ptr::eq(x, **t)))).map_or("", |r| r["team"].as_str().unwrap_or(""));
        let key = format!("{team}/{}", t["agent"].as_str().unwrap_or(""));
        match agents.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(t),
            None => agents.push((key, vec![t])),
        }
    }
    agents.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, ts) in &agents {
        let s: u64 = ts.iter().map(|t| t["grade"]["score"].as_u64().unwrap_or(0)).sum();
        let m: u64 = ts.iter().map(|t| t["grade"]["met"].as_u64().unwrap_or(0)).sum();
        let nm: u64 = ts.iter().map(|t| t["grade"]["not_met"].as_u64().unwrap_or(0)).sum();
        let _ = writeln!(out, "  {key:<36} {:>2}  {:.1}  {m}/{}", ts.len(), float(usize::try_from(s).unwrap_or(0)) / float(ts.len()), m + nm);
    }
    out.push_str("\nNot done:\n");
    for r in rows {
        for t in r["tasks"].as_array().into_iter().flatten().filter(|t| t["status"] != "done") {
            let _ = writeln!(out, "  {} #{} {} ({}): {}", r["id"].as_str().unwrap_or(""), r["repeat"], t["task"].as_str().unwrap_or(""), t["agent"].as_str().unwrap_or(""), t["status"].as_str().unwrap_or(""));
        }
    }
    out
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let only: Vec<String> = std::env::args().skip(1).collect();
    let model = std::env::var("TEAM_MODEL").unwrap_or_else(|_| "meta-models/muse-spark-open-1026".into());
    let judge_model = std::env::var("TEAM_JUDGE").unwrap_or_else(|_| "muse-spark-1.3".into());
    let repeats: usize = std::env::var("TEAM_REPEATS").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
    let cap: f64 = std::env::var("TEAM_CAP_USD").ok().and_then(|v| v.parse().ok()).unwrap_or(6.0);
    let tag = std::env::var("TEAM_TAG").unwrap_or_else(|_| "b9".into());
    let resume = std::env::var("TEAM_RESUME").is_ok_and(|v| v == "1");
    let muse = MuseClient::from_env()?;
    let baseten = MuseClient::baseten_from_env()?;
    std::fs::create_dir_all("bench-out")?;
    let path = format!("bench-out/b9_{tag}.jsonl");
    let mut rows: Vec<Value> = if resume { std::fs::read_to_string(&path).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect() } else { Vec::new() };
    let mut saved = std::fs::OpenOptions::new().create(true).write(true).append(resume).truncate(!resume).open(&path)?;
    let briefs: Vec<&str> = BRIEFS.iter().map(|b| b.0).filter(|id| only.is_empty() || only.iter().any(|o| o == id)).collect();
    let per_run = cap / float((briefs.len() * repeats).max(1)) * 3.0;
    let mut spent: f64 = rows.iter().map(|r| r["run"]["usd"].as_f64().unwrap_or(0.0) + r["judge"]["usd"].as_f64().unwrap_or(0.0)).sum();
    'all: for repeat in 0..repeats {
        for id in &briefs {
            if rows.iter().any(|r| r["id"] == *id && r["repeat"] == repeat) {
                continue;
            }
            match one(muse.clone(), baseten.clone(), &model, &judge_model, id, repeat, per_run).await {
                Ok(row) => {
                    spent += row["run"]["usd"].as_f64().unwrap_or(0.0) + row["judge"]["usd"].as_f64().unwrap_or(0.0);
                    let done = row["tasks"].as_array().map_or(0, |t| t.iter().filter(|x| x["status"] == "done").count());
                    eprintln!("{id} #{repeat}: {done}/{} tasks done (${spent:.2} so far)", row["tasks"].as_array().map_or(0, Vec::len));
                    std::io::Write::write_all(&mut saved, format!("{row}\n").as_bytes())?;
                    rows.push(row);
                }
                Err(e) => eprintln!("{id} #{repeat}: error: {e}"),
            }
            if spent > cap {
                eprintln!("over the cap: stopping");
                break 'all;
            }
        }
    }
    let text = report(&rows, &tag);
    std::fs::write(format!("bench-out/b9_{tag}.txt"), &text)?;
    print!("{text}");
    Ok(())
}
