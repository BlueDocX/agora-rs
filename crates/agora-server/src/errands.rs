//! Errands over HTTP: an agent works in Asha's sandbox while the office watches,
//! then every sentence it says about its work is checked against the ledger.
//!
//! - `POST /api/errands {"brief", "model"?, "agent"?, "area"?, "bot"?}` starts one (at
//!   most [`MAX_ACTIVE`] at once, within the office's total cap) and answers `{"id"}`.
//!   It runs on the asker's bot, the bot of an area's lead, or a named bot.
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
//! - `GET /api/errands` lists errands, newest first, each with its latest delivered
//!   message and how its receipts came out (for the glasses and for MCP clients).
//! - `GET /api/errands/{id}/ledger` gives the entries and whether the chain verifies.
//! - `POST /api/errands/{id}/forget {"seq"}` forgets what one call read or did: its
//!   ledger content is deleted (the chain still verifies), every message the agent
//!   wrote after it is withdrawn (it may repeat what was read), and the stored state
//!   is scanned for any copy left. The stream's history is rewritten (in memory and
//!   in the office store), so watchers reconnect to see it.
//! - `GET /api/errands/{id}/receipt` is the chain's head, signed by the office: keep
//!   it, and `agora-verify --receipt` can later prove what the office committed to.
//!
//! In a company, only the person who asked, the bot's owner, or the office owner may
//! ask about or forget an errand. Each errand has its own world, ledger (in the
//! journal file when there is one) and budget ([`ERRAND_CAP_USD`]); the office caps
//! what all errands spend together (`errands_cap_usd`). With a store, errands come
//! back after a restart.

use std::sync::{Arc, Mutex};

use agora_core::World;
use agora_engine::alibi::Audit;
use agora_engine::guard::{self, Checked, Checkers, Guarded};
use agora_engine::errand::{self, Errand};
use agora_engine::ledger::Receipt;
use agora_engine::{Budget, Caller, Entry, Ledger, SignedHead, Signer};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::company::{self, Member, Role};
use crate::store::{Op, Store};
use crate::{Hub, authorized, error, to_sse, unauthorized};

pub const MAX_ACTIVE: usize = 4;
/// What one errand (its work, its checks and every question about it) may spend.
pub const ERRAND_CAP_USD: f64 = 0.75;
/// Errands the office keeps. An older one is dropped: its stored stream is deleted and
/// its ledger's content forgotten (the hash chain stays, so it still verifies).
pub(crate) const KEEP_ERRANDS: usize = 200;
const MAX_ERRAND_CHARS: usize = 500;
const MAX_QUESTION_CHARS: usize = 300;
const DEFAULT_AGENT: &str = "Scout";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Chain {
    pub entries: usize,
    pub intact: bool,
    /// The newest entry's hash (what anyone can pin to check the ledger later).
    pub head: Option<String>,
    /// Where the chain breaks, if it does.
    pub broken: Option<String>,
    /// The head, signed by the office (when the chain is intact).
    #[serde(default)]
    pub signed: Option<SignedHead>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ErrandEvent {
    Started {
        id: String,
        brief: String,
        model: String,
        agent: String,
        checker: Option<String>,
        guard: bool,
        people: People,
        /// On the inbox with a planted prompt injection.
        #[serde(default)]
        poisoned: bool,
    },
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
        chain: Box<Chain>,
        spent_usd: f64,
    },
    Failed { turn: usize, reason: String },
    /// Entry `receipt.seq` was forgotten; these turns' messages were withdrawn.
    Forgotten { receipt: Receipt, withdrawn: Vec<usize>, scan: Scan },
    Idle,
}

/// The search for any copy of forgotten content left in what the office holds for
/// this errand: its stream history and its other ledger entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Scan {
    /// Distinct pieces of the forgotten content searched for.
    pub needles: usize,
    pub bytes: usize,
    /// Where a copy is still held (e.g. "entry #3 messages.send"), if anywhere.
    pub found_in: Vec<String>,
    /// Whether the office store's copy of the stream was rewritten with no old copy
    /// left in its write-ahead log (`None` = no store).
    #[serde(default)]
    pub store_cleared: Option<bool>,
}

const WITHDRAWN: &str = "[Withdrawn: written after reading something you asked to forget.]";

