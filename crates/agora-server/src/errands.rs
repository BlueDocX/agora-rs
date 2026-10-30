//! Errands over HTTP: an agent works in Asha's sandbox while the office watches,
//! then every sentence it says about its work is checked against the ledger.
//!
//! - `POST /api/errands {"brief", "model"?, "agent"?}` starts one (at most
//!   [`MAX_ACTIVE`] at once) and answers `{"id"}`.
//! - `GET /api/errands/{id}/events` streams it as Server-Sent Events: `started`,
//!   each ledger `step` as the executor writes it, each `draft` of the agent's
//!   message as the guard judges it, what was `said` (delivered, with its receipts
//!   and any correction from the ledger), then `idle`. It stays open for questions.
//! - With the guard on (the default; `"guard": false` turns it off to compare), no
//!   message reaches the person with a claim the ledger contradicts: see
//!   `agora_engine::guard`.
//! - `POST /api/errands/{id}/ask {"question"}` asks the agent about the errand
//!   later, in a fresh conversation (see `agora_engine::errand::ask_later`), and
//!   checks that answer the same way.
//! - `GET /api/errands/{id}/ledger` gives the entries and whether the chain verifies.
//! - `POST /api/errands/{id}/forget {"seq"}` forgets what one call read or did: its
//!   ledger content is deleted (the chain still verifies), every message the agent
//!   wrote after it is withdrawn (it may repeat what was read), and the stored state
//!   is scanned for any copy left. The stream's history is rewritten, so watchers
//!   reconnect to see it.
//!
//! Each errand has its own world, ledger (in the journal file when there is one) and
//! budget ([`ERRAND_CAP_USD`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use agora_core::World;
use agora_engine::alibi::Audit;
use agora_engine::guard::{self, Checked, Checkers, Guarded};
use agora_engine::errand::{self, Errand};
use agora_engine::ledger::Receipt;
use agora_engine::{Budget, Caller, Entry, Ledger};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{Hub, authorized, error, to_sse, unauthorized};

pub const MAX_ACTIVE: usize = 2;
/// What one errand (its work, its checks and every question about it) may spend.
pub const ERRAND_CAP_USD: f64 = 0.75;
const KEEP_ERRANDS: usize = 30;
const MAX_ERRAND_CHARS: usize = 500;
const MAX_QUESTION_CHARS: usize = 300;
const DEFAULT_AGENT: &str = "Scout";

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Chain {
    pub entries: usize,
    pub intact: bool,
    /// The newest entry's hash (what anyone can pin to check the ledger later).
    pub head: Option<String>,
    /// Where the chain breaks, if it does.
    pub broken: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ErrandEvent {
    Started { id: String, brief: String, model: String, agent: String, checker: Option<String>, guard: bool },
    /// The agent is working (turn 0) or answering a question (turn 1, 2…).
    Busy { turn: usize, question: Option<String> },
    Step { entry: Entry },
    /// A draft of the agent's message (round 0 is the first), judged before delivery.
    Draft { turn: usize, round: usize, check: Checked },
    /// What reached the person: the errand's report (turn 0) or an answer. `check`
    /// is the delivered text with its receipts (`check.agreed[i]`: whether the second
    /// checker agrees on claim `i`); `correction` was added from the ledger by code.
    Said {
        turn: usize,
        question: Option<String>,
        message: String,
        correction: Option<String>,
        finished: bool,
        check: Checked,
        /// How many times the agent rewrote it before delivery.
        rounds: usize,
        chain: Chain,
        spent_usd: f64,
    },
    Failed { turn: usize, reason: String },
    /// Entry `receipt.seq` was forgotten; these turns' messages were withdrawn.
    Forgotten { receipt: Receipt, withdrawn: Vec<usize>, scan: Scan },
    Idle,
}

/// The search for any copy of forgotten content left in what the office holds for
/// this errand: its stream history and its other ledger entries.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Scan {
    /// Distinct pieces of the forgotten content searched for.
    pub needles: usize,
    pub bytes: usize,
    /// Where a copy is still held (e.g. "entry #3 messages.send"), if anywhere.
    pub found_in: Vec<String>,
}

const WITHDRAWN: &str = "[Withdrawn: written after reading something you asked to forget.]";

