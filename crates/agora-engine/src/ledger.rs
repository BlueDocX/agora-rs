//! The action ledger: every tool call an agent made, written by the executor at the
//! moment it ran, never by the agent.
//!
//! Each entry is hashed together with the previous entry's hash, so changing,
//! dropping or reordering any past entry breaks every hash after it, and
//! [`Ledger::verify`] says where. Agents can't write here at all: `append` is
//! crate-private and only the errand executor calls it. When an agent later explains
//! what it did, this is the record its account is checked against.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agora_core::{Action, Outcome};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::journal::JournalError;

/// The hash before the first entry.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// 1, 2, 3… within the run.
    pub seq: u64,
    pub agent: String,
    pub action: Action,
    pub outcome: Outcome,
    pub at_ms: u64,
    /// The previous entry's hash ([`GENESIS`] for the first).
    pub prev: String,
    pub hash: String,
}

/// Where the chain stops matching what was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broken {
    pub seq: u64,
    pub reason: String,
}

#[derive(Debug)]
pub struct Ledger {
    db: Mutex<Connection>,
    run_id: String,
}

/// The exact bytes an entry's hash covers: its position, run and content, and the
/// hash before it. Field order is fixed by the struct, so the encoding is stable.
#[derive(Serialize)]
struct Hashed<'a> {
    run: &'a str,
    seq: u64,
    agent: &'a str,
    action: &'a Action,
    outcome: &'a Outcome,
    at_ms: u64,
    prev: &'a str,
}

fn hash_of(h: &Hashed<'_>) -> String {
    let bytes = serde_json::to_vec(h).unwrap_or_default();
    Sha256::digest(&bytes).iter().fold(String::with_capacity(64), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl Ledger {
    /// The ledger for `run_id` in the database at `path` (created if missing).
    ///
    /// # Errors
    /// If the file can't be opened or the table created.
    pub fn open(path: &Path, run_id: &str) -> Result<Self, JournalError> {
        Self::with(Connection::open(path)?, run_id)
    }

    /// A ledger that lives only as long as the process (tests, one-off errands).
    ///
    /// # Errors
    /// If SQLite can't create the table.
    pub fn in_memory(run_id: &str) -> Result<Self, JournalError> {
        Self::with(Connection::open_in_memory()?, run_id)
    }

    fn with(db: Connection, run_id: &str) -> Result<Self, JournalError> {
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS actions (
                 run_id TEXT NOT NULL, seq INTEGER NOT NULL, agent TEXT NOT NULL,
                 action TEXT NOT NULL, outcome TEXT NOT NULL, at_ms INTEGER NOT NULL,
                 prev TEXT NOT NULL, hash TEXT NOT NULL,
                 PRIMARY KEY (run_id, seq));",
        )?;
        Ok(Self { db: Mutex::new(db), run_id: run_id.to_string() })
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Record a call the executor just ran. Reads the head and writes the new entry
    /// under one lock, so concurrent agents still form a single chain.
    pub(crate) fn append(&self, agent: &str, action: &Action, outcome: &Outcome) -> Result<Entry, JournalError> {
        let db = self.db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let head: Option<(i64, String)> = db
            .query_row(
                "SELECT seq, hash FROM actions WHERE run_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![self.run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map(Some)
            .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })?;
        let (seq, prev) = head.map_or((1, GENESIS.to_string()), |(s, h)| (u64::try_from(s).unwrap_or(0) + 1, h));
        let at_ms = now_ms();
        let hash = hash_of(&Hashed { run: &self.run_id, seq, agent, action, outcome, at_ms, prev: &prev });
        db.execute("INSERT INTO actions VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![
            self.run_id,
            to_i64(seq),
            agent,
            serde_json::to_string(action).unwrap_or_default(),
            serde_json::to_string(outcome).unwrap_or_default(),
            to_i64(at_ms),
            prev,
            hash
        ])?;
        Ok(Entry { seq, agent: agent.to_string(), action: action.clone(), outcome: outcome.clone(), at_ms, prev, hash })
    }

    /// Every entry of this run, in order. Entries whose stored JSON no longer parses
    /// are an error: the ledger is the evidence, so it is never read leniently.
    ///
    /// # Errors
    /// If the database can't be read or an entry is unreadable.
    pub fn entries(&self) -> Result<Vec<Entry>, JournalError> {
        let db = self.db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = db.prepare(
            "SELECT seq, agent, action, outcome, at_ms, prev, hash FROM actions WHERE run_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![self.run_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?, r.get(5)?, r.get(6)?))
        })?;
        rows.map(|row| {
            let (seq, agent, action, outcome, at_ms, prev, hash) = row?;
            let bad = |what: &str, e: serde_json::Error| JournalError::new(format!("ledger entry {seq}: unreadable {what}: {e}"));
            Ok(Entry {
                seq: u64::try_from(seq).unwrap_or(0),
                agent,
                action: serde_json::from_str(&action).map_err(|e| bad("action", e))?,
                outcome: serde_json::from_str(&outcome).map_err(|e| bad("outcome", e))?,
                at_ms: u64::try_from(at_ms).unwrap_or(0),
                prev,
                hash,
            })
        })
        .collect()
    }

    /// Recompute the chain. `Ok(n)` = all `n` entries are intact and in order.
    ///
    /// # Errors
    /// [`Broken`] at the first entry that was changed, removed or moved, or if the
    /// ledger can't be read at all (`seq` 0).
    pub fn verify(&self) -> Result<usize, Broken> {
        let entries = self.entries().map_err(|e| Broken { seq: 0, reason: e.to_string() })?;
        verify_chain(&self.run_id, &entries)
    }
}

