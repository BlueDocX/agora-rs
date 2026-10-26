//! R1: check the live Model API does what the client assumes.
//!
//! `MODEL_API_KEY=... cargo run -p agora-muse --example smoke [model]`
//!
//! 1. A plain call: text, usage and cost come back.
//! 2. Structured output: the reply parses into our type.
//! 3. Prompt caching: two calls sharing a long prefix; the second should report
//!    `cached_tokens > 0`.

use std::time::Instant;

use agora_muse::{Effort, Message, MuseClient, Request, Response, price_for};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // printed with {:?}
struct Verdict {
    pass: bool,
    failed_criteria: Vec<String>,
}

fn report(label: &str, started: Instant, res: &Response) {
    let cost = price_for(&res.model).map_or_else(|| "unpriced".into(), |p| format!("${:.6}", p.cost_usd(&res.usage)));
    println!(
        "[{label}] {:.2}s · in {} (cached {}) · out {} (reasoning {}) · {cost} · remaining requests {:?}",
        started.elapsed().as_secs_f64(),
        res.usage.input_tokens,
        res.usage.cached_tokens,
        res.usage.output_tokens,
        res.usage.reasoning_tokens,
        res.rate_limit.remaining_requests,
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = std::env::args().nth(1).unwrap_or_else(|| "muse-spark-1.3".into());
    let muse = MuseClient::from_env()?;

    let t = Instant::now();
    let res = muse
        .respond(&Request::new(&model, vec![Message::user("Reply with exactly: agora online")]).effort(Effort::Minimal))
        .await?;
    report("plain", t, &res);
    println!("  text: {:?}", res.text);

    let schema = json!({
        "type": "object",
        "properties": {
            "pass": {"type": "boolean"},
            "failed_criteria": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["pass", "failed_criteria"],
        "additionalProperties": false
    });
    let t = Instant::now();
    let req = Request::new(
        &model,
        vec![
            Message::system("You check a deliverable against its acceptance criteria."),
            Message::user("Criteria: (1) under 10 words (2) mentions Rust.\nDeliverable: Agora is an office."),
        ],
    )
    .effort(Effort::Low)
    .json_schema("Verdict", schema);
    let (verdict, res): (Verdict, _) = muse.respond_json(&req).await?;
    report("structured", t, &res);
    println!("  verdict: {verdict:?}");

    // ~3k tokens of stable prefix, then a varying question.
    let charter = "Agora office charter. Every deliverable is checked against its acceptance criteria. ".repeat(250);
    for (i, question) in ["Name one office role.", "Name a different office role."].iter().enumerate() {
        let t = Instant::now();
        let req = Request::new(&model, vec![Message::system(&charter), Message::user(*question)])
            .effort(Effort::Minimal)
            .cache_key("smoke-charter");
        let res = muse.respond(&req).await?;
        report(&format!("cache {}", i + 1), t, &res);
    }
    Ok(())
}
