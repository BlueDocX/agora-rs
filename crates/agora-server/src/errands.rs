//! Errands over HTTP: an agent works in Asha's sandbox while the office watches,
//! then every sentence it says about its work is checked against the ledger.
//!
//! - `POST /api/errands {"brief", "model"?, "agent"?}` starts one (at most
//!   [`MAX_ACTIVE`] at once) and answers `{"id"}`.
//! - `GET /api/errands/{id}/events` streams it as Server-Sent Events: `started`,
//!   each ledger `step` as the executor writes it, what the agent `said`, the
//!   `checked` receipts for it, then `idle`. The stream stays open for questions.
//! - `POST /api/errands/{id}/ask {"question"}` asks the agent about the errand
//!   later, in a fresh conversation (see `agora_engine::errand::ask_later`), and
//!   checks that answer the same way.
//! - `GET /api/errands/{id}/ledger` gives the entries and whether the chain verifies.
//!
//! Each errand has its own world, ledger (in the journal file when there is one) and
//! budget ([`ERRAND_CAP_USD`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use agora_core::World;
use agora_engine::alibi::{self, Audit};
use agora_engine::errand::{self, Errand};
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
    Started { id: String, brief: String, model: String, agent: String, checker: Option<String> },
    /// The agent is working (turn 0) or answering a question (turn 1, 2…).
    Busy { turn: usize, question: Option<String> },
    Step { entry: Entry },
    /// What the agent told the person: the errand's report (turn 0) or an answer.
    Said { turn: usize, question: Option<String>, message: String, finished: bool },
    /// The receipts for that message. `agreed[i]` says whether the second checker
    /// reached the same verdict on claim `i` (`None` = no second checker).
    Checked { turn: usize, audit: Audit, agreed: Vec<Option<bool>>, chain: Chain },
    Failed { turn: usize, reason: String },
    Idle,
}

pub(crate) struct ErrandRoom {
    id: String,
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

    fn chain(&self) -> Chain {
        let entries = self.ledger.entries().unwrap_or_default();
        let verified = self.ledger.verify();
        Chain {
            entries: entries.len(),
            intact: verified.is_ok(),
            head: entries.last().map(|e| e.hash.clone()),
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

/// Check `message` against the ledger: parse it with the office model, and with the
/// second checker when there is one, then judge in plain code.
async fn check(hub: &Hub, room: &ErrandRoom, turn: usize, message: &str) {
    let entries = match room.ledger.entries() {
        Ok(e) => e,
        Err(e) => return room.push(ErrandEvent::Failed { turn, reason: e.to_string() }),
    };
    let second = hub.second_checker();
    let first = alibi::extract(&room.caller, &hub.config.model, message);
    let (first, other) = match &second {
        Some(model) => {
            let (a, b) = tokio::join!(first, alibi::extract(&room.caller, model, message));
            (a, Some(b))
        }
        None => (first.await, None),
    };
    let claims = match first {
        Ok(c) => c,
        Err(e) => return room.push(ErrandEvent::Failed { turn, reason: format!("could not check the message: {e}") }),
    };
    let audit = alibi::audit(message, &claims, &entries);
    let agreed = match other {
        Some(Ok(second)) => alibi::agreement(&audit, message, &second, &entries).into_iter().map(Some).collect(),
        // A second checker that failed can't confirm anything: mark every claim unconfirmed.
        Some(Err(_)) | None => vec![None; audit.checked.len()],
    };
    room.push(ErrandEvent::Checked { turn, audit, agreed, chain: room.chain() });
}

#[derive(Deserialize)]
pub(crate) struct StartErrand {
    brief: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    agent: Option<String>,
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
    let room = Arc::new(ErrandRoom {
        id: id.clone(),
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
    room.push(ErrandEvent::Started { id: id.clone(), brief, model, agent, checker });
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
            room.push(ErrandEvent::Said { turn: 0, question: None, message: message.clone(), finished: done.finished });
            room.lock().report = Some(message.clone());
            check(&hub, &room, 0, &message).await;
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
        let answer = {
            let world = room.world.lock().await;
            errand::ask_later(&room.caller, &world, &errand, &report, &question).await
        };
        match answer {
            Ok(message) => {
                room.push(ErrandEvent::Said { turn, question: Some(question), message: message.clone(), finished: true });
                check(&hub, &room, turn, &message).await;
            }
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
