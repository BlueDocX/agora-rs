//! The action ledger: every tool call an agent made, written by the executor at the
//! moment it ran, never by the agent.
//!
//! Each entry is hashed together with the previous entry's hash, so changing,
//! dropping or reordering any past entry breaks every hash after it, and
//! [`Ledger::verify`] says where. Agents can't write here at all: `append` is
//! crate-private and only the errand executor calls it. When an agent later explains
//! what it did, this is the record its account is checked against.
//!
//! Dropping the *newest* entries leaves a shorter chain that is still valid; nothing
//! inside a chain can show what came after its end. So the head hash is published as
//! the run goes (every streamed step, every receipt) and [`Ledger::verify_head`]
//! checks a ledger against a head someone kept.
//!
//! The chain covers a hash of each entry's content (what was called on what, and
//! what came back), not the content itself. So a person can have one entry's content
//! **forgotten** ([`Ledger::forget`]): the content is deleted (and overwritten on
//! disk), while the tool, the time, whether it worked and the content hash stay. The
//! chain still verifies, and the ledger still proves *that* the agent read something
//! at that moment, without keeping *what* it read.
//!
//! The content hash is salted: SHA-256 of a random per-entry salt and the content.
//! Forgetting deletes the salt too, so the hash that stays can't be matched against
//! guesses ("was it the chat with Neha?") the way a bare hash of guessable text could.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agora_core::{Action, Outcome, Tool};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::journal::JournalError;

/// The hash before the first entry.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Shown in place of forgotten content.
pub const FORGOTTEN: &str = "[forgotten]";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// 1, 2, 3… within the run.
    pub seq: u64,
    pub agent: String,
    /// For a forgotten entry only `action.tool` is real; the rest reads [`FORGOTTEN`].
    pub action: Action,
    /// For a forgotten entry only `outcome.ok` is real.
    pub outcome: Outcome,
    pub at_ms: u64,
    /// SHA-256 of the salt and the content as written, kept when the content is forgotten.
    #[serde(default)]
    pub content_hash: String,
    /// The per-entry salt (hex), deleted with the content. Empty in ledgers written
    /// before salting, whose content hash is of the content alone.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub salt: String,
    /// When the content was forgotten, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forgotten_at_ms: Option<u64>,
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
    /// Whether the table has the `salt` column (a ledger opened read-only from before
    /// salting doesn't, and isn't changed to get it).
    salted: bool,
}

/// What an entry's content hash covers.
#[derive(Serialize)]
struct Content<'a> {
    action: &'a Action,
    outcome: &'a Outcome,
}

/// The exact bytes an entry's hash covers. Field order is fixed by the struct, so
/// the encoding is stable.
#[derive(Serialize)]
struct Hashed<'a> {
    run: &'a str,
    seq: u64,
    agent: &'a str,
    tool: Tool,
    ok: bool,
    at_ms: u64,
    content: &'a str,
    prev: &'a str,
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::with_capacity(64), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

fn content_json(action: &Action, outcome: &Outcome) -> String {
    serde_json::to_string(&Content { action, outcome }).unwrap_or_default()
}

/// The content hash: of the salt and the content, or of the content alone for an
/// entry written before salting (no salt).
fn commitment(salt: &str, content: &str) -> String {
    if salt.is_empty() { sha_hex(content.as_bytes()) } else { sha_hex(format!("{salt}\n{content}").as_bytes()) }
}

fn hash_of(h: &Hashed<'_>) -> String {
    sha_hex(&serde_json::to_vec(h).unwrap_or_default())
}

