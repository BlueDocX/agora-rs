//! A durable record of every completed model reply, so a run that dies can resume
//! without paying for the same calls twice.
//!
//! Each reply is stored under (run, hash of the exact request body, occurrence).
//! Prompts are built only from the brief and earlier replies, so a resumed run asks
//! the same questions in the same order: everything that finished is replayed from
//! here, and the run continues from the first call that never completed. The
//! occurrence number keeps identical requests apart (the two extra reviews in a vote
//! are byte-for-byte the same request).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agora_muse::{RateLimit, Request, Response, Usage};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct JournalError(String);

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "run journal: {}", self.0)
    }
}

impl std::error::Error for JournalError {}

impl From<rusqlite::Error> for JournalError {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.to_string())
    }
}

/// A stored run: what to rerun when resuming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub id: String,
    pub brief: String,
    pub model: String,
    /// The reviewer's model on its own host, if the run used one.
    pub reviewer: Option<String>,
}

/// One run's view of the journal file.
#[derive(Debug)]
pub struct Journal {
    db: Mutex<Connection>,
    run_id: String,
    /// How many times each request key has been asked in this process.
    seen: Mutex<HashMap<String, u32>>,
    replayed: AtomicU32,
    write_failures: AtomicU32,
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn open(path: &Path) -> Result<Connection, JournalError> {
    let db = Connection::open(path)?;
    db.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS runs (
             id TEXT PRIMARY KEY, brief TEXT NOT NULL, model TEXT NOT NULL, created_at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS calls (
             run_id TEXT NOT NULL, key TEXT NOT NULL, n INTEGER NOT NULL,
             response_id TEXT NOT NULL, model TEXT NOT NULL, text TEXT NOT NULL,
             input_tokens INTEGER NOT NULL, cached_tokens INTEGER NOT NULL,
             output_tokens INTEGER NOT NULL, reasoning_tokens INTEGER NOT NULL,
             created_at INTEGER NOT NULL,
             PRIMARY KEY (run_id, key, n));",
    )?;
    // Journals from before the reviewer was recorded: add the column (empty = none).
    let has_reviewer: bool =
        db.query_row("SELECT COUNT(*) > 0 FROM pragma_table_info('runs') WHERE name = 'reviewer'", [], |r| r.get(0))?;
    if !has_reviewer {
        db.execute_batch("ALTER TABLE runs ADD COLUMN reviewer TEXT NOT NULL DEFAULT ''")?;
    }
    Ok(db)
}

/// Hex SHA-256 of the request exactly as it is sent.
fn request_key(request: &Request) -> String {
    let digest = Sha256::digest(request.to_body().to_string().as_bytes());
    digest.iter().fold(String::with_capacity(64), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

fn as_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn as_u64(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

impl Journal {
    /// Record a new run in the journal file at `path` (created if missing).
    ///
    /// # Errors
    /// If the file can't be opened or written.
    pub fn start(path: &Path, brief: &str, model: &str, reviewer: Option<&str>) -> Result<Self, JournalError> {
        let db = open(path)?;
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let run_id = format!("run-{nanos:x}");
        db.execute("INSERT INTO runs (id, brief, model, reviewer, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![
            run_id,
            brief,
            model,
            reviewer.unwrap_or(""),
            now_secs()
        ])?;
        Ok(Self::with(db, run_id))
    }

    /// Reopen a run to resume it. Returns the journal and what the run was asked.
    ///
    /// # Errors
    /// If the file can't be opened or has no run with this id.
    pub fn resume(path: &Path, run_id: &str) -> Result<(Self, RunRecord), JournalError> {
        // Don't create an empty journal for a mistyped path.
        if !path.is_file() {
            return Err(JournalError(format!("no journal at {}", path.display())));
        }
        let db = open(path)?;
        let record = db
            .query_row("SELECT id, brief, model, reviewer FROM runs WHERE id = ?1", params![run_id], |r| {
                let reviewer: String = r.get(3)?;
                Ok(RunRecord {
                    id: r.get(0)?,
                    brief: r.get(1)?,
                    model: r.get(2)?,
                    reviewer: (!reviewer.is_empty()).then_some(reviewer),
                })
            })
            .optional()?
            .ok_or_else(|| JournalError(format!("no run \"{run_id}\" in {}", path.display())))?;
        Ok((Self::with(db, record.id.clone()), record))
    }

    fn with(db: Connection, run_id: String) -> Self {
        Self {
            db: Mutex::new(db),
            run_id,
            seen: Mutex::new(HashMap::new()),
            replayed: AtomicU32::new(0),
            write_failures: AtomicU32::new(0),
        }
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// How many calls this process answered from the journal instead of the model.
    #[must_use]
    pub fn replayed(&self) -> u32 {
        self.replayed.load(Ordering::Relaxed)
    }

    /// Replies that could not be stored (they were used, but can't be replayed).
    #[must_use]
    pub fn write_failures(&self) -> u32 {
        self.write_failures.load(Ordering::Relaxed)
    }

    /// Claim the next occurrence of `request`: its key and occurrence number.
    pub(crate) fn claim(&self, request: &Request) -> (String, u32) {
        let key = request_key(request);
        let mut seen = self.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = seen.entry(key.clone()).or_insert(0);
        *n += 1;
        (key, *n)
    }

    /// The stored reply for this occurrence, if the run already got one.
    ///
    /// # Errors
    /// If the journal can't be read.
    pub(crate) fn replay(&self, key: &str, n: u32) -> Result<Option<Response>, JournalError> {
        let db = self.db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let found = db
            .query_row(
                "SELECT response_id, model, text, input_tokens, cached_tokens, output_tokens, reasoning_tokens
                 FROM calls WHERE run_id = ?1 AND key = ?2 AND n = ?3",
                params![self.run_id, key, n],
                |r| {
                    Ok(Response {
                        id: r.get(0)?,
                        model: r.get(1)?,
                        status: "completed".into(),
                        text: r.get(2)?,
                        usage: Usage {
                            input_tokens: as_u64(r.get(3)?),
                            cached_tokens: as_u64(r.get(4)?),
                            output_tokens: as_u64(r.get(5)?),
                            reasoning_tokens: as_u64(r.get(6)?),
                        },
                        rate_limit: RateLimit::default(),
                    })
                },
            )
            .optional()?;
        if found.is_some() {
            self.replayed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(found)
    }

    /// Store a completed reply before anything acts on it. A failed write is counted
    /// in [`Self::write_failures`], never raised: the reply is still good to use.
    pub(crate) fn record(&self, key: &str, n: u32, response: &Response) {
        let u = response.usage;
        let written = self.db.lock().unwrap_or_else(std::sync::PoisonError::into_inner).execute(
            "INSERT OR REPLACE INTO calls VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                self.run_id,
                key,
                n,
                response.id,
                response.model,
                response.text,
                as_i64(u.input_tokens),
                as_i64(u.cached_tokens),
                as_i64(u.output_tokens),
                as_i64(u.reasoning_tokens),
                now_secs()
            ],
        );
        if written.is_err() {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_muse::Message;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("agora-journal-{name}-{nanos}.db"))
    }

    fn response(text: &str) -> Response {
        Response {
            id: "r1".into(),
            model: "muse-spark-1.3".into(),
            status: "completed".into(),
            text: text.into(),
            usage: Usage { input_tokens: 10, cached_tokens: 4, output_tokens: 3, reasoning_tokens: 1 },
            rate_limit: RateLimit::default(),
        }
    }

    #[test]
    fn identical_requests_are_kept_apart_and_survive_a_reopen() {
        let path = temp_path("reopen");
        let request = Request::new("muse-spark-1.3", vec![Message::user("review this")]);
        let run_id = {
            let journal = Journal::start(&path, "brief", "muse-spark-1.3", None).unwrap();
            for text in ["first vote", "second vote"] {
                let (key, n) = journal.claim(&request);
                journal.record(&key, n, &response(text));
            }
            journal.run_id().to_string()
        };

        let (journal, record) = Journal::resume(&path, &run_id).unwrap();
        assert_eq!((record.brief.as_str(), record.reviewer), ("brief", None));
        let replies: Vec<String> = (0..3)
            .map(|_| {
                let (key, n) = journal.claim(&request);
                journal.replay(&key, n).unwrap().map(|r| r.text).unwrap_or_default()
            })
            .collect();
        assert_eq!(replies, ["first vote", "second vote", ""]);
        assert_eq!(journal.replayed(), 2);
        assert_eq!(journal.replay(&request_key(&request), 1).unwrap().unwrap().usage, response("").usage);
        remove_db(&path);
    }

    fn remove_db(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
        }
    }

    #[test]
    fn a_journal_from_before_reviewers_were_recorded_still_resumes() {
        let path = temp_path("old");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE runs (id TEXT PRIMARY KEY, brief TEXT NOT NULL, model TEXT NOT NULL, created_at INTEGER NOT NULL);
                 INSERT INTO runs VALUES ('run-old', 'old brief', 'muse-spark-1.3', 0);",
            )
            .unwrap();
        let (_, record) = Journal::resume(&path, "run-old").unwrap();
        assert_eq!((record.brief.as_str(), record.reviewer), ("old brief", None));
        remove_db(&path);
    }

    #[test]
    fn resuming_an_unknown_run_says_so_and_a_missing_file_is_not_created() {
        let path = temp_path("unknown");
        let err = Journal::resume(&path, "run-nope").unwrap_err();
        assert!(err.to_string().contains("no journal at"), "{err}");
        assert!(!path.exists());

        Journal::start(&path, "brief", "muse-spark-1.3", None).unwrap();
        let err = Journal::resume(&path, "run-nope").unwrap_err();
        assert!(err.to_string().contains("no run \"run-nope\""), "{err}");
        remove_db(&path);
    }
}
