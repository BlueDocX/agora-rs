//! Memory that can't drift: an agent working for days remembers only what its
//! ledger (or the person) can back.
//!
//! B4 measured the usual approach: an agent that carries its own account forward
//! (a summary) denies or invents a third of its claims about earlier work. A summary
//! of a summary drifts further each day. So here the agent never remembers in its own
//! words:
//! - **The record** is the ledger: every call of every errand, hash-chained.
//! - **Facts** ("Arjun Shah · email: arjun.shah@newco.in") are proposed by a model
//!   from tool results, but each must quote the result it came from. Code accepts
//!   it only if the quote is really in that ledger entry and the value is in the
//!   quote ([`learn`]). A memory the model made up is never written; the rejects are
//!   counted.
//! - **Threads** ("waiting for Priya to confirm lunch") are opened and closed only
//!   with a ledger entry that shows it.
//! - **Recall** is built by code ([`recall`]): the relevant facts, the open threads
//!   and the acts of earlier days, each with its `[ref]`. Nothing is summarised.
//!
//! Forgetting is designed in, not left to grow:
//! - a fact not confirmed for [`FACT_TTL_DAYS`] office days is retired, and the store
//!   holds at most [`MAX_FACTS`] (the least recently confirmed go first);
//! - forgetting a ledger entry deletes every fact and thread that cites it
//!   ([`Memory::forget_source`]);
//! - two sources that disagree (Arjun's address in the contacts vs the one he wrote
//!   from) are both kept and marked a **conflict**, shown to the agent and the
//!   person, and never merged by code: the person picks ([`Memory::resolve`]).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;

use agora_core::Weight;
use agora_muse::{Effort, Message, Request};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::call::{CallError, Caller};
use crate::journal::JournalError;
use crate::ledger::Entry;

/// Office days a fact lives without being confirmed again.
pub const FACT_TTL_DAYS: usize = 30;
/// Facts kept at most; past it, the least recently confirmed are retired.
pub const MAX_FACTS: usize = 500;
/// New facts a day that count as runaway growth (watch the slope, not the size).
pub const GROWTH_ALERT_PER_DAY: usize = 50;
/// Lines of each kind recall may show.
const RECALL_FACTS: usize = 14;
const RECALL_READS: usize = 10;
/// Acts of earlier errands recall shows: the most recent (prompts must not grow with
/// the office's whole history).
pub const RECALL_ACTS: usize = 40;
const RECALL_RESULT_CHARS: usize = 160;

/// Where a memory came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// A ledger entry: errand id and the entry's seq.
    Ledger { errand: String, seq: u64 },
    /// The person said so (e.g. when resolving a conflict).
    Person { who: String },
}

impl Origin {
    /// `[a1b2c3d4#3]`: how a line cites its source.
    #[must_use]
    pub fn cite(&self) -> String {
        match self {
            Self::Ledger { errand, seq } => format!("[{}#{seq}]", short(errand)),
            Self::Person { who } => format!("[told by {who}]"),
        }
    }
}

fn short(errand: &str) -> &str {
    let id = errand.strip_prefix("errand-").unwrap_or(errand);
    id.get(..8).unwrap_or(id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    /// Another source says something else about the same thing; the person decides.
    Conflict,
    /// Expired, pushed out, or set aside when the person resolved a conflict.
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub id: i64,
    /// Who or what it is about ("Arjun Shah").
    pub subject: String,
    /// Which property ("email", "lunch with Priya").
    pub attribute: String,
    pub value: String,
    /// The exact words in the source that say so.
    pub quote: String,
    pub origin: Origin,
    pub learned_day: usize,
    pub confirmed_day: usize,
    pub status: Status,
    /// Why it was retired, or how a conflict was resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Open,
    Done,
}

/// Something still pending across days ("waiting for Priya to confirm lunch").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: i64,
    pub text: String,
    pub opened: Origin,
    pub opened_day: usize,
    pub status: ThreadStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<Origin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_day: Option<usize>,
}

/// What adding a fact did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    New(i64),
    /// The same fact was already known: it is confirmed again (and keeps living).
    Confirmed(i64),
    /// It disagrees with these known facts: all are now conflicts.
    Conflict(Vec<i64>),
}

/// Sizes, for watching growth (the slope, not just the size).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub active: usize,
    pub conflicts: usize,
    pub retired: usize,
    pub open_threads: usize,
    pub done_threads: usize,
}

pub struct Memory {
    db: Mutex<Connection>,
}