fn has_salt(db: &Connection) -> rusqlite::Result<bool> {
    db.query_row("SELECT COUNT(*) > 0 FROM pragma_table_info('ledger') WHERE name = 'salt'", [], |r| r.get(0))
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn to_u64(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// Proof that an entry's content was forgotten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub run: String,
    pub seq: u64,
    pub tool: Tool,
    pub at_ms: u64,
    /// The salted hash of the content that existed. Its salt is gone with it, so it
    /// can't be turned back into the content or checked against a guess.
    pub content_hash: String,
    pub forgotten_at_ms: u64,
    /// The chain still verifies with the content gone: this many entries, this head.
    pub chain_entries: usize,
    pub head: String,
    /// False if another reader kept the old copy in the write-ahead log for now (it
    /// is overwritten at the next checkpoint).
    pub wal_cleared: bool,
}

impl Ledger {
    /// The ledger for `run_id` in the database at `path` (created if missing).
    ///
    /// # Errors
    /// If the file can't be opened or the table created.
    pub fn open(path: &Path, run_id: &str) -> Result<Self, JournalError> {
        Self::with(Connection::open(path)?, run_id)
    }

    /// Open an existing ledger without writing anything to the file (no table
    /// creation, no journal-mode change): for checking evidence.
    ///
    /// # Errors
    /// If there is no such file, or it can't be opened.
    pub fn open_read_only(path: &Path, run_id: &str) -> Result<Self, JournalError> {
        if !path.is_file() {
            return Err(JournalError::new(format!("no ledger at {}", path.display())));
        }
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let salted = has_salt(&db)?;
        Ok(Self { db: Mutex::new(db), run_id: run_id.to_string(), salted })
    }

    /// A ledger that lives only as long as the process (tests, one-off errands).
    ///
    /// # Errors
    /// If SQLite can't create the table.
    pub fn in_memory(run_id: &str) -> Result<Self, JournalError> {
        Self::with(Connection::open_in_memory()?, run_id)
    }

    fn with(db: Connection, run_id: &str) -> Result<Self, JournalError> {
        // secure_delete: deleted content is overwritten on disk, not just unlinked.
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA secure_delete = ON;
             CREATE TABLE IF NOT EXISTS ledger (
                 run_id TEXT NOT NULL, seq INTEGER NOT NULL, agent TEXT NOT NULL,
                 tool TEXT NOT NULL, ok INTEGER NOT NULL, at_ms INTEGER NOT NULL,
                 content TEXT, content_hash TEXT NOT NULL, forgotten_at_ms INTEGER,
                 prev TEXT NOT NULL, hash TEXT NOT NULL, salt TEXT,
                 PRIMARY KEY (run_id, seq));",
        )?;
        // Ledgers from before salting gain the column (their entries keep no salt).
        if !has_salt(&db)? {
            db.execute_batch("ALTER TABLE ledger ADD COLUMN salt TEXT;")?;
        }
        Ok(Self { db: Mutex::new(db), run_id: run_id.to_string(), salted: true })
    }

    /// The runs (errands) that have entries in the database at `path`, oldest first.
    ///
    /// # Errors
    /// If the file can't be opened or read.
    pub fn runs(path: &Path) -> Result<Vec<String>, JournalError> {
        if !path.is_file() {
            return Err(JournalError::new(format!("no ledger at {}", path.display())));
        }
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let has: bool = db.query_row("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'ledger'", [], |r| r.get(0))?;
        if !has {
            return Ok(Vec::new());
        }
        let mut stmt = db.prepare("SELECT run_id FROM ledger GROUP BY run_id ORDER BY MIN(at_ms)")?;
        let runs = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(runs)
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record a call the executor just ran. Reads the head and writes the new entry
    /// under one lock, so concurrent agents still form a single chain.
    pub(crate) fn append(&self, agent: &str, action: &Action, outcome: &Outcome) -> Result<Entry, JournalError> {
        let db = self.lock();
        let head: Option<(i64, String)> = db
            .query_row("SELECT seq, hash FROM ledger WHERE run_id = ?1 ORDER BY seq DESC LIMIT 1", params![self.run_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let (seq, prev) = head.map_or((1, GENESIS.to_string()), |(s, h)| (to_u64(s) + 1, h));
        let at_ms = now_ms();
        let content = content_json(action, outcome);
        let salt = crate::signing::random_hex(16);
        let content_hash = commitment(&salt, &content);
        let hash = hash_of(&Hashed { run: &self.run_id, seq, agent, tool: action.tool, ok: outcome.ok, at_ms, content: &content_hash, prev: &prev });
        db.execute(
            "INSERT INTO ledger (run_id, seq, agent, tool, ok, at_ms, content, content_hash, forgotten_at_ms, prev, hash, salt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, ?11)",
            params![
            self.run_id,
            to_i64(seq),
            agent,
            action.tool.name(),
            outcome.ok,
            to_i64(at_ms),
            content,
            content_hash,
            prev,
            hash,
            salt
        ])?;
        Ok(Entry {
            seq,
            agent: agent.to_string(),
            action: action.clone(),
            outcome: outcome.clone(),
            at_ms,
            content_hash,
            salt,
            forgotten_at_ms: None,
            prev,
            hash,
        })
    }

    /// Every entry of this run, in order. An entry whose stored content no longer
    /// parses, or no longer matches its content hash, is an error: the ledger is the
    /// evidence, so it is never read leniently.
    ///
    /// # Errors
    /// If the database can't be read or an entry is unreadable.
    pub fn entries(&self) -> Result<Vec<Entry>, JournalError> {
        let db = self.lock();
        let salt = if self.salted { "salt" } else { "NULL" };
        let mut stmt = db.prepare(&format!(
            "SELECT seq, agent, tool, ok, at_ms, content, content_hash, forgotten_at_ms, prev, hash, {salt}
             FROM ledger WHERE run_id = ?1 ORDER BY seq"
        ))?;
        let rows = stmt.query_map(params![self.run_id], |r| {
            Ok(Row {
                seq: to_u64(r.get(0)?),
                agent: r.get(1)?,
                tool: r.get(2)?,
                ok: r.get(3)?,
                at_ms: to_u64(r.get(4)?),
                content: r.get(5)?,
                content_hash: r.get(6)?,
                forgotten_at_ms: r.get::<_, Option<i64>>(7)?.map(to_u64),
                prev: r.get(8)?,
                hash: r.get(9)?,
                salt: r.get::<_, Option<String>>(10)?.unwrap_or_default(),
            })
        })?;
        rows.map(|row| row.map_err(JournalError::from).and_then(Row::into_entry)).collect()
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

    /// Verify the chain and that it reaches `head`, a head hash published earlier
    /// (so entries dropped from the end are caught too).
    ///
    /// # Errors
    /// [`Broken`] if the chain doesn't verify or `head` isn't in it.
    pub fn verify_head(&self, head: &str) -> Result<usize, Broken> {
        let entries = self.entries().map_err(|e| Broken { seq: 0, reason: e.to_string() })?;
        let n = verify_chain(&self.run_id, &entries)?;
        match entries.iter().position(|e| e.hash == head) {
            Some(_) => Ok(n),
            None => Err(Broken { seq: entries.last().map_or(0, |e| e.seq) + 1, reason: "the published head is missing: entries were dropped from the end".into() }),
        }
    }

    /// Delete entry `seq`'s content for good, keeping what the chain needs. Returns
    /// the receipt, or `None` if there is no such entry. Forgetting twice is fine.
    ///
    /// # Errors
    /// If the database can't be written, or the chain doesn't verify afterwards.
    pub fn forget(&self, seq: u64) -> Result<Option<Receipt>, JournalError> {
        // Never delete evidence from a ledger that is already broken: the content
        // may be what proves it.
        self.verify().map_err(|b| JournalError::new(format!("not forgetting: the ledger is broken at entry {}: {}", b.seq, b.reason)))?;
        let forgotten_at_ms = now_ms();
        let changed = self.lock().execute(
            "UPDATE ledger SET content = NULL, salt = NULL, forgotten_at_ms = COALESCE(forgotten_at_ms, ?3) WHERE run_id = ?1 AND seq = ?2",
            params![self.run_id, to_i64(seq), to_i64(forgotten_at_ms)],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        // Old pages can linger in the write-ahead log: fold it into the (securely
        // overwritten) main file. A reader elsewhere can hold the log open; then the
        // receipt says the old copy is still waiting to be overwritten.
        let wal_cleared = self.checkpoint();
        let entries = self.entries()?;
        let chain_entries =
            verify_chain(&self.run_id, &entries).map_err(|b| JournalError::new(format!("the chain broke at entry {}: {}", b.seq, b.reason)))?;
        let Some(e) = entries.iter().find(|e| e.seq == seq) else { return Ok(None) };
        Ok(Some(Receipt {
            run: self.run_id.clone(),
            seq,
            tool: e.action.tool,
            at_ms: e.at_ms,
            content_hash: e.content_hash.clone(),
            forgotten_at_ms: e.forgotten_at_ms.unwrap_or(forgotten_at_ms),
            chain_entries,
            head: entries.last().map(|e| e.hash.clone()).unwrap_or_default(),
            wal_cleared,
        }))
    }

    /// Fold the write-ahead log into the main file and empty it, retrying briefly
    /// while another connection is reading. True when nothing is left in the log
    /// (or there is no log, as in memory).
    fn checkpoint(&self) -> bool {
        for attempt in 0..5 {
            let busy: Result<i64, _> = self.lock().query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0));
            match busy {
                Ok(0) => return true,
                Ok(_) if attempt < 4 => std::thread::sleep(std::time::Duration::from_millis(40)),
                _ => return false,
            }
        }
        false
    }
}

struct Row {
    seq: u64,
    agent: String,
    tool: String,
    ok: bool,
    at_ms: u64,
    content: Option<String>,
    content_hash: String,
    forgotten_at_ms: Option<u64>,
    prev: String,
    hash: String,
    salt: String,
}

#[derive(Deserialize)]
struct Stored {
    action: Action,
    outcome: Outcome,
}

impl Row {
    fn into_entry(self) -> Result<Entry, JournalError> {
        let seq = self.seq;
        let bad = |why: String| JournalError::new(format!("ledger entry {seq}: {why}"));
        let tool = Tool::from_stored_name(&self.tool).ok_or_else(|| bad(format!("unknown tool {}", self.tool)))?;
        let (action, outcome) = match &self.content {
            Some(content) => {
                if commitment(&self.salt, content) != self.content_hash {
                    return Err(bad("its content was changed after it was written".into()));
                }
                let stored: Stored = serde_json::from_str(content).map_err(|e| bad(format!("unreadable content: {e}")))?;
                // The clear columns (what survives forgetting) must agree with the content.
                if stored.action.tool != tool || stored.outcome.ok != self.ok {
                    return Err(bad("its content was changed after it was written".into()));
                }
                (stored.action, stored.outcome)
            }
            None => (
                Action { tool, target: FORGOTTEN.into(), when: String::new(), text: String::new(), call: None },
                Outcome { ok: self.ok, subject: FORGOTTEN.into(), result: String::new() },
            ),
        };
        Ok(Entry {
            seq,
            agent: self.agent,
            action,
            outcome,
            at_ms: self.at_ms,
            content_hash: self.content_hash,
            salt: if self.content.is_some() { self.salt } else { String::new() },
            forgotten_at_ms: self.forgotten_at_ms,
            prev: self.prev,
            hash: self.hash,
        })
    }
}

/// Check a sequence of entries forms one unbroken chain for `run`. Content that is
/// present must match its content hash; forgotten content is covered by the hash alone.
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
        let changed = || Broken { seq: e.seq, reason: "its content was changed after it was written".into() };
        if e.forgotten_at_ms.is_none() && commitment(&e.salt, &content_json(&e.action, &e.outcome)) != e.content_hash {
            return Err(changed());
        }
        let hash = hash_of(&Hashed {
            run,
            seq: e.seq,
            agent: &e.agent,
            tool: e.action.tool,
            ok: e.outcome.ok,
            at_ms: e.at_ms,
            content: &e.content_hash,
            prev: &e.prev,
        });
        if hash != e.hash {
            return Err(changed());
        }
        prev.clone_from(&e.hash);
    }
    Ok(entries.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_core::World;

    fn act(tool: Tool, target: &str) -> Action {
        Action { tool, target: target.into(), when: String::new(), text: "hi".into(), call: None }
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
        // Make the bounce look like a success, as an agent covering its tracks would:
        // in the content and in the clear `ok` column.
        let db = ledger.lock();
        let content: String = db.query_row("SELECT content FROM ledger WHERE seq = 2", [], |r| r.get(0)).unwrap();
        db.execute("UPDATE ledger SET content = ?1, ok = 1 WHERE seq = 2", params![content.replace("\"ok\":false", "\"ok\":true")]).unwrap();
        drop(db);
        let err = ledger.verify().unwrap_err();
        assert_eq!(err.seq, 0, "content that no longer matches its hash can't even be read");
        assert!(err.reason.contains("entry 2") && err.reason.contains("changed"), "{err:?}");
        // Faking the content hash too still breaks the chain hash.
        let db = ledger.lock();
        let content: String = db.query_row("SELECT content FROM ledger WHERE seq = 2", [], |r| r.get(0)).unwrap();
        let salt: String = db.query_row("SELECT salt FROM ledger WHERE seq = 2", [], |r| r.get(0)).unwrap();
        db.execute("UPDATE ledger SET content_hash = ?1 WHERE seq = 2", params![commitment(&salt, &content)]).unwrap();
        drop(db);
        assert_eq!(ledger.verify(), Err(Broken { seq: 2, reason: "its content was changed after it was written".into() }));
    }

    #[test]
    fn deleting_an_entry_is_caught() {
        let ledger = ledger_with_three();
        ledger.lock().execute("DELETE FROM ledger WHERE seq = 2", []).unwrap();
        assert_eq!(ledger.verify().unwrap_err().reason, "entry 2 is missing");
    }

    #[test]
    fn a_chain_copied_into_another_run_does_not_verify() {
        let ledger = ledger_with_three();
        let entries = ledger.entries().unwrap();
        assert_eq!(verify_chain("run-t", &entries), Ok(3));
        assert_eq!(verify_chain("run-other", &entries).unwrap_err().seq, 1);
    }

    #[test]
    fn dropping_the_newest_entry_is_caught_against_a_published_head() {
        let ledger = ledger_with_three();
        let head = ledger.entries().unwrap()[2].hash.clone();
        assert_eq!(ledger.verify_head(&head), Ok(3));
        ledger.lock().execute("DELETE FROM ledger WHERE seq = 3", []).unwrap();
        assert_eq!(ledger.verify(), Ok(2), "a shorter chain is still a valid chain");
        assert!(ledger.verify_head(&head).unwrap_err().reason.contains("dropped from the end"));
    }

    #[test]
    fn an_edited_clear_column_is_caught_and_blocks_forgetting() {
        let ledger = ledger_with_three();
        ledger.lock().execute("UPDATE ledger SET ok = 1 WHERE seq = 2", []).unwrap();
        assert_eq!(ledger.verify().unwrap_err().seq, 0);
        assert!(ledger.forget(2).unwrap_err().to_string().contains("not forgetting"));
        let kept: Option<String> = ledger.lock().query_row("SELECT content FROM ledger WHERE seq = 2", [], |r| r.get(0)).unwrap();
        assert!(kept.is_some(), "the evidence stays");
    }

    #[test]
    fn forgetting_deletes_the_content_but_the_chain_still_verifies() {
        let ledger = ledger_with_three();
        let receipt = ledger.forget(3).unwrap().unwrap();
        assert_eq!((receipt.tool, receipt.chain_entries), (Tool::MessagesRead, 3));
        let e = &ledger.entries().unwrap()[2];
        assert_eq!((e.action.target.as_str(), e.outcome.subject.as_str(), e.outcome.result.as_str()), (FORGOTTEN, FORGOTTEN, ""));
        assert!(e.outcome.ok && e.forgotten_at_ms.is_some());
        let stored: (Option<String>, Option<String>) = ledger.lock().query_row("SELECT content, salt FROM ledger WHERE seq = 3", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(stored, (None, None), "the salt goes with the content, so the hash can't be checked against a guess");
        assert_eq!(e.salt, "");
        assert_eq!(ledger.verify(), Ok(3));
        assert_eq!(ledger.forget(9).unwrap(), None);
        // But a forgotten entry can't be quietly turned into a different call.
        ledger.lock().execute("UPDATE ledger SET tool = 'messages.send' WHERE seq = 3", []).unwrap();
        assert_eq!(ledger.verify().unwrap_err().seq, 3);
    }

    #[test]
    fn the_content_hash_is_salted_so_two_identical_calls_hash_differently() {
        let ledger = Ledger::in_memory("run-s").unwrap();
        let mut world = World::demo();
        let a = act(Tool::MessagesRead, "Neha");
        for _ in 0..2 {
            let outcome = world.execute(&a);
            ledger.append("Scout", &a, &outcome).unwrap();
        }
        let e = ledger.entries().unwrap();
        assert_ne!(e[0].content_hash, e[1].content_hash);
        assert_ne!(e[0].content_hash, sha_hex(content_json(&e[0].action, &e[0].outcome).as_bytes()), "a guess of the content alone doesn't match");
        assert_eq!(e[0].salt.len(), 32);
    }

    #[test]
    fn a_ledger_from_before_salting_still_verifies() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE ledger (run_id TEXT NOT NULL, seq INTEGER NOT NULL, agent TEXT NOT NULL, tool TEXT NOT NULL, ok INTEGER NOT NULL,
             at_ms INTEGER NOT NULL, content TEXT, content_hash TEXT NOT NULL, forgotten_at_ms INTEGER, prev TEXT NOT NULL, hash TEXT NOT NULL,
             PRIMARY KEY (run_id, seq));",
        )
        .unwrap();
        let a = act(Tool::ContactsRead, "Arjun");
        let o = World::demo().execute(&a);
        let content = content_json(&a, &o);
        let content_hash = sha_hex(content.as_bytes());
        let hash = hash_of(&Hashed { run: "run-old", seq: 1, agent: "Scout", tool: a.tool, ok: o.ok, at_ms: 5, content: &content_hash, prev: GENESIS });
        db.execute("INSERT INTO ledger VALUES ('run-old', 1, 'Scout', 'contacts.read', 1, 5, ?1, ?2, NULL, ?3, ?4)", params![content, content_hash, GENESIS, hash]).unwrap();
        // Read-only (as agora-verify opens it): readable without being changed.
        let dir = std::env::temp_dir().join(format!("agora-old-{}", crate::signing::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        db.execute("VACUUM INTO ?1", params![path.to_string_lossy()]).unwrap();
        assert_eq!(Ledger::open_read_only(&path, "run-old").unwrap().verify(), Ok(1));
        std::fs::remove_dir_all(dir).unwrap();
        let ledger = Ledger::with(db, "run-old").unwrap();
        assert_eq!(ledger.verify(), Ok(1));
        ledger.append("Scout", &a, &o).unwrap();
        assert_eq!(ledger.verify(), Ok(2));
    }
}
