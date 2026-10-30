//! What the office remembers across restarts: the people (with only a hash of each
//! key), the activity feed, the messages, and every errand's stream.
//!
//! Writes go to one thread that owns the database, so a request never waits on the
//! disk; the office reads everything back once, at start. Forgetting rewrites an
//! errand's stored stream and folds the write-ahead log into the (securely
//! overwritten) main file before it answers, so no copy of what was forgotten is
//! left in this file either.

use std::path::Path;
use std::sync::mpsc;

use rusqlite::{Connection, params};

pub(crate) enum Op {
    Kv { key: String, value: String },
    Member { id: String, json: String },
    RemoveMember { id: String },
    Activity { json: String },
    /// A message, by its number (posting again replaces it, e.g. when withdrawn).
    Message { n: u64, json: String },
    /// An errand dropped from the office (past its retention): its stream goes.
    DropErrand { id: String },
    Errand { id: String, json: String },
    Event { errand: String, n: usize, json: String },
    /// Replace an errand's whole stream, then say when it is on disk.
    Rewrite { errand: String, events: Vec<String>, done: mpsc::Sender<bool> },
}

#[derive(Debug, Default)]
pub(crate) struct Loaded {
    pub kv: Vec<(String, String)>,
    pub members: Vec<String>,
    pub activity: Vec<String>,
    pub messages: Vec<String>,
    /// Newest last: (id, meta, events).
    pub errands: Vec<(String, String, Vec<String>)>,
    /// Errands past retention, deleted from the store as it opened (their ledgers'
    /// content should be forgotten too).
    pub pruned: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Store {
    tx: mpsc::Sender<Op>,
}

const KEEP_ACTIVITY: i64 = 200;
const KEEP_MESSAGES: i64 = 2000;

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let db = Connection::open(path)?;
    db.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA secure_delete = ON;
         CREATE TABLE IF NOT EXISTS office_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS office_members (id TEXT PRIMARY KEY, json TEXT NOT NULL, at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS office_activity (n INTEGER PRIMARY KEY AUTOINCREMENT, json TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS office_messages (n INTEGER PRIMARY KEY, json TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS office_errands (id TEXT PRIMARY KEY, json TEXT NOT NULL, at INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS office_errand_events (errand TEXT NOT NULL, n INTEGER NOT NULL, json TEXT NOT NULL, PRIMARY KEY (errand, n));",
    )?;
    Ok(db)
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn apply(db: &Connection, op: Op) -> rusqlite::Result<()> {
    match op {
        Op::Kv { key, value } => {
            db.execute("INSERT INTO office_kv VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2", params![key, value])?;
        }
        Op::Member { id, json } => {
            db.execute("INSERT INTO office_members VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET json = ?2", params![id, json, now()])?;
        }
        Op::RemoveMember { id } => {
            db.execute("DELETE FROM office_members WHERE id = ?1", params![id])?;
        }
        Op::Activity { json } => {
            db.execute("INSERT INTO office_activity (json) VALUES (?1)", params![json])?;
            db.execute("DELETE FROM office_activity WHERE n <= (SELECT MAX(n) FROM office_activity) - ?1", params![KEEP_ACTIVITY])?;
        }
        Op::Message { n, json } => {
            db.execute("INSERT OR REPLACE INTO office_messages VALUES (?1, ?2)", params![i64::try_from(n).unwrap_or(i64::MAX), json])?;
            db.execute("DELETE FROM office_messages WHERE n <= (SELECT MAX(n) FROM office_messages) - ?1", params![KEEP_MESSAGES])?;
        }
        Op::DropErrand { id } => {
            db.execute("DELETE FROM office_errand_events WHERE errand = ?1", params![id])?;
            db.execute("DELETE FROM office_errands WHERE id = ?1", params![id])?;
        }
        Op::Errand { id, json } => {
            db.execute("INSERT INTO office_errands VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET json = ?2", params![id, json, now()])?;
        }
        Op::Event { errand, n, json } => {
            db.execute("INSERT OR REPLACE INTO office_errand_events VALUES (?1, ?2, ?3)", params![errand, i64::try_from(n).unwrap_or(i64::MAX), json])?;
        }
        Op::Rewrite { errand, events, done } => {
            let written = (|| {
                db.execute_batch("BEGIN")?;
                db.execute("DELETE FROM office_errand_events WHERE errand = ?1", params![errand])?;
                for (n, json) in events.iter().enumerate() {
                    db.execute("INSERT INTO office_errand_events VALUES (?1, ?2, ?3)", params![errand, i64::try_from(n).unwrap_or(i64::MAX), json])?;
                }
                db.execute_batch("COMMIT")?;
                // The old pages may still be in the write-ahead log: fold it in.
                let busy: i64 = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
                Ok::<bool, rusqlite::Error>(busy == 0)
            })();
            if written.is_err() {
                let _ = db.execute_batch("ROLLBACK");
            }
            let _ = done.send(written.unwrap_or(false));
        }
    }
    Ok(())
}

impl Store {
    /// Open (or create) the store at `path`, read what it holds, and start its writer.
    ///
    /// # Errors
    /// If the file can't be opened or read.
    pub(crate) fn open(path: &Path, keep_errands: usize) -> rusqlite::Result<(Self, Loaded)> {
        let db = open(path)?;
        let all = |sql: &str| -> rusqlite::Result<Vec<String>> { db.prepare(sql)?.query_map([], |r| r.get(0))?.collect() };
        let mut loaded = Loaded {
            kv: db.prepare("SELECT key, value FROM office_kv")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?,
            members: all("SELECT json FROM office_members ORDER BY at, id")?,
            activity: all("SELECT json FROM office_activity ORDER BY n")?,
            messages: all("SELECT json FROM office_messages ORDER BY n")?,
            errands: Vec::new(),
            pruned: Vec::new(),
        };
        let keep = i64::try_from(keep_errands).unwrap_or(i64::MAX);
        // Past retention: delete the stream (the ledger's content is forgotten by the hub).
        loaded.pruned = db.prepare("SELECT id FROM office_errands ORDER BY at DESC, rowid DESC LIMIT -1 OFFSET ?1")?.query_map(params![keep], |r| r.get(0))?.collect::<Result<_, _>>()?;
        for id in &loaded.pruned {
            apply(&db, Op::DropErrand { id: id.clone() })?;
        }
        let errands: Vec<(String, String)> = db
            .prepare("SELECT id, json FROM (SELECT id, json, at, rowid AS r FROM office_errands ORDER BY at DESC, rowid DESC LIMIT ?1) ORDER BY at, r")?
            .query_map(params![keep], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (id, meta) in errands {
            let events = db.prepare("SELECT json FROM office_errand_events WHERE errand = ?1 ORDER BY n")?.query_map(params![id], |r| r.get(0))?.collect::<Result<_, _>>()?;
            loaded.errands.push((id, meta, events));
        }
        let (tx, rx) = mpsc::channel::<Op>();
        std::thread::Builder::new()
            .name("agora-store".into())
            .spawn(move || {
                for op in rx {
                    if let Err(e) = apply(&db, op) {
                        eprintln!("the office store couldn't save a change: {e}");
                    }
                }
            })
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        Ok((Self { tx }, loaded))
    }

    pub(crate) fn send(&self, op: Op) {
        let _ = self.tx.send(op);
    }

    /// Replace an errand's stored stream and wait until it is on disk. True when no
    /// old copy is left in the write-ahead log either. Blocks: call off the async
    /// workers.
    pub(crate) fn rewrite(&self, errand: &str, events: Vec<String>) -> bool {
        let (done, wait) = mpsc::channel();
        self.send(Op::Rewrite { errand: errand.to_string(), events, done });
        wait.recv().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_written_is_read_back_and_a_rewrite_replaces_the_stream() {
        let dir = std::env::temp_dir().join(format!("agora-store-{}", agora_engine::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        {
            let (store, loaded) = Store::open(&path, 30).unwrap();
            assert_eq!(loaded.members.len(), 0);
            store.send(Op::Kv { key: "invite".into(), value: "abc".into() });
            store.send(Op::Errand { id: "e0".into(), json: "{}".into() });
            store.send(Op::Member { id: "m1".into(), json: "{\"id\":\"m1\"}".into() });
            store.send(Op::Errand { id: "e1".into(), json: "{}".into() });
            for n in 0..3 {
                store.send(Op::Event { errand: "e1".into(), n, json: format!("secret {n}") });
            }
            assert!(store.rewrite("e1", vec!["kept".into()]));
        }
        let (_, loaded) = Store::open(&path, 1).unwrap();
        assert_eq!(loaded.pruned, ["e0"], "past retention");
        assert_eq!(loaded.kv, [("invite".to_string(), "abc".to_string())]);
        assert_eq!(loaded.members.len(), 1);
        assert_eq!(loaded.errands, [("e1".to_string(), "{}".to_string(), vec!["kept".to_string()])]);
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.windows(8).any(|w| w == b"secret 1"), "a rewritten stream leaves no copy in the file");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