pub(crate) struct ErrandRoom {
    id: String,
    /// Whether messages go through the guard before delivery.
    guard: bool,
    brief: String,
    model: String,
    agent: String,
    caller: Caller,
    world: tokio::sync::Mutex<World>,
    ledger: Ledger,
    inner: Mutex<Inner>,
}

struct Inner {
    history: Vec<ErrandEvent>,
    live: broadcast::Sender<ErrandEvent>,
    busy: bool,
    /// The agent's report, which every later question starts from.
    report: Option<String>,
    turns: usize,
}

#[derive(Default)]
pub(crate) struct Errands {
    rooms: Vec<Arc<ErrandRoom>>,
}

impl ErrandRoom {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push(&self, event: ErrandEvent) {
        let mut inner = self.lock();
        let _ = inner.live.send(event.clone());
        inner.history.push(event);
    }

    /// The chain as it stands, checked against the newest head this errand has
    /// streamed (so entries dropped from the end show as broken too).
    fn chain(&self) -> Chain {
        let published = self.lock().history.iter().rev().find_map(|e| match e {
            ErrandEvent::Step { entry } => Some(entry.hash.clone()),
            _ => None,
        });
        let entries = self.ledger.entries();
        let head = entries.as_ref().ok().and_then(|e| e.last()).map(|e| e.hash.clone());
        let verified = match &published {
            Some(h) => self.ledger.verify_head(h),
            None => self.ledger.verify(),
        };
        Chain {
            entries: entries.map_or(0, |e| e.len()),
            intact: verified.is_ok(),
            head,
            broken: verified.err().map(|b| format!("entry {}: {}", b.seq, b.reason)),
        }
    }
}

impl Hub {
    fn errands(&self) -> std::sync::MutexGuard<'_, Errands> {
        self.errands.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn errand(&self, id: &str) -> Option<Arc<ErrandRoom>> {
        self.errands().rooms.iter().find(|r| r.id == id).cloned()
    }

    /// The second checker: Spark Open on Baseten when the office has it and the
    /// first checker isn't already on it.
    fn second_checker(&self) -> Option<String> {
        let open = "meta-models/muse-spark-open-1026";
        (self.config.baseten.is_some() && self.config.model != open).then(|| open.to_string())
    }
}

/// Judge the agent's `message` and deliver what may reach the person: through the
/// guard (checked, revised, corrected from the ledger if needed), or with the guard
/// off, checked only. Every draft is streamed as it is judged.
async fn deliver(hub: &Hub, room: &ErrandRoom, turn: usize, question: Option<String>, message: &str, finished: bool) {
    let entries = match room.ledger.entries() {
        Ok(e) => e,
        Err(e) => return room.push(ErrandEvent::Failed { turn, reason: e.to_string() }),
    };
    let second = hub.second_checker();
    let checkers = Checkers { first: &hub.config.model, second: second.as_deref() };
    let on_draft = |round: usize, check: &Checked| room.push(ErrandEvent::Draft { turn, round, check: check.clone() });
    let owner = room.world.lock().await.owner;
    let guarded = if room.guard {
        guard::deliver(&room.caller, checkers, &room.model, owner, message, &entries, &on_draft).await
    } else {
        guard::check(&room.caller, checkers, message, &entries).await.map(|check| {
            on_draft(0, &check);
            Guarded { drafts: vec![check.clone()], delivered: check, correction: None }
        })
    };
    // If the guard can't run, the person still gets the ledger's own account (and
    // the errand can still be asked about), never the agent's unchecked words.
    let guarded = guarded.or_else(|e| {
        if room.guard {
            room.push(ErrandEvent::Failed { turn, reason: format!("the guard couldn't check the message ({e}); delivering the ledger's account instead") });
            Ok(guard::withheld(&entries))
        } else {
            Err(e)
        }
    });
    match guarded {
        Ok(g) => {
            let told = match &g.correction {
                Some(c) if g.delivered.message.is_empty() => c.clone(),
                Some(c) => format!("{}\n\n{c}", g.delivered.message),
                None => g.delivered.message.clone(),
            };
            room.lock().report.get_or_insert(told);
            room.push(ErrandEvent::Said {
                turn,
                question,
                message: g.delivered.message.clone(),
                correction: g.correction,
                finished,
                rounds: g.drafts.len().saturating_sub(1),
                check: g.delivered,
                chain: room.chain(),
                spent_usd: room.caller.spend().cost_usd,
            });
        }
        Err(e) => room.push(ErrandEvent::Failed { turn, reason: format!("could not check the message: {e}") }),
    }
}

