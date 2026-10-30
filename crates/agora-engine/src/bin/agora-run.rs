//! Run one brief from the command line.
//!
//! `MODEL_API_KEY=... cargo run -p agora-engine --bin agora-run -- "your brief"`
//! `MODEL_API_KEY=... cargo run -p agora-engine --bin agora-run -- --resume <run-id>`
//!
//! Every completed model reply is journaled in `AGORA_JOURNAL` (default
//! `agora-journal.db`). Resuming replays those for free and continues from the first
//! call that never completed. Optional: `AGORA_MODEL` (default `muse-spark-1.3`),
//! `AGORA_CAP_USD` (a spending cap; none by default), `AGORA_PARALLEL` (default 4), and
//! `AGORA_REVIEWER=baseten` to review on Muse Spark Open (open weights) on Baseten
//! with `BASETEN_API_KEY` (`AGORA_REVIEWER_MODEL` overrides the model id), and
//! `AGORA_TEAM=<preset id>` (e.g. `creative-studio`) to staff the run with a team. The
//! deliverable goes to stdout, the report to stderr.

use std::path::PathBuf;

use agora_engine::{Journal, Outcome, RunConfig, run};
use agora_muse::MuseClient;

/// The variable parsed, or `default` if unset. A value that doesn't parse is an
/// error, never silently the default (this includes the spending cap).
fn env_or<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(v) => v.trim().parse().map_err(|_| format!("{name}={v:?} is not a valid value")),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path: PathBuf = env_or("AGORA_JOURNAL", PathBuf::from("agora-journal.db"))?;
    let mut config = RunConfig {
        model: env_or("AGORA_MODEL", "muse-spark-1.3".to_string())?,
        max_parallel: env_or("AGORA_PARALLEL", 4)?,
        cap_usd: env_or("AGORA_CAP_USD", f64::INFINITY)?,
        reviewer_model: match env_or("AGORA_REVIEWER", String::new())?.as_str() {
            "" | "meta" => None,
            // Baseten's docs say `meta-models/muse-spark-open`; with a hackathon key that id
            // answers 403 "check the api-key", and only the dated id works (checked live
            // 2026-10-30).
            "baseten" => Some(env_or("AGORA_REVIEWER_MODEL", "meta-models/muse-spark-open-1026".to_string())?),
            other => return Err(format!("AGORA_REVIEWER={other:?}: use \"baseten\" or leave it unset").into()),
        },
        // Baseten is required for AGORA_REVIEWER=baseten (so its key error shows), and
        // otherwise optional.
        baseten: if std::env::var("AGORA_REVIEWER").is_ok_and(|r| r.trim() == "baseten") {
            Some(MuseClient::baseten_from_env()?)
        } else {
            MuseClient::baseten_from_env().ok()
        },
        events: None,
        team: None,
    };
    let muse = MuseClient::from_env()?; // before journaling a run that can't start
    let (journal, brief) = match args.as_slice() {
        [flag, run_id] if flag == "--resume" => {
            // Replay only matches the same models, so the run's own setup wins over
            // whatever the environment says now.
            let (journal, record) = Journal::resume(&path, run_id)?;
            config.model = record.model;
            config.reviewer_model = record.reviewer;
            if std::env::var_os("AGORA_TEAM").is_some() {
                eprintln!("note: AGORA_TEAM is ignored on resume; the run keeps its own team");
            }
            config.team = record.team;
            (journal, record.brief)
        }
        _ if !args.is_empty() && !args[0].starts_with("--") && !args.join(" ").trim().is_empty() => {
            let brief = args.join(" ");
            config.team = match env_or("AGORA_TEAM", String::new())?.as_str() {
                "" => None,
                id => Some(agora_core::preset(id).ok_or_else(|| {
                    let ids: Vec<String> = agora_core::presets().into_iter().map(|t| t.id).collect();
                    format!("AGORA_TEAM={id:?}: use one of {}", ids.join(", "))
                })?),
            };
            config.check_hosts()?; // before a run record is written
            let reviewer = config.reviewer_model.as_deref();
            (Journal::start(&path, &brief, &config.model, reviewer, config.team.as_ref())?, brief)
        }
        _ => return Err("usage: agora-run \"<brief>\" | agora-run --resume <run-id>".into()),
    };
    eprintln!("run {} (journal {})", journal.run_id(), path.display());
    let report = run(muse, &config, &brief, Some(journal)).await?;

    if let Some(m) = &config.reviewer_model {
        eprintln!("reviewer: {m}");
    }
    eprintln!("plan: {} tasks (planner attempts: {})", report.planned.tasks.len(), report.planned.attempts);
    for (task, outcome) in report.planned.tasks.iter().zip(&report.outcomes) {
        let status = match outcome {
            Outcome::Done(a) => format!("accepted (drafts {}, reviews {})", a.drafts, a.reviews),
            Outcome::Failed(e) => format!("needs a person: {e}"),
            Outcome::Skipped => "skipped (an input task failed)".into(),
        };
        eprintln!("  {:<24} {status}", task.id);
    }
    let s = report.spend;
    eprintln!(
        "{:.1}s · {} calls ({} replayed free) · in {} (cached {}, {:.0}%) · out {} (reasoning {}) · ${:.4} (run total ${:.4})",
        report.elapsed.as_secs_f64(),
        s.calls,
        report.replayed,
        s.usage.input_tokens,
        s.usage.cached_tokens,
        s.usage.cache_ratio() * 100.0,
        s.usage.output_tokens,
        s.usage.reasoning_tokens,
        s.cost_usd,
        s.cost_usd + s.replayed_usd,
    );
    if report.journal_write_failures > 0 {
        eprintln!("warning: {} replies could not be journaled; a resume would pay for them again", report.journal_write_failures);
    }
    println!("{}", report.deliverable());
    Ok(())
}