/// Who asked for an errand and whose bot runs it: (member id, name[, area]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct People {
    pub asked_by: Option<(String, String)>,
    pub bot_of: Option<(String, String, String)>,
}

/// What the store keeps about an errand besides its stream.
#[derive(Serialize, Deserialize)]
struct Meta {
    people: People,
    persona: String,
    guard: bool,
    brief: String,
    model: String,
    agent: String,
    reply_to: Option<String>,
    #[serde(default)]
    poisoned: bool,
    /// When it was asked for (0 in errands stored before this was kept).
    #[serde(default)]
    at_ms: u64,
}

pub(crate) struct ErrandRoom {
    id: String,
    people: People,
    /// The bot's preset role and skills, as its instructions carry them.
    persona: String,
    /// Whether messages go through the guard before delivery.
    guard: bool,
    brief: String,
    model: String,
    agent: String,
    /// The channel the errand was asked in: its report goes back there.
    reply_to: Option<String>,
    /// Run on the inbox with a planted prompt injection.
    poisoned: bool,
    /// When it was asked for.
    at_ms: u64,
    caller: Caller,
    world: tokio::sync::Mutex<World>,
    ledger: Ledger,
    signer: Arc<Signer>,
    store: Option<Store>,
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
    /// What errands no longer kept in memory spent (since the server started).
    retired_usd: f64,
}

impl Errands {
    /// What errands have spent since the server started, counting each one at work
    /// at its full cap (it may still spend that much).
    fn committed_usd(&self) -> f64 {
        let each = |r: &Arc<ErrandRoom>| if r.lock().busy { ERRAND_CAP_USD.max(r.caller.spend().cost_usd) } else { r.caller.spend().cost_usd };
        self.retired_usd + self.rooms.iter().map(each).sum::<f64>()
    }
}

impl ErrandRoom {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push(&self, event: ErrandEvent) {
        let mut inner = self.lock();
        let _ = inner.live.send(event.clone());
        self.save(inner.history.len(), &event);
        inner.history.push(event);
    }

    fn save(&self, n: usize, event: &ErrandEvent) {
        if let (Some(store), Ok(json)) = (&self.store, serde_json::to_string(event)) {
            store.send(Op::Event { errand: self.id.clone(), n, json });
        }
    }

    fn meta(&self) -> Meta {
        Meta {
            people: self.people.clone(),
            persona: self.persona.clone(),
            guard: self.guard,
            brief: self.brief.clone(),
            model: self.model.clone(),
            agent: self.agent.clone(),
            reply_to: self.reply_to.clone(),
            poisoned: self.poisoned,
            at_ms: self.at_ms,
        }
    }

    /// Whether `me` may ask about or forget this errand: in a company, the asker, the
    /// bot's owner or the office owner.
    fn may_act(&self, me: Option<&Member>) -> bool {
        let ids = [self.people.asked_by.as_ref().map(|(id, _)| id), self.people.bot_of.as_ref().map(|(id, ..)| id)];
        if ids.iter().all(Option::is_none) {
            return true; // an errand from before anyone joined
        }
        me.is_some_and(|m| m.role == Role::Owner || ids.iter().flatten().any(|id| **id == m.id))
    }

