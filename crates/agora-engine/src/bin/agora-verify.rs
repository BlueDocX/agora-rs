//! Check the action ledgers in a journal file, without trusting the server that
//! wrote them.
//!
//! `cargo run -p agora-engine --bin agora-verify -- [agora-journal.db] [errand-id [head-hash]]`
//!
//! For every errand (or just the one named) it recomputes the hash chain and prints
//! the entry count and the head hash, or the first entry that was changed, removed
//! or moved. With an errand id it also lists the entries, and with a head hash you
//! kept (the office shows it under every receipt) it also catches entries dropped
//! from the end. The file is opened read-only. Exits 1 if any check fails.

use std::path::PathBuf;

use agora_engine::Ledger;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().unwrap_or_else(|| "agora-journal.db".into()));
    let only = args.next();
    let head = args.next();
    let runs = match &only {
        Some(id) => vec![id.clone()],
        None => Ledger::runs(&path)?,
    };
    if runs.is_empty() {
        println!("No ledgers in {}.", path.display());
        return Ok(());
    }
    let mut broken = 0;
    for run in &runs {
        let ledger = Ledger::open_read_only(&path, run)?;
        if only.is_some() && ledger.entries()?.is_empty() {
            println!("No errand \"{run}\" in {}.", path.display());
            std::process::exit(1);
        }
        let checked = match &head {
            Some(h) => ledger.verify_head(h),
            None => ledger.verify(),
        };
        match checked {
            Ok(n) => {
                let head = ledger.entries()?.last().map(|e| e.hash.clone()).unwrap_or_default();
                println!("ok      {run}: {n} entries, head {head}");
            }
            Err(b) => {
                broken += 1;
                println!("BROKEN  {run}: entry {}: {}", b.seq, b.reason);
            }
        }
        if only.is_some() {
            for e in ledger.entries()? {
                let o = &e.outcome;
                println!("  #{:<3} {:<16} {:<44} {:<6} {}", e.seq, e.action.tool.name(), o.subject, if o.ok { "ok" } else { "failed" }, e.hash.get(..16).unwrap_or(&e.hash));
            }
        }
    }
    if broken > 0 {
        println!("{broken} of {} ledgers do not verify.", runs.len());
        std::process::exit(1);
    }
    Ok(())
}