/// Check a sequence of entries forms one unbroken chain for `run`.
///
/// # Errors
/// [`Broken`] at the first entry that doesn't follow from the one before.
pub fn verify_chain(run: &str, entries: &[Entry]) -> Result<usize, Broken> {
    let mut prev = GENESIS.to_string();
    for (i, e) in entries.iter().enumerate() {
        let expected_seq = i as u64 + 1;
        if e.seq != expected_seq {
            return Err(Broken { seq: e.seq, reason: format!("entry {expected_seq} is missing") });
        }
        if e.prev != prev {
            return Err(Broken { seq: e.seq, reason: "does not follow the entry before it".into() });
        }
        let hash = hash_of(&Hashed { run, seq: e.seq, agent: &e.agent, action: &e.action, outcome: &e.outcome, at_ms: e.at_ms, prev: &e.prev });
        if hash != e.hash {
            return Err(Broken { seq: e.seq, reason: "its content was changed after it was written".into() });
        }
        prev.clone_from(&e.hash);
    }
    Ok(entries.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_core::{Tool, World};

    fn act(tool: Tool, target: &str) -> Action {
        Action { tool, target: target.into(), when: String::new(), text: "hi".into() }
    }

    fn ledger_with_three() -> Ledger {
        let ledger = Ledger::in_memory("run-t").unwrap();
        let mut world = World::demo();
        for a in [act(Tool::ContactsRead, "Arjun"), act(Tool::EmailSend, "arjun.shah@oldmail.in"), act(Tool::MessagesRead, "Neha")] {
            let outcome = world.execute(&a);
            ledger.append("Scout", &a, &outcome).unwrap();
        }
        ledger
    }

    #[test]
    fn an_untouched_chain_verifies_and_reads_back() {
        let ledger = ledger_with_three();
        assert_eq!(ledger.verify(), Ok(3));
        let e = ledger.entries().unwrap();
        assert_eq!(e[0].prev, GENESIS);
        assert_eq!(e[1].prev, e[0].hash);
        assert!(!e[1].outcome.ok, "the bounce is on record");
    }

    #[test]
    fn rewriting_history_is_caught_at_the_entry_that_changed() {
        let ledger = ledger_with_three();
        // Make the bounce look like a success, as an agent covering its tracks would.
        let db = ledger.db.lock().unwrap();
        let mut outcome: Outcome = serde_json::from_str(&db.query_row("SELECT outcome FROM actions WHERE seq = 2", [], |r| r.get::<_, String>(0)).unwrap()).unwrap();
        outcome.ok = true;
        db.execute("UPDATE actions SET outcome = ?1 WHERE seq = 2", params![serde_json::to_string(&outcome).unwrap()]).unwrap();
        drop(db);
        assert_eq!(ledger.verify(), Err(Broken { seq: 2, reason: "its content was changed after it was written".into() }));
    }

    #[test]
    fn deleting_an_entry_is_caught() {
        let ledger = ledger_with_three();
        ledger.db.lock().unwrap().execute("DELETE FROM actions WHERE seq = 2", []).unwrap();
        assert_eq!(ledger.verify().unwrap_err().reason, "entry 2 is missing");
    }

    #[test]
    fn a_chain_copied_into_another_run_does_not_verify() {
        let ledger = ledger_with_three();
        let entries = ledger.entries().unwrap();
        assert_eq!(verify_chain("run-t", &entries), Ok(3));
        assert_eq!(verify_chain("run-other", &entries).unwrap_err().seq, 1);
    }
}
