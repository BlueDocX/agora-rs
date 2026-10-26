//! Run one brief from the command line.
//!
//! `MODEL_API_KEY=... cargo run -p agora-engine --bin agora-run -- "your brief"`
//!
//! Optional: `AGORA_MODEL` (default `muse-spark-1.3`), `AGORA_CAP_USD` (default 0.50),
//! `AGORA_PARALLEL` (default 4). The deliverable goes to stdout, the report to stderr.

use agora_engine::{Outcome, RunConfig, run};
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
    let brief = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if brief.trim().is_empty() {
        return Err("usage: agora-run \"<brief>\"".into());
    }
    let config = RunConfig {
        model: env_or("AGORA_MODEL", "muse-spark-1.3".to_string())?,
        max_parallel: env_or("AGORA_PARALLEL", 4)?,
        cap_usd: env_or("AGORA_CAP_USD", 0.50)?,
    };
    let report = run(MuseClient::from_env()?, &config, &brief).await?;

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
        "{:.1}s · {} calls · in {} (cached {}, {:.0}%) · out {} (reasoning {}) · ${:.4}",
        report.elapsed.as_secs_f64(),
        s.calls,
        s.usage.input_tokens,
        s.usage.cached_tokens,
        s.usage.cache_ratio() * 100.0,
        s.usage.output_tokens,
        s.usage.reasoning_tokens,
        s.cost_usd,
    );
    println!("{}", report.deliverable());
    Ok(())
}