#[derive(Deserialize)]
pub(crate) struct StartErrand {
    brief: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    /// `false` turns the guard off (to see what the agent says unchecked). Default on.
    #[serde(default)]
    guard: Option<bool>,
}

#[derive(Serialize)]
struct Started {
    id: String,
}

fn new_id() -> String {
    format!("errand-{:x}", SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()))
}

pub(crate) async fn start(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Result<Json<StartErrand>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers, None) {
        return unauthorized();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that request isn't a valid errand: {}", e.body_text())),
    };
    let brief = body.brief.trim().to_string();
    if brief.is_empty() || brief.chars().count() > MAX_ERRAND_CHARS {
        return error(StatusCode::BAD_REQUEST, "the errand must be 1 to 500 characters");
    }
    let agent = body.agent.map_or_else(|| DEFAULT_AGENT.to_string(), |a| a.trim().to_string());
    if agent.is_empty() || agent.chars().count() > 40 || agent.chars().any(char::is_control) {
        return error(StatusCode::BAD_REQUEST, "the agent's name must be 1 to 40 characters on one line");
    }
    let model = body.model.unwrap_or_else(|| hub.config.model.clone());
    if let Err(why) = hub.config.check_model(&model) {
        return error(StatusCode::BAD_REQUEST, &why);
    }
    let checker = hub.second_checker();
    let priced: Vec<&str> = [model.as_str(), hub.config.model.as_str()].into_iter().chain(checker.as_deref()).collect();
    let budget = match Budget::new(&priced, ERRAND_CAP_USD) {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let id = new_id();
    let ledger = match &hub.config.journal {
        Some(path) => {
            let (path, id) = (path.clone(), id.clone());
            tokio::task::spawn_blocking(move || Ledger::open(&path, &id)).await.map_err(|e| e.to_string()).and_then(|l| l.map_err(|e| e.to_string()))
        }
        None => Ledger::in_memory(&id).map_err(|e| e.to_string()),
    };
    let ledger = match ledger {
        Ok(l) => l,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let guard = body.guard.unwrap_or(true);
    let room = Arc::new(ErrandRoom {
        id: id.clone(),
        guard,
        brief: brief.clone(),
        model: model.clone(),
        agent: agent.clone(),
        caller: Caller::new(hub.config.muse.clone(), hub.config.baseten.clone(), budget),
        world: tokio::sync::Mutex::new(World::demo()),
        ledger,
        inner: Mutex::new(Inner { history: Vec::new(), live: broadcast::channel(256).0, busy: true, report: None, turns: 0 }),
    });
    {
        let mut errands = hub.errands();
        if errands.rooms.iter().filter(|r| r.lock().busy).count() >= MAX_ACTIVE {
            return error(StatusCode::TOO_MANY_REQUESTS, "two errands are already running; try again when one finishes");
        }
        errands.rooms.push(Arc::clone(&room));
        while errands.rooms.len() > KEEP_ERRANDS {
            let Some(oldest) = errands.rooms.iter().position(|r| !r.lock().busy) else { break };
            errands.rooms.remove(oldest);
        }
    }
    room.push(ErrandEvent::Started { id: id.clone(), brief, model, agent, checker, guard });
    room.push(ErrandEvent::Busy { turn: 0, question: None });
    tokio::spawn(work(hub, room));
    (StatusCode::CREATED, Json(Started { id })).into_response()
}

/// Run the errand, streaming each ledger entry as it is written, then check the report.
async fn work(hub: Arc<Hub>, room: Arc<ErrandRoom>) {
    let errand = Errand { agent: &room.agent, model: &room.model, brief: &room.brief };
    let done = {
        let mut world = room.world.lock().await;
        let on_step = |entry: &Entry| room.push(ErrandEvent::Step { entry: entry.clone() });
        errand::run(&room.caller, &room.ledger, &mut world, &errand, &on_step).await
    };
    match done {
        Ok(done) => {
            let message = if done.message.is_empty() { "(The agent ran out of steps without reporting.)".to_string() } else { done.message };
            deliver(&hub, &room, 0, None, &message, done.finished).await;
        }
        Err(e) => room.push(ErrandEvent::Failed { turn: 0, reason: e.to_string() }),
    }
    finish(&room);
}

/// Free the errand for questions and say so, under one lock.
fn finish(room: &ErrandRoom) {
    let mut inner = room.lock();
    inner.busy = false;
    let _ = inner.live.send(ErrandEvent::Idle);
    inner.history.push(ErrandEvent::Idle);
}

#[derive(Deserialize)]
pub(crate) struct Ask {
    question: String,
}

pub(crate) async fn ask(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap, body: Result<Json<Ask>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers, None) {
        return unauthorized();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that isn't a valid question: {}", e.body_text())),
    };
    let question = body.question.trim().to_string();
    if question.is_empty() || question.chars().count() > MAX_QUESTION_CHARS {
        return error(StatusCode::BAD_REQUEST, "the question must be 1 to 300 characters");
    }
    let Some(room) = hub.errand(&id) else { return error(StatusCode::NOT_FOUND, "no such errand") };
    let (turn, report) = {
        let mut inner = room.lock();
        let Some(report) = inner.report.clone() else {
            return error(StatusCode::CONFLICT, "the errand hasn't reported yet");
        };
        if inner.busy {
            return error(StatusCode::TOO_MANY_REQUESTS, "the agent is still answering");
        }
        inner.busy = true;
        inner.turns += 1;
        (inner.turns, report)
    };
    room.push(ErrandEvent::Busy { turn, question: Some(question.clone()) });
    tokio::spawn(async move {
        let errand = Errand { agent: &room.agent, model: &room.model, brief: &room.brief };
        // With the guard on, the agent answers from its ledger, not from memory.
        let log = room.guard.then(|| guard::log(&room.ledger.entries().unwrap_or_default()));
        let answer = {
            let world = room.world.lock().await;
            errand::ask_later(&room.caller, &world, &errand, &report, &question, log.as_deref()).await
        };
        match answer {
            Ok(message) => deliver(&hub, &room, turn, Some(question), &message, true).await,
            Err(e) => room.push(ErrandEvent::Failed { turn, reason: e.to_string() }),
        }
        finish(&room);
    });
    StatusCode::ACCEPTED.into_response()
}