    /// The chain as it stands, checked against the newest head this errand has
    /// streamed (so entries dropped from the end show as broken too).
    pub(crate) fn chain(&self) -> Chain {
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
        let count = entries.map_or(0, |e| e.len());
        // Only an intact chain is signed: the office never vouches for a broken one.
        let signed = match (&verified, &head) {
            (Ok(_), Some(h)) => Some(self.signer.sign(&self.id, count, h, now_ms())),
            _ => None,
        };
        Chain { entries: count, intact: verified.is_ok(), head, broken: verified.err().map(|b| format!("entry {}: {}", b.seq, b.reason)), signed }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl Hub {
    fn errands(&self) -> std::sync::MutexGuard<'_, Errands> {
        self.errands.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn errand(&self, id: &str) -> Option<Arc<ErrandRoom>> {
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
        guard::deliver(&room.caller, checkers, guard::Author { model: &room.model, owner, persona: &room.persona }, message, &entries, &on_draft).await
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
            room.lock().report.get_or_insert_with(|| told.clone());
            if let Some((owner_id, _, _)) = &room.people.bot_of {
                let backed = g.delivered.audit.supported();
                let how = if !room.guard {
                    "guard off: checked, not corrected".to_string()
                } else if g.correction.is_some() {
                    "corrected from the ledger".to_string()
                } else if g.drafts.len() > 1 {
                    format!("rewritten {}x by the guard", g.drafts.len() - 1)
                } else {
                    "passed the guard first time".to_string()
                };
                let what = if turn == 0 { "reported" } else { "answered" };
                let text = format!("{} {what}: {backed} claims backed, {} false delivered ({how})", room.agent, g.delivered.false_claims.len());
                hub.company().note(owner_id, text, Some(room.id.clone()));
            }
            // The report goes back to the channel it was asked in; later answers go only
            // to whoever asked them.
            if let (0, Some(channel), Some((owner_id, ..))) = (turn, &room.reply_to, &room.people.bot_of) {
                let mut company = hub.company();
                if let Some(owner) = company.by_id(owner_id).cloned() {
                    let backed = g.delivered.audit.supported();
                    let receipts = format!(
                        "\n\n({backed} claim{} backed by the ledger, {} false delivered; receipts in the errand)",
                        if backed == 1 { "" } else { "s" },
                        g.delivered.false_claims.len()
                    );
                    let text: String = format!("{told}{receipts}").chars().take(company::MAX_MESSAGE_CHARS).collect();
                    company.say(&owner, true, channel, text, Some(room.id.clone()));
                }
            }
            room.push(ErrandEvent::Said {
                turn,
                question,
                message: g.delivered.message.clone(),
                correction: g.correction,
                finished,
                rounds: g.drafts.len().saturating_sub(1),
                check: g.delivered,
                chain: Box::new(room.chain()),
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
    /// Send it to an area: it runs on the bot of that area's lead.
    #[serde(default)]
    area: Option<String>,
    /// Send it to a bot by name: it runs on that bot.
    #[serde(default)]
    bot: Option<String>,
    /// Run it on an inbox with a planted prompt injection (to watch the defences).
    #[serde(default)]
    poisoned: bool,
}

/// An errand to start, however it was asked for.
#[derive(Default)]
pub(crate) struct Begin {
    pub brief: String,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub guard: Option<bool>,
    pub area: Option<String>,
    /// The bot's owner, when a bot was named.
    pub bot: Option<Member>,
    /// The channel to report back to.
    pub reply_to: Option<String>,
    /// On the inbox with a planted prompt injection.
    pub poisoned: bool,
}

#[derive(Serialize)]
struct Started {
    id: String,
}

pub(crate) async fn start(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Result<Json<StartErrand>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that request isn't a valid errand: {}", e.body_text())),
    };
    let bot = match body.bot.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
        Some(name) => match hub.company().by_bot(name).cloned() {
            Some(owner) => Some(owner),
            None => return error(StatusCode::BAD_REQUEST, &format!("there is no bot called {name} here")),
        },
        None => None,
    };
    let begin_with = Begin { brief: body.brief, model: body.model, agent: body.agent, guard: body.guard, area: body.area, bot, reply_to: None, poisoned: body.poisoned };
    match begin(&hub, hub.member(&headers), begin_with).await {
        Ok(id) => (StatusCode::CREATED, Json(Started { id })).into_response(),
        Err((status, why)) => error(status, &why),
    }
}

/// Start an errand for `me` (if they joined). Answers its id.
pub(crate) async fn begin(hub: &Arc<Hub>, me: Option<Member>, body: Begin) -> Result<String, (StatusCode, String)> {
    let refuse = |status, why: &str| Err((status, why.to_string()));
    let brief = body.brief.trim().to_string();
    if brief.is_empty() || brief.chars().count() > MAX_ERRAND_CHARS {
        return refuse(StatusCode::BAD_REQUEST, "the errand must be 1 to 500 characters");
    }
    // Whose bot does it: a named bot, the area's lead's, else the asker's own.
    let area = body.area.as_deref().map(str::trim).filter(|a| !a.is_empty());
    let lead = match area {
        Some(area) => match hub.company().lead_of(area).cloned() {
            Some(lead) => Some(lead),
            None => return refuse(StatusCode::BAD_REQUEST, &format!("nobody leads {area} yet")),
        },
        None => None,
    };
    let owner = body.bot.or(lead).or_else(|| me.clone());
    let agent = owner.as_ref().map_or_else(|| body.agent.map_or_else(|| DEFAULT_AGENT.to_string(), |a| a.trim().to_string()), |o| o.bot.clone());
    if agent.is_empty() || agent.chars().count() > 40 || agent.chars().any(char::is_control) {
        return refuse(StatusCode::BAD_REQUEST, "the agent's name must be 1 to 40 characters on one line");
    }
    let people = People {
        asked_by: me.as_ref().map(|m| (m.id.clone(), m.name.clone())),
        bot_of: owner.as_ref().map(|o| (o.id.clone(), o.name.clone(), o.area.clone())),
    };
    let model = body.model.unwrap_or_else(|| hub.config.model.clone());
    if let Err(why) = hub.config.check_model(&model) {
        return refuse(StatusCode::BAD_REQUEST, &why);
    }
    let checker = hub.second_checker();
    let priced: Vec<&str> = [model.as_str(), hub.config.model.as_str()].into_iter().chain(checker.as_deref()).collect();
    let budget = match Budget::new(&priced, ERRAND_CAP_USD) {
        Ok(b) => b,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let id = format!("errand-{}", agora_engine::random_hex(8));
    let ledger = match &hub.config.journal {
        Some(path) => {
            let (path, id) = (path.clone(), id.clone());
            tokio::task::spawn_blocking(move || Ledger::open(&path, &id)).await.map_err(|e| e.to_string()).and_then(|l| l.map_err(|e| e.to_string()))
        }
        None => Ledger::in_memory(&id).map_err(|e| e.to_string()),
    };
    let ledger = match ledger {
        Ok(l) => l,
        Err(e) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let guard = body.guard.unwrap_or(true);
    let persona = owner.as_ref().map(company::Member::persona).unwrap_or_default();
    let room = Arc::new(ErrandRoom {
        id: id.clone(),
        people: people.clone(),
        persona,
        guard,
        brief: brief.clone(),
        model: model.clone(),
        agent: agent.clone(),
        reply_to: body.reply_to,
        poisoned: body.poisoned,
        at_ms: now_ms(),
        caller: Caller::new(hub.config.muse.clone(), hub.config.baseten.clone(), budget),
        world: tokio::sync::Mutex::new(if body.poisoned { World::poisoned() } else { World::demo() }),
        ledger,
        signer: Arc::clone(&hub.signer),
        store: hub.store.clone(),
        inner: Mutex::new(Inner { history: Vec::new(), live: broadcast::channel(256).0, busy: true, report: None, turns: 0 }),
    });
    {
        let mut errands = hub.errands();
        if errands.rooms.iter().filter(|r| r.lock().busy).count() >= MAX_ACTIVE {
            return refuse(StatusCode::TOO_MANY_REQUESTS, &format!("{MAX_ACTIVE} errands are already running; try again when one finishes"));
        }
        let spent = errands.committed_usd();
        if spent + ERRAND_CAP_USD > hub.config.errands_cap_usd {
            return refuse(StatusCode::PAYMENT_REQUIRED, &format!("the office's errand budget is spent (${spent:.2} of ${:.2})", hub.config.errands_cap_usd));
        }
        errands.rooms.push(Arc::clone(&room));
        while errands.rooms.len() > KEEP_ERRANDS {
            let Some(oldest) = errands.rooms.iter().position(|r| !r.lock().busy) else { break };
            let gone = errands.rooms.remove(oldest);
            errands.retired_usd += gone.caller.spend().cost_usd;
            retire(gone);
        }
    }
    if let (Some(store), Ok(json)) = (&room.store, serde_json::to_string(&room.meta())) {
        store.send(Op::Errand { id: id.clone(), json });
    }
    room.push(ErrandEvent::Started { id: id.clone(), brief: brief.clone(), model, agent: agent.clone(), checker, guard, people: people.clone(), poisoned: body.poisoned });
    announce(hub, &people, &agent, &brief, &id);
    room.push(ErrandEvent::Busy { turn: 0, question: None });
    tokio::spawn(work(Arc::clone(hub), room));
    Ok(id)
}

/// Tell the company who asked whose bot to do what.
fn announce(hub: &Hub, people: &People, agent: &str, brief: &str, id: &str) {
    let Some((owner_id, owner_name, area)) = &people.bot_of else { return };
    let asker = people.asked_by.as_ref().map_or("Someone", |(_, n)| n.as_str());
    let whose = if people.asked_by.as_ref().is_some_and(|(a, _)| a == owner_id) {
        format!("their bot {agent}")
    } else if area.is_empty() {
        format!("{owner_name}'s bot {agent}")
    } else {
        format!("{owner_name}'s {area} bot {agent}")
    };
    hub.company().note(owner_id, format!("{asker} asked {whose}: {brief}"), Some(id.to_string()));
}

/// Run the errand, streaming each ledger entry as it is written, then check the report.
async fn work(hub: Arc<Hub>, room: Arc<ErrandRoom>) {
    let errand = Errand { agent: &room.agent, model: &room.model, brief: &room.brief, persona: &room.persona };
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
    room.save(inner.history.len(), &ErrandEvent::Idle);
    inner.history.push(ErrandEvent::Idle);
}

#[derive(Deserialize)]
pub(crate) struct Ask {
    question: String,
}

pub(crate) async fn ask(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap, body: Result<Json<Ask>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers) {
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
    if !room.may_act(hub.member(&headers).as_ref()) {
        return error(StatusCode::FORBIDDEN, "only whoever asked for this errand, its bot's owner, or the office owner can ask about it");
    }
    // A question may spend what is left of the errand's own cap: keep that within the
    // office's too.
    let committed = hub.errands().committed_usd();
    let could_spend = ERRAND_CAP_USD - room.caller.spend().cost_usd;
    if committed + could_spend > hub.config.errands_cap_usd {
        return error(StatusCode::PAYMENT_REQUIRED, &format!("the office's errand budget is spent (${committed:.2} of ${:.2})", hub.config.errands_cap_usd));
    }
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
        let errand = Errand { agent: &room.agent, model: &room.model, brief: &room.brief, persona: &room.persona };
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

pub(crate) async fn events(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
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

/// The chain's head, signed by the office, to keep.
pub(crate) async fn receipt(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Some(room) = hub.errand(&id) else { return error(StatusCode::NOT_FOUND, "no such errand") };
    match room.chain().signed {
        Some(signed) => {
            let name = format!("attachment; filename=\"{}-receipt.json\"", room.id);
            ([(axum::http::header::CONTENT_DISPOSITION, name)], Json(signed)).into_response()
        }
        None => error(StatusCode::CONFLICT, "there is no intact chain to sign yet"),
    }
}

#[derive(Serialize)]
struct LedgerView {
    errand: String,
    entries: Vec<Entry>,
    chain: Chain,
}

pub(crate) async fn ledger(State(hub): State<Arc<Hub>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
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
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Json(Forget { seq }) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("that isn't a valid request: {}", e.body_text())),
    };
    let Some(room) = hub.errand(&id) else { return error(StatusCode::NOT_FOUND, "no such errand") };
    if !room.may_act(hub.member(&headers).as_ref()) {
        return error(StatusCode::FORBIDDEN, "only whoever asked for this errand, its bot's owner, or the office owner can forget from it");
    }
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
    // Forgetting writes, checkpoints and may wait on a reader: off the async workers.
    let forgetting = Arc::clone(&room);
    let forgot = tokio::task::spawn_blocking(move || forgetting.ledger.forget(seq)).await;
    let receipt = match forgot {
        Ok(Ok(Some(r))) => r,
        Ok(Ok(None)) => {
            release();
            return error(StatusCode::NOT_FOUND, "no such ledger entry");
        }
        Ok(Err(e)) => {
            release();
            return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
        Err(_) => {
            release();
            return error(StatusCode::INTERNAL_SERVER_ERROR, "forgetting stopped unexpectedly");
        }
    };
    let after = room.ledger.entries().unwrap_or_default();
    let forgotten = after.iter().find(|e| e.seq == seq).cloned();
    // Reports a bot posted to a channel from this errand go too; then every message the
    // office holds is part of the scan.
    let messages = {
        let mut company = hub.company();
        company.withdraw_errand(&room.id, WITHDRAWN);
        company.message_texts()
    };
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
        let scan = scan(&needles(&before), &inner.history, &room.ledger, messages);
        (ErrandEvent::Forgotten { receipt, withdrawn, scan }, inner.history.clone())
    };
    let (mut event, history) = event;
    // The office store holds the stream too: rewrite it (with this event), then say
    // whether any old copy is left in its log.
    if let Some(store) = room.store.clone() {
        let mut events: Vec<String> = history.iter().filter_map(|e| serde_json::to_string(e).ok()).collect();
        events.push(serde_json::to_string(&event).unwrap_or_default());
        let id = room.id.clone();
        let cleared = tokio::task::spawn_blocking(move || store.rewrite(&id, events)).await.unwrap_or(false);
        if let ErrandEvent::Forgotten { scan, .. } = &mut event {
            scan.store_cleared = Some(cleared);
        }
    }
    {
        let mut inner = room.lock();
        inner.busy = false;
        let _ = inner.live.send(event.clone());
        room.save(inner.history.len(), &event);
        inner.history.push(event.clone());
    }
    Json(event).into_response()
}

/// Search what the office still holds about an errand for `wanted`: its stream, the
/// office's messages, and its other ledger entries (an agent may have repeated what it
/// read in a call it made). The text itself is searched (every string in it), not its
/// JSON encoding, where quotes and backslashes would be escaped and never match.
fn scan(wanted: &[String], history: &[ErrandEvent], ledger: &Ledger, messages: String) -> Scan {
    let mut places = vec![
        ("the errand's history".to_string(), strings(&serde_json::to_value(history).unwrap_or_default())),
        ("the office's messages".to_string(), messages),
    ];
    for e in ledger.entries().unwrap_or_default() {
        places.push((format!("entry #{} {}", e.seq, e.action.tool.name()), strings(&serde_json::to_value((&e.action, &e.outcome)).unwrap_or_default())));
    }
    let bytes = places.iter().map(|(_, text)| text.len()).sum();
    let found_in = places.into_iter().filter(|(_, text)| wanted.iter().any(|n| text.contains(n.as_str()))).map(|(place, _)| place).collect();
    Scan { needles: wanted.len(), bytes, found_in, store_cleared: None }
}

fn withdrawn_check() -> Checked {
    Checked { message: WITHDRAWN.to_string(), audit: Audit::default(), agreed: Vec::new(), false_claims: Vec::new(), unmentioned: Vec::new(), unverified: None, parsers: 0, second_missing: false }
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

/// One errand at a glance.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Summary {
    pub id: String,
    pub brief: String,
    pub agent: String,
    pub people: People,
    pub busy: bool,
    pub steps: usize,
    /// The latest message delivered, and its receipts.
    pub said: Option<SaidSummary>,
    /// Why the latest turn failed, if it did after the last delivery.
    pub failed: Option<String>,
    /// On the inbox with a planted prompt injection.
    pub poisoned: bool,
    /// When it was asked for, if known.
    pub at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SaidSummary {
    pub turn: usize,
    pub question: Option<String>,
    pub message: String,
    pub correction: Option<String>,
    pub backed: usize,
    pub false_delivered: usize,
    pub rewrites: usize,
}

impl ErrandRoom {
    pub(crate) fn summary(&self) -> Summary {
        let inner = self.lock();
        let steps = inner.history.iter().filter(|e| matches!(e, ErrandEvent::Step { .. })).count();
        // A failure after the last delivery means the latest turn (e.g. a question)
        // didn't get an answer: say so instead of passing off the old one.
        let failed = inner.history.iter().rev().take_while(|e| !matches!(e, ErrandEvent::Said { .. })).find_map(|e| match e {
            ErrandEvent::Failed { reason, .. } => Some(reason.clone()),
            _ => None,
        });
        let said = inner.history.iter().rev().find_map(|e| match e {
            ErrandEvent::Said { turn, question, message, correction, check, rounds, .. } => Some(SaidSummary {
                turn: *turn,
                question: question.clone(),
                message: message.clone(),
                correction: correction.clone(),
                backed: check.audit.supported(),
                false_delivered: check.false_claims.len(),
                rewrites: *rounds,
            }),
            _ => None,
        });
        Summary {
            id: self.id.clone(),
            brief: self.brief.clone(),
            agent: self.agent.clone(),
            people: self.people.clone(),
            busy: inner.busy,
            steps,
            said,
            failed,
            poisoned: self.poisoned,
            at_ms: (self.at_ms > 0).then_some(self.at_ms),
        }
    }
}

impl Hub {
    /// Every errand kept, newest first.
    pub(crate) fn errand_summaries(&self) -> Vec<Summary> {
        let rooms: Vec<Arc<ErrandRoom>> = self.errands().rooms.clone();
        rooms.iter().rev().map(|r| r.summary()).collect()
    }
}

pub(crate) async fn list(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    Json(hub.errand_summaries()).into_response()
}

/// Drop an errand past retention: delete its stored stream and forget its ledger's content.
fn retire(gone: Arc<ErrandRoom>) {
    if let Some(store) = &gone.store {
        store.send(Op::DropErrand { id: gone.id.clone() });
    }
    tokio::task::spawn_blocking(move || forget_all(&gone.ledger));
}

fn forget_all(ledger: &Ledger) {
    for e in ledger.entries().unwrap_or_default().iter().filter(|e| e.forgotten_at_ms.is_none()) {
        if let Err(e) = ledger.forget(e.seq) {
            eprintln!("couldn't forget an old errand's ledger ({}): {e}", ledger.run_id());
            return;
        }
    }
}

/// Bring back the errands the store kept (their ledgers are in the journal). Each
/// world is rebuilt by replaying its ledger on a fresh one (the world is
/// deterministic); a forgotten call can't be replayed, so that world may differ, which
/// only matters to later questions. An errand cut off by a restart says so.
pub(crate) fn restore(hub: &Hub, kept: Vec<(String, String, Vec<String>)>, pruned: &[String]) -> usize {
    let (Some(path), Some(store)) = (&hub.config.journal, &hub.store) else { return 0 };
    // Past retention: the store already deleted their streams; forget their ledgers.
    for id in pruned {
        if let Ok(ledger) = Ledger::open(path, id) {
            forget_all(&ledger);
        }
    }
    let mut restored = 0;
    for (id, meta, events) in kept {
        let Ok(meta) = serde_json::from_str::<Meta>(&meta) else { continue };
        let Ok(ledger) = Ledger::open(path, &id) else { continue };
        let mut world = if meta.poisoned { World::poisoned() } else { World::demo() };
        for e in ledger.entries().unwrap_or_default().iter().filter(|e| e.forgotten_at_ms.is_none()) {
            world.execute(&e.action);
        }
        let mut history: Vec<ErrandEvent> = events.iter().filter_map(|j| serde_json::from_str(j).ok()).collect();
        let turns = history.iter().filter_map(|e| if let ErrandEvent::Busy { turn, .. } = e { Some(*turn) } else { None }).max().unwrap_or(0);
        // Cut off: the last turn started (busy) and never went idle.
        let last_busy = history.iter().rposition(|e| matches!(e, ErrandEvent::Busy { .. }));
        let cut_off = last_busy.is_some_and(|at| !history[at..].iter().any(|e| matches!(e, ErrandEvent::Idle)));
        if cut_off {
            history.push(ErrandEvent::Failed { turn: turns, reason: "the office restarted while this was running".into() });
            history.push(ErrandEvent::Idle);
        }
        let report = history.iter().find_map(|e| match e {
            ErrandEvent::Said { turn: 0, message, correction, .. } => Some(match correction {
                Some(c) if message.is_empty() => c.clone(),
                Some(c) => format!("{message}\n\n{c}"),
                None => message.clone(),
            }),
            _ => None,
        });
        let checker = hub.second_checker();
        let priced: Vec<&str> = [meta.model.as_str(), hub.config.model.as_str()].into_iter().chain(checker.as_deref()).collect();
        let Ok(budget) = Budget::new(&priced, ERRAND_CAP_USD) else { continue };
        let room = ErrandRoom {
            id: id.clone(),
            people: meta.people,
            persona: meta.persona,
            guard: meta.guard,
            brief: meta.brief,
            model: meta.model,
            agent: meta.agent,
            reply_to: meta.reply_to,
            poisoned: meta.poisoned,
            at_ms: meta.at_ms,
            caller: Caller::new(hub.config.muse.clone(), hub.config.baseten.clone(), budget),
            world: tokio::sync::Mutex::new(world),
            ledger,
            signer: Arc::clone(&hub.signer),
            store: Some(store.clone()),
            inner: Mutex::new(Inner { history: Vec::new(), live: broadcast::channel(256).0, busy: false, report, turns }),
        };
        for (n, e) in history.iter().enumerate().skip(events.len()) {
            room.save(n, e);
        }
        room.lock().history = history;
        hub.errands().rooms.push(Arc::new(room));
        restored += 1;
    }
    restored
}