impl std::fmt::Debug for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memory").finish_non_exhaustive()
    }
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The same fact said twice: equal, or one starts with the other ("Thursday 13:00"
/// and "Thursday 13:00 Kestrel interview"). Not mere containment: "Thursday now, not
/// Friday" holds "Friday" and is a change, not a confirmation.
fn same_value(a: &str, b: &str) -> bool {
    let (a, b) = (norm(a), norm(b));
    a.starts_with(&b) || b.starts_with(&a)
}

/// After a fact goes, a conflict left with one side (or with sides that now agree)
/// is no conflict: make it active again.
fn settle_conflicts(db: &Connection) -> rusqlite::Result<()> {
    let open: Vec<(i64, String, String, String)> = db
        .prepare("SELECT id, lower(subject), lower(attribute), value FROM memory_facts WHERE status = 'conflict'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    for (id, subject, attribute, value) in &open {
        let others: Vec<String> = db
            .prepare("SELECT value FROM memory_facts WHERE id != ?1 AND status != 'retired' AND lower(subject) = ?2 AND lower(attribute) = ?3")?
            .query_map(params![id, subject, attribute], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        if others.iter().all(|o| same_value(o, value)) {
            db.execute("UPDATE memory_facts SET status = 'active' WHERE id = ?1", params![id])?;
        }
    }
    Ok(())
}

fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn to_usize(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Active => "active",
        Status::Conflict => "conflict",
        Status::Retired => "retired",
    }
}

fn status_of(s: &str) -> Status {
    match s {
        "active" => Status::Active,
        "conflict" => Status::Conflict,
        _ => Status::Retired,
    }
}

impl Memory {
    /// The memory in the database at `path` (the journal file), created if missing.
    ///
    /// # Errors
    /// If the file can't be opened or the tables created.
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        Self::with(Connection::open(path)?)
    }

    /// A memory that lives as long as the process (tests, benchmarks).
    ///
    /// # Errors
    /// If SQLite can't create the tables.
    pub fn in_memory() -> Result<Self, JournalError> {
        Self::with(Connection::open_in_memory()?)
    }

    fn with(db: Connection) -> Result<Self, JournalError> {
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA secure_delete = ON;
             CREATE TABLE IF NOT EXISTS memory_facts (
                 id INTEGER PRIMARY KEY, subject TEXT NOT NULL, attribute TEXT NOT NULL, value TEXT NOT NULL,
                 quote TEXT NOT NULL, origin TEXT NOT NULL, learned_day INTEGER NOT NULL, confirmed_day INTEGER NOT NULL,
                 status TEXT NOT NULL, note TEXT);
             CREATE TABLE IF NOT EXISTS memory_threads (
                 id INTEGER PRIMARY KEY, text TEXT NOT NULL, opened TEXT NOT NULL, opened_day INTEGER NOT NULL,
                 status TEXT NOT NULL, closed TEXT, closed_day INTEGER);
             CREATE TABLE IF NOT EXISTS memory_growth (
                 day INTEGER PRIMARY KEY, active INTEGER NOT NULL, conflicts INTEGER NOT NULL, retired INTEGER NOT NULL,
                 open_threads INTEGER NOT NULL);",
        )?;
        Ok(Self { db: Mutex::new(db) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every fact, retired ones too, oldest first.
    ///
    /// # Errors
    /// If the database can't be read.
    pub fn facts(&self) -> Result<Vec<Fact>, JournalError> {
        let db = self.lock();
        let mut stmt = db.prepare("SELECT id, subject, attribute, value, quote, origin, learned_day, confirmed_day, status, note FROM memory_facts ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(Fact {
                id: r.get(0)?,
                subject: r.get(1)?,
                attribute: r.get(2)?,
                value: r.get(3)?,
                quote: r.get(4)?,
                origin: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(Origin::Person { who: "?".into() }),
                learned_day: to_usize(r.get(6)?),
                confirmed_day: to_usize(r.get(7)?),
                status: status_of(&r.get::<_, String>(8)?),
                note: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every thread, oldest first.
    ///
    /// # Errors
    /// If the database can't be read.
    pub fn threads(&self) -> Result<Vec<Thread>, JournalError> {
        let db = self.lock();
        let mut stmt = db.prepare("SELECT id, text, opened, opened_day, status, closed, closed_day FROM memory_threads ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            let closed: Option<String> = r.get(5)?;
            Ok(Thread {
                id: r.get(0)?,
                text: r.get(1)?,
                opened: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or(Origin::Person { who: "?".into() }),
                opened_day: to_usize(r.get(3)?),
                status: if r.get::<_, String>(4)? == "open" { ThreadStatus::Open } else { ThreadStatus::Done },
                closed: closed.and_then(|c| serde_json::from_str(&c).ok()),
                closed_day: r.get::<_, Option<i64>>(6)?.map(to_usize),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Add a fact (already checked against its source). The same fact again is a
    /// confirmation; a different value for the same subject and attribute is a
    /// conflict, kept side by side for the person to resolve.
    ///
    /// # Errors
    /// If the database can't be written.
    pub fn add_fact(&self, subject: &str, attribute: &str, value: &str, quote: &str, origin: &Origin, day: usize) -> Result<Added, JournalError> {
        let db = self.lock();
        let known: Vec<(i64, String)> = db
            .prepare("SELECT id, value FROM memory_facts WHERE status != 'retired' AND lower(subject) = ?1 AND lower(attribute) = ?2")?
            .query_map(params![norm(subject), norm(attribute)], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        if let Some((id, _)) = known.iter().find(|(_, v)| same_value(v, value)) {
            db.execute("UPDATE memory_facts SET confirmed_day = MAX(confirmed_day, ?2) WHERE id = ?1", params![id, to_i64(day)])?;
            return Ok(Added::Confirmed(*id));
        }
        let status = if known.is_empty() { Status::Active } else { Status::Conflict };
        db.execute(
            "INSERT INTO memory_facts (subject, attribute, value, quote, origin, learned_day, confirmed_day, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
            params![subject.trim(), attribute.trim(), value.trim(), quote.trim(), serde_json::to_string(origin).unwrap_or_default(), to_i64(day), status_name(status)],
        )?;
        let id = db.last_insert_rowid();
        if known.is_empty() {
            return Ok(Added::New(id));
        }
        for (other, _) in &known {
            db.execute("UPDATE memory_facts SET status = 'conflict' WHERE id = ?1", params![other])?;
        }
        Ok(Added::Conflict(known.iter().map(|(i, _)| *i).chain([id]).collect()))
    }

    /// The person settles a conflict: `keep` stands, the others about the same thing
    /// are retired with a note saying so.
    ///
    /// # Errors
    /// If the database can't be written, or there is no such fact.
    pub fn resolve(&self, keep: i64, who: &str) -> Result<Fact, JournalError> {
        let fact = self.facts()?.into_iter().find(|f| f.id == keep && f.status != Status::Retired).ok_or_else(|| JournalError::new("no such fact"))?;
        let db = self.lock();
        db.execute(
            "UPDATE memory_facts SET status = 'retired', note = ?3 WHERE id != ?1 AND status != 'retired' AND lower(subject) = ?2 AND lower(attribute) = ?4",
            params![keep, norm(&fact.subject), format!("{who} chose another value"), norm(&fact.attribute)],
        )?;
        db.execute("UPDATE memory_facts SET status = 'active', note = ?2 WHERE id = ?1", params![keep, format!("confirmed by {who}")])?;
        drop(db);
        self.facts()?.into_iter().find(|f| f.id == keep).ok_or_else(|| JournalError::new("no such fact"))
    }

    /// Undo a retirement (rollback): the fact is live again. If something else is now
    /// known about the same thing, both become a conflict rather than one silently
    /// replacing the other.
    ///
    /// # Errors
    /// If the database can't be written, or there is no such retired fact.
    pub fn restore(&self, id: i64) -> Result<Fact, JournalError> {
        let fact = self.facts()?.into_iter().find(|f| f.id == id && f.status == Status::Retired).ok_or_else(|| JournalError::new("no such retired fact"))?;
        let db = self.lock();
        let others: Vec<(i64, String)> = db
            .prepare("SELECT id, value FROM memory_facts WHERE id != ?1 AND status != 'retired' AND lower(subject) = ?2 AND lower(attribute) = ?3")?
            .query_map(params![id, norm(&fact.subject), norm(&fact.attribute)], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let disagree: Vec<i64> = others.iter().filter(|(_, v)| !same_value(v, &fact.value)).map(|(i, _)| *i).collect();
        let status = if disagree.is_empty() { "active" } else { "conflict" };
        db.execute("UPDATE memory_facts SET status = ?2, note = 'restored' WHERE id = ?1", params![id, status])?;
        for other in &disagree {
            db.execute("UPDATE memory_facts SET status = 'conflict' WHERE id = ?1", params![other])?;
        }
        drop(db);
        self.facts()?.into_iter().find(|f| f.id == id).ok_or_else(|| JournalError::new("no such fact"))
    }

    /// Record today's size, and say whether facts grew faster than
    /// [`GROWTH_ALERT_PER_DAY`] since the day before (a store that only grows ends up
    /// burying what matters).
    ///
    /// # Errors
    /// If the database can't be written.
    pub fn record_growth(&self, today: usize) -> Result<Option<String>, JournalError> {
        let s = self.stats()?;
        let db = self.lock();
        db.execute(
            "INSERT OR REPLACE INTO memory_growth VALUES (?1, ?2, ?3, ?4, ?5)",
            params![to_i64(today), to_i64(s.active), to_i64(s.conflicts), to_i64(s.retired), to_i64(s.open_threads)],
        )?;
        let before: Option<i64> = db.query_row("SELECT active FROM memory_growth WHERE day < ?1 ORDER BY day DESC LIMIT 1", params![to_i64(today)], |r| r.get(0)).optional()?;
        let grew = before.map_or(0, |b| s.active.saturating_sub(to_usize(b)));
        Ok((grew > GROWTH_ALERT_PER_DAY).then(|| format!("the office's memory grew by {grew} facts in a day (over {GROWTH_ALERT_PER_DAY}): check what is being kept")))
    }

    /// Size by day: (day, active, conflicts, retired, open threads).
    ///
    /// # Errors
    /// If the database can't be read.
    pub fn growth(&self) -> Result<Vec<[usize; 5]>, JournalError> {
        let db = self.lock();
        let mut stmt = db.prepare("SELECT day, active, conflicts, retired, open_threads FROM memory_growth ORDER BY day")?;
        let rows = stmt.query_map([], |r| Ok([to_usize(r.get(0)?), to_usize(r.get(1)?), to_usize(r.get(2)?), to_usize(r.get(3)?), to_usize(r.get(4)?)]))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Delete one fact for good (the person doesn't want it remembered).
    ///
    /// # Errors
    /// If the database can't be written.
    pub fn delete_fact(&self, id: i64) -> Result<bool, JournalError> {
        let db = self.lock();
        let gone = db.execute("DELETE FROM memory_facts WHERE id = ?1", params![id])? > 0;
        settle_conflicts(&db)?;
        Ok(gone)
    }

    /// Open a thread (the caller has checked its source).
    ///
    /// # Errors
    /// If the database can't be written.
    pub fn open_thread(&self, text: &str, origin: &Origin, day: usize) -> Result<i64, JournalError> {
        let db = self.lock();
        let same: Option<i64> = db.query_row("SELECT id FROM memory_threads WHERE status = 'open' AND lower(text) = ?1", params![norm(text)], |r| r.get(0)).optional()?;
        if let Some(id) = same {
            return Ok(id);
        }
        db.execute(
            "INSERT INTO memory_threads (text, opened, opened_day, status) VALUES (?1, ?2, ?3, 'open')",
            params![text.trim(), serde_json::to_string(origin).unwrap_or_default(), to_i64(day)],
        )?;
        Ok(db.last_insert_rowid())
    }

    /// Close an open thread with the entry that shows it's done.
    ///
    /// # Errors
    /// If the database can't be written.
    pub fn close_thread(&self, id: i64, origin: &Origin, day: usize) -> Result<bool, JournalError> {
        let changed = self.lock().execute(
            "UPDATE memory_threads SET status = 'done', closed = ?2, closed_day = ?3 WHERE id = ?1 AND status = 'open'",
            params![id, serde_json::to_string(origin).unwrap_or_default(), to_i64(day)],
        )?;
        Ok(changed > 0)
    }

    /// A ledger entry was forgotten: delete every fact and thread it is the source of.
    /// A thread it closed opens again (what showed it done is gone). Answers how many
    /// facts and threads went.
    ///
    /// # Errors
    /// If the database can't be written.
    pub fn forget_source(&self, errand: &str, seq: u64) -> Result<(usize, usize), JournalError> {
        let origin = serde_json::to_string(&Origin::Ledger { errand: errand.to_string(), seq }).unwrap_or_default();
        let db = self.lock();
        let facts = db.execute("DELETE FROM memory_facts WHERE origin = ?1", params![origin])?;
        let threads = db.execute("DELETE FROM memory_threads WHERE opened = ?1", params![origin])?;
        db.execute("UPDATE memory_threads SET status = 'open', closed = NULL, closed_day = NULL WHERE closed = ?1", params![origin])?;
        settle_conflicts(&db)?;
        Ok((facts, threads))
    }

    /// Run the forgetting rules: retire facts not confirmed for [`FACT_TTL_DAYS`],
    /// then the least recently confirmed past [`MAX_FACTS`]. Answers how many of each.
    ///
    /// # Errors
    /// If the database can't be written.
    pub fn consolidate(&self, today: usize) -> Result<(usize, usize), JournalError> {
        let db = self.lock();
        let expired = db.execute(
            "UPDATE memory_facts SET status = 'retired', note = 'not confirmed for 30 days' WHERE status != 'retired' AND confirmed_day + ?2 < ?1",
            params![to_i64(today), to_i64(FACT_TTL_DAYS)],
        )?;
        let over = db.execute(
            "UPDATE memory_facts SET status = 'retired', note = 'pushed out: the memory is full' WHERE id IN (
                 SELECT id FROM memory_facts WHERE status != 'retired' ORDER BY confirmed_day DESC, id DESC LIMIT -1 OFFSET ?1)",
            params![to_i64(MAX_FACTS)],
        )?;
        Ok((expired, over))
    }

    /// Counts, for watching how the memory grows.
    ///
    /// # Errors
    /// If the database can't be read.
    pub fn stats(&self) -> Result<Stats, JournalError> {
        let facts = self.facts()?;
        let threads = self.threads()?;
        let count = |s: Status| facts.iter().filter(|f| f.status == s).count();
        Ok(Stats {
            active: count(Status::Active),
            conflicts: count(Status::Conflict),
            retired: count(Status::Retired),
            open_threads: threads.iter().filter(|t| t.status == ThreadStatus::Open).count(),
            done_threads: threads.iter().filter(|t| t.status == ThreadStatus::Done).count(),
        })
    }
}

/// One errand's ledger, as recall and the week's check see it.
#[derive(Debug, Clone)]
pub struct Errand<'a> {
    pub id: &'a str,
    /// The office day it ran on (an index into `agora_core::world::DAYS`).
    pub day: usize,
    pub entries: &'a [Entry],
}

const DAY_SHORT: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

fn day(d: usize) -> &'static str {
    DAY_SHORT[d.min(6)]
}

fn words(s: &str) -> HashSet<String> {
    const COMMON: &[&str] = &["the", "and", "for", "with", "that", "this", "from", "your", "you", "about", "what", "did", "anything", "still", "week", "today", "please"];
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 3).map(str::to_lowercase).filter(|w| !COMMON.contains(&w.as_str())).collect()
}

fn line(errand: &str, day_index: usize, e: &Entry) -> String {
    let o = &e.outcome;
    let what = if e.forgotten_at_ms.is_some() {
        "(content forgotten at the person's request)".to_string()
    } else {
        let flat = o.result.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" / ");
        if flat.chars().count() > RECALL_RESULT_CHARS { format!("{}…", flat.chars().take(RECALL_RESULT_CHARS).collect::<String>()) } else { flat }
    };
    format!("[{}#{}, {}] {} · {} → {}: {what}", short(errand), e.seq, day(day_index), e.action.tool.name(), o.subject, if o.ok { "ok" } else { "FAILED" })
}

/// What the agent recalls for `query`, built by code from the memory and its earlier
/// errands. Every line carries its `[ref]`:
/// - facts about what the query names (conflicts shown as conflicts);
/// - every open thread;
/// - every act of earlier errands (sends, bookings, writes, failed or not);
/// - private reads and lookups of earlier errands about what the query names.
///
/// Empty when there is nothing to recall.
#[must_use]
pub fn recall(memory: &Memory, earlier: &[Errand<'_>], query: &str) -> String {
    let wanted = words(query);
    let overlap = |text: &str| words(text).intersection(&wanted).count();
    let mut out = String::new();
    let facts = memory.facts().unwrap_or_default();
    let mut relevant: Vec<&Fact> = facts.iter().filter(|f| f.status != Status::Retired && overlap(&format!("{} {} {}", f.subject, f.attribute, f.value)) > 0).collect();
    relevant.sort_by_key(|f| std::cmp::Reverse((overlap(&format!("{} {} {}", f.subject, f.attribute, f.value)), f.confirmed_day)));
    relevant.truncate(RECALL_FACTS);
    if !relevant.is_empty() {
        out.push_str("Facts (each from the source cited):\n");
        let mut shown = HashSet::new();
        for f in &relevant {
            if f.status == Status::Conflict {
                let key = (norm(&f.subject), norm(&f.attribute));
                if !shown.insert(key.clone()) {
                    continue;
                }
                let sides: Vec<String> =
                    facts.iter().filter(|g| g.status == Status::Conflict && (norm(&g.subject), norm(&g.attribute)) == key).map(|g| format!("\"{}\" {}", g.value, g.origin.cite())).collect();
                let _ = writeln!(out, "- UNRESOLVED {} · {}: sources disagree: {}. Ask before relying on either.", f.subject, f.attribute, sides.join(" vs "));
            } else {
                let _ = writeln!(out, "- {} · {}: {} {}", f.subject, f.attribute, f.value, f.origin.cite());
            }
        }
    }
    let threads: Vec<Thread> = memory.threads().unwrap_or_default().into_iter().filter(|t| t.status == ThreadStatus::Open).collect();
    if !threads.is_empty() {
        out.push_str("Open threads (still pending):\n");
        for t in &threads {
            let _ = writeln!(out, "- (thread {}) since {}: {} {}", t.id, day(t.opened_day), t.text, t.opened.cite());
        }
    }
    let acts: Vec<String> = earlier.iter().flat_map(|e| e.entries.iter().filter(|x| x.action.tool.weight() == Weight::Act).map(move |x| line(e.id, e.day, x))).collect();
    if !acts.is_empty() {
        out.push_str("What you did on earlier errands (sends, bookings, writes; from the ledger):\n");
        let older = acts.len().saturating_sub(RECALL_ACTS);
        if older > 0 {
            let _ = writeln!(out, "({older} earlier acts not shown: say so if asked about them, don't guess)");
        }
        for a in &acts[older..] {
            let _ = writeln!(out, "- {a}");
        }
    }
    let mut reads: Vec<(usize, String)> = Vec::new();
    for e in earlier {
        for x in e.entries.iter().filter(|x| x.action.tool.weight() != Weight::Act) {
            let score = words(&format!("{} {} {}", x.action.target, x.outcome.subject, x.outcome.result)).intersection(&wanted).count();
            if score > 0 {
                reads.push((score, line(e.id, e.day, x)));
            }
        }
    }
    reads.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    reads.truncate(RECALL_READS);
    if !reads.is_empty() {
        out.push_str("Earlier reads and lookups about this:\n");
        for (_, r) in &reads {
            let _ = writeln!(out, "- {r}");
        }
    }
    out
}

/// Every entry of several errands as one list the guard can check a claim about any
/// of them against: renumbered 1, 2, 3… in order, with a map back to (errand, seq).
#[must_use]
pub fn combined(errands: &[Errand<'_>]) -> (Vec<Entry>, Vec<(String, u64)>) {
    let mut all = Vec::new();
    let mut map = Vec::new();
    for e in errands {
        for x in e.entries {
            let mut x = x.clone();
            map.push((e.id.to_string(), x.seq));
            x.seq = all.len() as u64 + 1;
            all.push(x);
        }
    }
    (all, map)
}

const LEARN_ROLE: &str = "You keep an assistant's long-term memory. You are shown the \
calls it just made (each with its seq and what it returned) and the threads still open. \
Return:\n\
- facts: durable things worth knowing on later days (an address, a date someone \
promised, who wants what, a booking that exists). Each fact names the entry seq it \
comes from and copies quote EXACTLY from that entry's result (a short phrase). value \
must appear in the quote. subject is the person's full name as in the contacts \
(\"Mr. Kapoor\", \"Arjun Shah\") or the thing itself; attribute is a short plain \
name (\"email\", \"phone\", \"refund date\"). Never infer, never combine entries, never \
record what the assistant did (the ledger already has that).\n\
- open: things STILL pending after these calls that a later day must follow up \
(waiting for a reply, a promised date to check), each citing the seq that shows it. \
Don't open what these calls already did, and don't open one that says the same as an \
open thread listed below.\n\
- close: ids of open threads listed below that these calls show are done or no longer \
needed, each citing the seq that shows it.\n\
Return empty lists when nothing qualifies. Fewer, well-sourced items are better.";

fn learn_schema() -> Value {
    let s = json!({ "type": "string" });
    let n = json!({ "type": "integer" });
    json!({
        "type": "object", "additionalProperties": false, "required": ["facts", "open", "close"],
        "properties": {
            "facts": { "type": "array", "items": { "type": "object", "additionalProperties": false,
                "required": ["subject", "attribute", "value", "seq", "quote"],
                "properties": { "subject": s, "attribute": s, "value": s, "seq": n, "quote": s } } },
            "open": { "type": "array", "items": { "type": "object", "additionalProperties": false,
                "required": ["text", "seq"], "properties": { "text": s, "seq": n } } },
            "close": { "type": "array", "items": { "type": "object", "additionalProperties": false,
                "required": ["thread", "seq"], "properties": { "thread": n, "seq": n } } }
        }
    })
}

#[derive(Deserialize)]
struct Proposed {
    facts: Vec<ProposedFact>,
    open: Vec<ProposedThread>,
    close: Vec<ProposedClose>,
}

#[derive(Deserialize)]
struct ProposedFact {
    subject: String,
    attribute: String,
    value: String,
    seq: u64,
    quote: String,
}

#[derive(Deserialize)]
struct ProposedThread {
    text: String,
    seq: u64,
}

#[derive(Deserialize)]
struct ProposedClose {
    thread: i64,
    seq: u64,
}

/// What one learning pass did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Learned {
    pub proposed: usize,
    /// New, confirmed again, or a conflict.
    pub new: usize,
    pub confirmed: usize,
    pub conflicts: usize,
    /// Proposed but not written, and why (a quote that isn't in its source, …).
    pub rejected: Vec<String>,
    pub opened: usize,
    pub closed: usize,
}

/// Why a proposed fact can't be written, or `None` if its source backs it.
fn unbacked(f: &ProposedFact, entries: &[Entry]) -> Option<String> {
    let Some(e) = entries.iter().find(|e| e.seq == f.seq) else { return Some(format!("cites #{}, which isn't in this errand", f.seq)) };
    if e.forgotten_at_ms.is_some() || !e.outcome.ok {
        return Some(format!("#{} is forgotten or failed", f.seq));
    }
    if [&f.subject, &f.attribute, &f.value].iter().any(|t| t.trim().is_empty() || t.chars().count() > 120) {
        return Some("a field is empty or too long".into());
    }
    let source = norm(&format!("{} {}", e.outcome.subject, e.outcome.result));
    let quote = norm(&f.quote);
    if quote.is_empty() || !source.contains(&quote) {
        return Some(format!("the quote \"{}\" isn't in #{}", f.quote, f.seq));
    }
    if !quote.contains(&norm(&f.value)) {
        return Some(format!("the value \"{}\" isn't in its quote", f.value));
    }
    None
}

/// After an errand: have `model` propose facts and threads from its calls, write the
/// ones their sources back, and run the forgetting rules.
///
/// # Errors
/// If the model call fails or the memory can't be written.
pub async fn learn(caller: &Caller, model: &str, memory: &Memory, errand: &str, entries: &[Entry], today: usize) -> Result<Learned, CallError> {
    let mut learned = Learned::default();
    let usable: Vec<&Entry> = entries.iter().filter(|e| e.forgotten_at_ms.is_none()).collect();
    if usable.is_empty() {
        return Ok(learned);
    }
    let calls = usable.iter().fold(String::new(), |mut s, e| {
        let _ = writeln!(s, "#{} {} · {} → {}:\n{}\n", e.seq, e.action.tool.name(), e.outcome.subject, if e.outcome.ok { "ok" } else { "FAILED" }, e.outcome.result);
        s
    });
    let open: Vec<Thread> = memory.threads().map_err(|e| CallError::Unfinished { status: e.to_string() })?.into_iter().filter(|t| t.status == ThreadStatus::Open).collect();
    let threads = open.iter().fold(String::new(), |mut s, t| {
        let _ = writeln!(s, "thread {}: {}", t.id, t.text);
        s
    });
    let request = Request::new(model, vec![
        Message::system(LEARN_ROLE),
        Message::user(format!("Calls:\n{calls}\nOpen threads:\n{}", if threads.is_empty() { "(none)\n" } else { &threads })),
    ])
    .effort(Effort::Low)
    .json_schema("memory", learn_schema())
    .cache_key("agora-memory-learn")
    .max_output_tokens(4000);
    let reply = caller.call(&request).await?;
    let proposed: Proposed = serde_json::from_str(&reply.text).map_err(|e| CallError::Unfinished { status: format!("unparseable memory: {e}") })?;
    let store_err = |e: JournalError| CallError::Unfinished { status: e.to_string() };
    learned.proposed = proposed.facts.len() + proposed.open.len() + proposed.close.len();
    for f in &proposed.facts {
        if let Some(why) = unbacked(f, entries) {
            learned.rejected.push(format!("{} · {}: {why}", f.subject, f.attribute));
            continue;
        }
        let origin = Origin::Ledger { errand: errand.to_string(), seq: f.seq };
        match memory.add_fact(&f.subject, &f.attribute, &f.value, &f.quote, &origin, today).map_err(store_err)? {
            Added::New(_) => learned.new += 1,
            Added::Confirmed(_) => learned.confirmed += 1,
            Added::Conflict(_) => learned.conflicts += 1,
        }
    }
    let has = |seq: u64| entries.iter().any(|e| e.seq == seq && e.forgotten_at_ms.is_none());
    for t in &proposed.open {
        if !has(t.seq) || t.text.trim().is_empty() || t.text.chars().count() > 200 {
            learned.rejected.push(format!("thread \"{}\": no usable source #{}", t.text, t.seq));
            continue;
        }
        memory.open_thread(&t.text, &Origin::Ledger { errand: errand.to_string(), seq: t.seq }, today).map_err(store_err)?;
        learned.opened += 1;
    }
    for c in &proposed.close {
        if !has(c.seq) || !open.iter().any(|t| t.id == c.thread) {
            learned.rejected.push(format!("closing thread {}: no usable source #{}", c.thread, c.seq));
            continue;
        }
        if memory.close_thread(c.thread, &Origin::Ledger { errand: errand.to_string(), seq: c.seq }, today).map_err(store_err)? {
            learned.closed += 1;
        }
    }
    memory.consolidate(today).map_err(store_err)?;
    Ok(learned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;
    use agora_core::{Action, Tool, World};

    fn entries() -> Vec<Entry> {
        let ledger = Ledger::in_memory("errand-aaaabbbbccccdddd").unwrap();
        let mut world = World::demo();
        for (tool, target) in [(Tool::ContactsRead, "Arjun"), (Tool::EmailRead, "e5"), (Tool::EmailSend, "priya.menon@example.in")] {
            let a = Action { tool, target: target.into(), when: "Lunch".into(), text: "Thursday?".into(), call: None };
            let o = world.execute(&a);
            ledger.append("Scout", &a, &o).unwrap();
        }
        ledger.entries().unwrap()
    }

    fn proposed(subject: &str, value: &str, seq: u64, quote: &str) -> ProposedFact {
        ProposedFact { subject: subject.into(), attribute: "email".into(), value: value.into(), seq, quote: quote.into() }
    }

    #[test]
    fn a_memory_is_written_only_if_its_source_says_so() {
        let e = entries();
        assert_eq!(unbacked(&proposed("Arjun Shah", "arjun.shah@oldmail.in", 1, "email arjun.shah@oldmail.in"), &e), None);
        assert!(unbacked(&proposed("Arjun Shah", "arjun@gmail.com", 1, "email arjun@gmail.com"), &e).unwrap().contains("quote"), "a made-up quote");
        assert!(unbacked(&proposed("Arjun Shah", "arjun@gmail.com", 1, "email arjun.shah@oldmail.in"), &e).unwrap().contains("value"), "a value its quote doesn't hold");
        assert!(unbacked(&proposed("Arjun Shah", "x", 9, "x"), &e).unwrap().contains("isn't in this errand"));
    }

    #[test]
    fn two_sources_that_disagree_are_a_conflict_the_person_resolves() {
        let m = Memory::in_memory().unwrap();
        let contacts = Origin::Ledger { errand: "errand-1".into(), seq: 1 };
        let his_email = Origin::Ledger { errand: "errand-1".into(), seq: 2 };
        assert!(matches!(m.add_fact("Arjun Shah", "email", "arjun.shah@oldmail.in", "q", &contacts, 0).unwrap(), Added::New(_)));
        assert!(matches!(m.add_fact("arjun shah", "Email", "arjun.shah@oldmail.in", "q", &contacts, 2).unwrap(), Added::Confirmed(_)), "the same fact again");
        m.add_fact("Kestrel interview", "time", "Thursday 13:00 Kestrel interview (video)", "q", &contacts, 0).unwrap();
        assert!(matches!(m.add_fact("Kestrel interview", "time", "Thursday 13:00", "q", &contacts, 1).unwrap(), Added::Confirmed(_)), "worded differently, not a disagreement");
        m.add_fact("Vikram Bose", "Q3 deadline", "Friday", "q", &contacts, 0).unwrap();
        assert!(matches!(m.add_fact("Vikram Bose", "Q3 deadline", "Thursday now, not Friday", "q", &his_email, 2).unwrap(), Added::Conflict(_)), "a change is not a confirmation");
        let Added::Conflict(ids) = m.add_fact("Arjun Shah", "email", "arjun.shah@newco.in", "q", &his_email, 2).unwrap() else { panic!() };
        assert_eq!(ids.len(), 2);
        let recalled = recall(&m, &[], "email Arjun");
        assert!(recalled.contains("UNRESOLVED Arjun Shah · email") && recalled.contains("newco") && recalled.contains("oldmail"), "{recalled}");
        let kept = m.resolve(ids[1], "Asha").unwrap();
        assert_eq!((kept.status, kept.value.as_str()), (Status::Active, "arjun.shah@newco.in"));
        assert_eq!(m.stats().unwrap(), Stats { active: 2, conflicts: 2, retired: 1, open_threads: 0, done_threads: 0 });
        // Rolling the old one back brings the disagreement back, not a silent swap.
        assert_eq!(m.restore(ids[0]).unwrap().status, Status::Conflict);
        assert_eq!(m.stats().unwrap().conflicts, 4, "Arjun's email, and Vikram's deadline");
        // Forgetting where one side came from leaves no one-sided conflict behind.
        m.forget_source("errand-1", 2).unwrap();
        let left: Vec<Fact> = m.facts().unwrap().into_iter().filter(|f| f.subject.starts_with("Arjun") && f.status != Status::Retired).collect();
        assert_eq!((left.len(), left[0].status), (1, Status::Active), "{left:?}");
    }

    #[test]
    fn forgetting_an_entry_forgets_what_was_learned_from_it() {
        let m = Memory::in_memory().unwrap();
        let read = Origin::Ledger { errand: "errand-1".into(), seq: 2 };
        let reply = Origin::Ledger { errand: "errand-2".into(), seq: 1 };
        m.add_fact("Neha Iyer", "interview prep", "Wednesday evening", "q", &read, 1).unwrap();
        let t = m.open_thread("Waiting for Priya to confirm lunch", &read, 1).unwrap();
        let u = m.open_thread("Lunch moved to Friday", &Origin::Ledger { errand: "errand-1".into(), seq: 3 }, 1).unwrap();
        m.close_thread(u, &reply, 2).unwrap();
        assert_eq!(m.forget_source("errand-1", 2).unwrap(), (1, 1));
        assert!(m.facts().unwrap().is_empty() && !m.threads().unwrap().iter().any(|x| x.id == t));
        m.forget_source("errand-2", 1).unwrap();
        assert_eq!(m.threads().unwrap()[0].status, ThreadStatus::Open, "what showed it done is gone");
    }

    #[test]
    fn facts_expire_unless_confirmed_and_the_store_is_bounded() {
        let m = Memory::in_memory().unwrap();
        let o = Origin::Person { who: "Asha".into() };
        m.add_fact("Gym", "day", "Saturday", "q", &o, 0).unwrap();
        m.add_fact("Rent", "due", "the 5th", "q", &o, 0).unwrap();
        m.add_fact("Rent", "due", "the 5th", "q", &o, 25).unwrap();
        assert_eq!(m.consolidate(40).unwrap(), (1, 0));
        let s = m.stats().unwrap();
        assert_eq!((s.active, s.retired), (1, 1), "the confirmed one lives");
        for i in 0..=MAX_FACTS {
            m.add_fact(&format!("thing {i}"), "x", "y", "q", &o, 40).unwrap();
        }
        let (_, over) = m.consolidate(40).unwrap();
        assert_eq!((over, m.stats().unwrap().active), (2, MAX_FACTS));
        // That day's growth is far past the alert line.
        assert_eq!(m.record_growth(39).unwrap(), None, "no earlier day to compare with");
        m.add_fact("one more", "x", "y", "q", &o, 41).unwrap();
        assert!(m.record_growth(41).unwrap().is_none(), "one fact a day is fine");
        assert_eq!(m.growth().unwrap().len(), 2);
    }

    #[test]
    fn recall_cites_every_line_and_shows_earlier_acts_and_open_threads() {
        let e = entries();
        let m = Memory::in_memory().unwrap();
        m.open_thread("Waiting for Priya to confirm lunch", &Origin::Ledger { errand: "errand-aaaabbbbccccdddd".into(), seq: 3 }, 0).unwrap();
        let earlier = [Errand { id: "errand-aaaabbbbccccdddd", day: 0, entries: &e }];
        let text = recall(&m, &earlier, "Did Priya reply about lunch?");
        assert!(text.contains("[aaaabbbb#3, Mon] email.send · Priya Menon"), "{text}");
        assert!(text.contains("(thread 1) since Mon: Waiting for Priya"), "{text}");
        assert!(text.lines().filter(|l| l.starts_with("- ")).all(|l| l.contains('[')), "every line cites: {text}");
        assert_eq!(recall(&Memory::in_memory().unwrap(), &[], "hello"), "");
    }

    #[test]
    fn the_week_is_one_list_with_a_way_back() {
        let e = entries();
        let both = [Errand { id: "errand-a", day: 0, entries: &e }, Errand { id: "errand-b", day: 1, entries: &e }];
        let (all, map) = combined(&both);
        assert_eq!(all.iter().map(|x| x.seq).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6]);
        assert_eq!(map[4], ("errand-b".to_string(), 2));
    }
}
