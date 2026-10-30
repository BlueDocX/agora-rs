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
//!
//! `agora-verify --receipt <errand-receipt.json> [agora-journal.db] [key-hex]` checks a
//! receipt the office signed (download it under any errand): that the signature is
//! good (and by `key-hex`, the office's published key, if given), and that the ledger
//! in the file still reaches the signed head with at least that many entries. A
//! ledger rewritten after the receipt was issued fails here, even if its own hashes
//! were all recomputed.

use std::path::{Path, PathBuf};

use agora_engine::{Ledger, SignedHead};

/// Check a signed receipt against the ledger in `path`.
fn check_receipt(receipt: &str, path: &Path, key: Option<&str>) -> Result<(), String> {
    let text = std::fs::read_to_string(receipt).map_err(|e| format!("can't read {receipt}: {e}"))?;
    let signed: SignedHead = serde_json::from_str(&text).map_err(|e| format!("{receipt} isn't a receipt: {e}"))?;
    signed.verify().map_err(|e| format!("the signature is bad: {e}"))?;
    if let Some(key) = key
        && !key.trim().eq_ignore_ascii_case(&signed.key)
    {
        return Err(format!("signed by {}, not by the office key you gave", signed.key));
    }
    println!("signature ok: {} entries, head {}, signed by {}", signed.entries, signed.head, signed.key);
    let ledger = Ledger::open_read_only(path, &signed.run).map_err(|e| e.to_string())?;
    let n = ledger.verify_head(&signed.head).map_err(|b| format!("BROKEN: entry {}: {}", b.seq, b.reason))?;
    if n < signed.entries {
        return Err(format!("BROKEN: the receipt saw {} entries, the ledger has {n}", signed.entries));
    }
    println!("ok      {}: the ledger in {} still reaches the signed head ({n} entries now)", signed.run, path.display());
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().is_some_and(|a| a == "--receipt") {
        args.next();
        let receipt = args.next().ok_or("--receipt needs a file")?;
        let path = PathBuf::from(args.next().unwrap_or_else(|| "agora-journal.db".into()));
        if let Err(why) = check_receipt(&receipt, &path, args.next().as_deref()) {
            println!("{why}");
            std::process::exit(1);
        }
        return Ok(());
    }
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
