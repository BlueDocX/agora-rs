//! Check the action ledgers in a journal file, without trusting the server that
//! wrote them.
//!
//! `cargo run -p agora-engine --bin agora-verify -- [agora-journal.db] [errand-id]`
//!
//! For every errand (or just the one named) it recomputes the hash chain and prints
//! the entry count and the head hash, or the first entry that was changed, removed
//! or moved. With an errand id it also lists the entries. Exits 1 if any chain is
//! broken.

use std::path::PathBuf;

use agora_engine::Ledger;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().unwrap_or_else(|| "agora-journal.db".into()));
    let only = args.next();
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
        let ledger = Ledger::open(&path, run)?;
        match ledger.verify() {
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
                println!("  #{:<3} {:<16} {:<44} {:<6} {}", e.seq, e.action.tool.name(), o.subject, if o.ok { "ok" } else { "failed" }, &e.hash[..16]);
            }
        }
    }
    if broken > 0 {
        println!("{broken} of {} ledgers do not verify.", runs.len());
        std::process::exit(1);
    }
    Ok(())
}