pub(crate) async fn events(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap, Query(query): Query<HashMap<String, String>>) -> Response {
    if !authorized(&hub, &headers, query.get("token").map(String::as_str)) {
        return unauthorized();
    }
    let Some(room) = hub.errand(&id) else { return error(StatusCode::NOT_FOUND, "no such errand") };
    let (history, live) = {
        let inner = room.lock();
        (inner.history.clone(), inner.live.subscribe())
    };
    let past = stream::iter(history).filter_map(|e| async move { to_sse(&e) });
    let live = stream::unfold(live, |mut rx| async move { rx.recv().await.ok().map(|e| (e, rx)) }).filter_map(|e| async move { to_sse(&e) });
    Sse::new(past.chain(live)).keep_alive(KeepAlive::default()).into_response()
}

#[derive(Serialize)]
struct LedgerView {
    errand: String,
    entries: Vec<Entry>,
    chain: Chain,
}

pub(crate) async fn ledger(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers, None) {
        return unauthorized();
    }
    let Some(room) = hub.errand(&id) else { return error(StatusCode::NOT_FOUND, "no such errand") };
    match room.ledger.entries() {
        Ok(entries) => Json(LedgerView { errand: room.id.clone(), entries, chain: room.chain() }).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub(crate) struct Forget {
    seq: u64,
}

/// The pieces of an entry's content worth searching for: the lines of what it
/// returned and what it was called on, long enough not to match by chance.
fn needles(entry: &Entry) -> Vec<String> {
    let a = &entry.action;
    let mut found: Vec<String> = [entry.outcome.result.as_str(), a.text.as_str()]
        .iter()
        .flat_map(|t| t.split(['\n', '.', ';']))
        .chain([entry.outcome.subject.as_str()])
        .map(str::trim)
        .filter(|t| t.chars().count() >= 12)
        .map(str::to_string)
        .collect();
    found.sort();
    found.dedup();
    found
}

pub(crate) async fn forget(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap, body: Result<Json<Forget>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers, None) {
        return unauthorized();
    }
    let Json(Forget { seq }) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that isn't a valid request: {}", e.body_text())),
    };
    let Some(room) = hub.errand(&id) else { return error(StatusCode::NOT_FOUND, "no such errand") };
    {
        // Hold the errand while forgetting, so no question can start from the old report.
        let mut inner = room.lock();
        if inner.busy {
            return error(StatusCode::TOO_MANY_REQUESTS, "the agent is busy; forget when it's done");
        }
        inner.busy = true;
    }
    let release = || room.lock().busy = false;
    let Some(before) = room.ledger.entries().ok().and_then(|e| e.into_iter().find(|e| e.seq == seq)) else {
        release();
        return error(StatusCode::NOT_FOUND, "no such ledger entry");
    };
    let receipt = match room.ledger.forget(seq) {
        Ok(Some(r)) => r,
        Ok(None) => {
            release();
            return error(StatusCode::NOT_FOUND, "no such ledger entry");
        }
        Err(e) => {
            release();
            return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
    };
    let after = room.ledger.entries().unwrap_or_default();
    let forgotten = after.iter().find(|e| e.seq == seq).cloned();
    let event = {
        let mut inner = room.lock();
        // Everything the agent said came after its calls, so every message is withdrawn.
        let mut withdrawn = Vec::new();
        for e in &mut inner.history {
            match e {
                ErrandEvent::Step { entry } if entry.seq == seq => {
                    if let Some(f) = &forgotten {
                        entry.clone_from(f);
                    }
                }
                ErrandEvent::Said { turn, message, check, correction, .. } => {
                    if message != WITHDRAWN {
                        withdrawn.push(*turn);
                    }
                    message.clear();
                    message.push_str(WITHDRAWN);
                    *check = withdrawn_check();
                    // The correction is the ledger's own account: write it again from
                    // the ledger as it now stands.
                    if correction.is_some() {
                        *correction = Some(guard::correction(&after));
                    }
                }
                ErrandEvent::Draft { check, .. } => *check = withdrawn_check(),
                _ => {}
            }
        }
        if inner.report.is_some() {
            inner.report = Some(WITHDRAWN.to_string());
        }
        // Search what is left for this errand: the stream history, then every other
        // ledger entry (an agent may have repeated what it read in a call it made).
        let wanted = needles(&before);
        // Search the text itself (every string in it), not its JSON encoding, where
        // quotes and backslashes would be escaped and never match.
        let mut places = vec![("the errand's history".to_string(), strings(&serde_json::to_value(&inner.history).unwrap_or_default()))];
        for e in room.ledger.entries().unwrap_or_default() {
            places.push((format!("entry #{} {}", e.seq, e.action.tool.name()), strings(&serde_json::to_value((&e.action, &e.outcome)).unwrap_or_default())));
        }
        let bytes = places.iter().map(|(_, text)| text.len()).sum();
        let found_in = places.into_iter().filter(|(_, text)| wanted.iter().any(|n| text.contains(n.as_str()))).map(|(place, _)| place).collect();
        let scan = Scan { needles: wanted.len(), bytes, found_in };
        let event = ErrandEvent::Forgotten { receipt, withdrawn, scan };
        inner.busy = false;
        let _ = inner.live.send(event.clone());
        inner.history.push(event.clone());
        event
    };
    Json(event).into_response()
}

fn withdrawn_check() -> Checked {
    Checked { message: WITHDRAWN.to_string(), audit: Audit::default(), agreed: Vec::new(), false_claims: Vec::new(), unmentioned: Vec::new() }
}

/// Every string in a JSON value, one per line.
fn strings(value: &serde_json::Value) -> String {
    fn walk(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            serde_json::Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            serde_json::Value::Object(map) => map.values().for_each(|i| walk(i, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    walk(value, &mut out);
    out
}
