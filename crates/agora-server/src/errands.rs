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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agora_core::World;
use agora_engine::alibi::Audit;
use agora_engine::connected::{self, BoxFuture, Decision, Gateway};
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
use serde_json::Value;
use tokio::sync::broadcast;

use crate::company::{self, Member, Role};
use crate::connectors::{Approver, OfficeGateway};
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
/// Whose world the office runs on.
const OWNER: &str = "Asha Rao";

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
        /// The files attached to it: name, type, size and SHA-256 (never the bytes).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<crate::attach::Listed>,
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
    /// What the bot recalled from earlier days before it started (built by code from
    /// its ledgers and memory; every line cites its source).
    Recalled { text: String },
    /// What the office's memory kept from this errand (and refused: a fact whose
    /// quote isn't in its source is never written).
    Learned { learned: agora_engine::memory::Learned },
    /// The memory couldn't be updated after the errand (the errand itself is done).
    NotLearned { reason: String },
    /// Another bot is working in the office; this errand starts when it's done.
    Waiting,
    /// A call to a connected tool that changes something waits for the person: the
    /// exact arguments and where it goes. Answer at `.../approvals/{id}`.
    Approval { id: String, tool: String, args: Value, destinations: Vec<String>, approver: Option<String> },
    /// The answer to an approval (or no answer in time).
    Decided { id: String, approved: bool, by: String },
    /// Entry `receipt.seq` was forgotten; these turns' messages were withdrawn, and
    /// `memories` facts and threads learned from it deleted.
    Forgotten {
        receipt: Receipt,
        withdrawn: Vec<usize>,
        scan: Scan,
        #[serde(default)]
        memories: (usize, usize),
    },
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
    /// The office day it ran on.
    #[serde(default)]
    day: usize,
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
    /// The attached files and text, until the work takes them (the bytes aren't kept
    /// once it has).
    files: Mutex<Option<crate::attach::Attached>>,
    /// Calls to connected tools waiting for the person, by approval id.
    approvals: Mutex<BTreeMap<String, Waiting>>,
    /// An added agent's connected tools (`None` = a person's bot: every tool on).
    only_tools: Option<Vec<String>>,
    /// The connected tools it had (id and weight), for the guard's parsers.
    available: Mutex<Vec<(String, agora_core::Weight)>>,
    /// When it was asked for.
    at_ms: u64,
    /// The office day it ran on.
    day: std::sync::atomic::AtomicUsize,
    caller: Caller,
    /// The office's world (shared: errands that change it take turns), or for a
    /// poisoned errand its own copy with the injection planted.
    world: Arc<tokio::sync::Mutex<World>>,
    ledger: Ledger,
    signer: Arc<Signer>,
    store: Option<Store>,
    memory: Arc<agora_engine::memory::Memory>,
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
            day: self.day(),
        }
    }

    /// Whether `me` may ask about or forget this errand: in a company, the asker, the
    /// bot's owner or the office owner.
    /// Who approves a call that changes something: the bot's owner, else whoever
    /// asked, else (before anyone joined) anyone who may use the office.
    fn approver(&self) -> Option<String> {
        self.people.bot_of.as_ref().map(|(_, name, _)| name.clone()).or_else(|| self.people.asked_by.as_ref().map(|(_, name)| name.clone()))
    }

    /// `empty_office`: nobody has joined, so anyone who may use it may answer an
    /// errand from before anyone joined (once someone has, nobody may).
    fn may_approve(&self, me: Option<&Member>, empty_office: bool) -> bool {
        match (&self.people.bot_of, &self.people.asked_by) {
            (Some((owner, ..)), _) => me.is_some_and(|m| m.id == *owner),
            (None, Some((asker, _))) => me.is_some_and(|m| m.id == *asker),
            (None, None) => empty_office,
        }
    }

    /// `empty_office`: as for [`Self::may_approve`]. An errand from before anyone joined
    /// is anyone's while nobody has; after that, the office owner's.
    fn may_act(&self, me: Option<&Member>, empty_office: bool) -> bool {
        let ids = [self.people.asked_by.as_ref().map(|(id, _)| id), self.people.bot_of.as_ref().map(|(id, ..)| id)];
        if empty_office && ids.iter().all(Option::is_none) {
            return true;
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

/// One errand's ledger, owned, for recall across the week.
pub(crate) struct Recorded {
    pub id: String,
    pub day: usize,
    pub entries: Vec<Entry>,
}

pub(crate) fn views(record: &[Recorded]) -> Vec<agora_engine::memory::Errand<'_>> {
    record.iter().map(|r| agora_engine::memory::Errand { id: &r.id, day: r.day, entries: &r.entries }).collect()
}

impl ErrandRoom {
    /// The office day it ran on.
    fn day(&self) -> usize {
        self.day.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// What the bot recalled before it started, if anything.
    fn recalled(&self) -> String {
        self.lock().history.iter().find_map(|e| if let ErrandEvent::Recalled { text } = e { Some(text.clone()) } else { None }).unwrap_or_default()
    }
}

impl Hub {
    /// The office's record: every kept errand's ledger in order (red-team copies and
    /// `except` left out).
    pub(crate) fn record(&self, except: &str) -> Vec<Recorded> {
        let rooms: Vec<Arc<ErrandRoom>> = self.errands().rooms.clone();
        rooms
            .iter()
            .filter(|r| !r.poisoned && r.id != except)
            .map(|r| Recorded { id: r.id.clone(), day: r.day(), entries: r.ledger.entries().unwrap_or_default() })
            .collect()
    }

    /// Count spending that didn't belong to a kept errand (a question about the week).
    /// Negative to give back part of a reservation.
    pub(crate) fn retire_spend(&self, usd: f64) {
        self.errands().retired_usd += usd;
    }

    /// Reserve `usd` of the office's errand budget for work that isn't an errand (a
    /// question about the week), checked and taken under one lock so concurrent
    /// questions can't overshoot. Answers what was committed when it can't.
    pub(crate) fn reserve(&self, usd: f64) -> Result<(), f64> {
        let mut errands = self.errands();
        let committed = errands.committed_usd();
        if committed + usd > self.config.errands_cap_usd {
            return Err(committed);
        }
        errands.retired_usd += usd;
        Ok(())
    }

    fn errands(&self) -> std::sync::MutexGuard<'_, Errands> {
        self.errands.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn errand(&self, id: &str) -> Option<Arc<ErrandRoom>> {
        self.errands().rooms.iter().find(|r| r.id == id).cloned()
    }

    /// The second checker: Spark Open on Baseten when the office has it and the
    /// first checker isn't already on it.
    pub(crate) fn second_checker(&self) -> Option<String> {
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
    let available = room.available.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let checkers = Checkers { first: &hub.config.model, second: second.as_deref(), connected: &available };
    let on_draft = |round: usize, check: &Checked| room.push(ErrandEvent::Draft { turn, round, check: check.clone() });
    let owner = OWNER;
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
        Ok(mut g) => {
            // The errand's report lists its reads; a later answer is about one question.
            if room.guard && question.is_none() {
                guard::list_reads(&mut g, &entries);
            }
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
    /// Images, PDFs or text files for the bot to read with the errand.
    #[serde(default)]
    attachments: Vec<crate::attach::Upload>,
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
    /// An agent someone added, when it was named (its owner is in `bot`).
    pub agent_bot: Option<crate::agents::AgentBot>,
    /// The channel to report back to.
    pub reply_to: Option<String>,
    /// On the inbox with a planted prompt injection.
    pub poisoned: bool,
    /// Files to read with it (checked once the model is known).
    pub uploads: Vec<crate::attach::Upload>,
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
    let (bot, agent_bot) = match body.bot.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
        Some(name) => match resolve_bot(&hub, name) {
            Some(found) => found,
            None => return error(StatusCode::BAD_REQUEST, &format!("there is no bot called {name} here")),
        },
        None => (None, None),
    };
    let begin_with = Begin { brief: body.brief, model: body.model, agent: body.agent, guard: body.guard, area: body.area, bot, agent_bot, reply_to: None, poisoned: body.poisoned, uploads: body.attachments };
    match begin(&hub, hub.member(&headers), begin_with).await {
        Ok(id) => (StatusCode::CREATED, Json(Started { id })).into_response(),
        Err((status, why)) => error(status, &why),
    }
}

/// Whose bot does an errand: a named bot (or added agent), the area's lead's, else the
/// asker's own. Answers its owner and the name it works under.
fn whose(hub: &Hub, me: Option<&Member>, area: Option<&str>, bot: Option<Member>, agent_bot: Option<&crate::agents::AgentBot>, agent: Option<String>) -> Result<(Option<Member>, String), (StatusCode, String)> {
    let lead = match area.map(str::trim).filter(|a| !a.is_empty()) {
        Some(area) => Some(hub.company().lead_of(area).cloned().ok_or_else(|| (StatusCode::BAD_REQUEST, format!("nobody leads {area} yet")))?),
        None => None,
    };
    let owner = bot.or(lead).or_else(|| me.cloned());
    let name = match agent_bot {
        Some(a) => a.name.clone(),
        None => owner.as_ref().map_or_else(|| agent.map_or_else(|| DEFAULT_AGENT.to_string(), |a| a.trim().to_string()), |o| o.bot.clone()),
    };
    Ok((owner, name))
}

/// The bot called `name`: a person's (its owner), or an added agent (and its owner,
/// if it has one).
pub(crate) fn resolve_bot(hub: &Hub, name: &str) -> Option<(Option<Member>, Option<crate::agents::AgentBot>)> {
    if let Some(owner) = hub.company().by_bot(name).cloned() {
        return Some((Some(owner), None));
    }
    let agent = hub.agents().by_name(name).cloned()?;
    let owner = agent.owner.as_deref().and_then(|id| hub.company().by_id(id).cloned());
    Some((owner, Some(agent)))
}

/// Start an errand for `me` (if they joined). Answers its id.
pub(crate) async fn begin(hub: &Arc<Hub>, me: Option<Member>, body: Begin) -> Result<String, (StatusCode, String)> {
    let refuse = |status, why: &str| Err((status, why.to_string()));
    let brief = body.brief.trim().to_string();
    if brief.is_empty() || brief.chars().count() > MAX_ERRAND_CHARS {
        return refuse(StatusCode::BAD_REQUEST, "the errand must be 1 to 500 characters");
    }
    // Once people have joined, an errand has someone answerable for it.
    if me.is_none() && !hub.company().members_view().is_empty() {
        return refuse(StatusCode::FORBIDDEN, "join the office first: an errand needs someone answerable for it");
    }
    let (owner, agent) = whose(hub, me.as_ref(), body.area.as_deref(), body.bot, body.agent_bot.as_ref(), body.agent)?;
    let agent_bot = body.agent_bot;
    if agent.is_empty() || agent.chars().count() > 40 || agent.chars().any(char::is_control) {
        return refuse(StatusCode::BAD_REQUEST, "the agent's name must be 1 to 40 characters on one line");
    }
    let people = People {
        asked_by: me.as_ref().map(|m| (m.id.clone(), m.name.clone())),
        bot_of: owner.as_ref().map(|o| (o.id.clone(), o.name.clone(), o.area.clone())),
    };
    let model = body.model.or_else(|| agent_bot.as_ref().and_then(|a| a.model.clone())).unwrap_or_else(|| hub.config.model.clone());
    hub.config.check_model(&model).map_err(|why| (StatusCode::BAD_REQUEST, why))?;
    let files = crate::attach::accept(body.uploads, &model).map_err(|why| (StatusCode::BAD_REQUEST, why))?;
    let listed = files.listed.clone();
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
    let persona = agent_bot.as_ref().map_or_else(|| owner.as_ref().map(company::Member::persona).unwrap_or_default(), crate::agents::AgentBot::persona);
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
        files: Mutex::new(Some(files)),
        approvals: Mutex::default(),
        available: Mutex::default(),
        only_tools: agent_bot.map(|a| a.tools),
        at_ms: now_ms(),
        caller: Caller::new(hub.config.muse.clone(), hub.config.baseten.clone(), budget),
        day: std::sync::atomic::AtomicUsize::new(hub.day()),
        // A poisoned errand gets its own copy of the office when it starts (see `work`).
        world: if body.poisoned { Arc::new(tokio::sync::Mutex::new(World::poisoned())) } else { Arc::clone(&hub.office) },
        ledger,
        signer: Arc::clone(&hub.signer),
        store: hub.store.clone(),
        memory: Arc::clone(&hub.memory),
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
    room.push(ErrandEvent::Started { id: id.clone(), brief: brief.clone(), model, agent: agent.clone(), checker, guard, people: people.clone(), poisoned: body.poisoned, attachments: listed });
    announce(hub, &people, &agent, &brief, &id);
    let recalled = agora_engine::memory::recall(&hub.memory, &views(&hub.record(&id)), &brief);
    if !recalled.is_empty() {
        room.push(ErrandEvent::Recalled { text: recalled });
    }
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
    let recalled = room.recalled();
    // The files go to this run once; the room doesn't keep the bytes, and they're
    // dropped as soon as the work is done (before the guard and memory calls).
    let files = room.files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take().unwrap_or_default();
    if room.poisoned {
        // The red-team copy: the office as it is now, with the injection planted.
        let mut copy = hub.office.lock().await.clone();
        copy.poison();
        *room.world.lock().await = copy;
    }
    // The office's real tools, checked against what the owner approved (before taking
    // the world: listing them waits on the network).
    let tools = OfficeGateway::prepare(&hub, Arc::new(RoomApprover(Arc::clone(&room), Arc::clone(&hub))), room.only_tools.as_deref()).await;
    *room.available.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = tools.listed();
    let done = {
        // The office's world is shared: an errand that may change it waits its turn.
        // (A call waiting for approval holds it, up to APPROVAL_WAIT: the next errand
        // that may change the office waits too, rather than act on a world about to
        // change under it.)
        let mut world = if let Ok(world) = room.world.try_lock() {
            world
        } else {
            room.push(ErrandEvent::Waiting);
            room.world.lock().await
        };
        // The day it really ran on (a new day may have started while it waited).
        if !room.poisoned && world.today != room.day() {
            room.day.store(world.today, std::sync::atomic::Ordering::Relaxed);
            if let (Some(store), Ok(json)) = (&room.store, serde_json::to_string(&room.meta())) {
                store.send(Op::Errand { id: room.id.clone(), json });
            }
        }
        let on_step = |entry: &Entry| room.push(ErrandEvent::Step { entry: entry.clone() });
        let gateway = (!tools.is_empty()).then_some(&tools as &dyn Gateway);
        let errand = Errand { agent: &room.agent, model: &room.model, brief: &room.brief, persona: &room.persona, memory: &recalled, attachments: &files.media, documents: &files.text, gateway };
        let done = errand::run(&room.caller, &room.ledger, &mut world, &errand, &on_step).await;
        drop(files);
        if !room.poisoned {
            hub.set_office_view(&world);
        }
        done
    };
    match done {
        Ok(done) => {
            let message = if done.message.is_empty() { "(The agent ran out of steps without reporting.)".to_string() } else { done.message };
            deliver(&hub, &room, 0, None, &message, done.finished).await;
            // The office remembers what this errand's calls can back (not the red-team copy).
            if !room.poisoned {
                match agora_engine::memory::learn(&room.caller, &hub.config.model, &hub.memory, &room.id, &done.entries, room.day()).await {
                    Ok(learned) => room.push(ErrandEvent::Learned { learned }),
                    Err(e) => room.push(ErrandEvent::NotLearned { reason: e.to_string() }),
                }
            }
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
    if !room.may_act(hub.member(&headers).as_ref(), hub.company().members_view().is_empty()) {
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
        // With the guard on, the agent answers from its ledger and the office's memory,
        // not from its own impressions.
        let recalled = if room.guard { agora_engine::memory::recall(&hub.memory, &views(&hub.record(&room.id)), &question) } else { String::new() };
        // A later question is about the record; the attached files went with the work.
        let errand = Errand { agent: &room.agent, model: &room.model, brief: &room.brief, persona: &room.persona, memory: &recalled, attachments: &[], documents: "", gateway: None };
        let log = room.guard.then(|| guard::log(&room.ledger.entries().unwrap_or_default()));
        // A snapshot, without waiting: the office may be busy with another errand.
        let world = if room.poisoned { room.world.lock().await.clone() } else { hub.office_view() };
        let answer = errand::ask_later(&room.caller, &world, &errand, &report, &question, log.as_deref()).await;
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
    // A connected call's arguments: each text value in them.
    let args: Vec<String> = a.call.as_ref().and_then(|c| serde_json::from_str::<Value>(&c.args).ok()).map(|v| strings(&v).lines().map(str::to_string).collect()).unwrap_or_default();
    let mut found: Vec<String> = [entry.outcome.result.as_str(), a.text.as_str()]
        .into_iter()
        .chain(args.iter().map(String::as_str))
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
    if !room.may_act(hub.member(&headers).as_ref(), hub.company().members_view().is_empty()) {
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
    // Later errands recalled this entry (and facts learned from it): those lines go too,
    // and what is left of every errand's stream is searched.
    let others = scrub_recall(&hub, &room.id, seq).await;
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
                other => scrub_approval(other, &before),
            }
        }
        if inner.report.is_some() {
            inner.report = Some(WITHDRAWN.to_string());
        }
        // Search what is left for this errand: the stream history, then every other
        // ledger entry (an agent may have repeated what it read in a call it made).
        let scan = scan(&needles(&before), &inner.history, &room.ledger, messages, others);
        let memories = hub.memory.forget_source(&room.id, seq).unwrap_or_default();
        (ErrandEvent::Forgotten { receipt, withdrawn, scan, memories }, inner.history.clone())
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

/// Remove, from every other errand's recall, the lines that cite entry `seq` of
/// `errand` (they quote what it returned), and rewrite those streams in the store.
/// Answers each other errand's stream as text, for the forget's scan.
async fn scrub_recall(hub: &Hub, errand: &str, seq: u64) -> Vec<(String, String)> {
    let cite = |line: &str| {
        let id = errand.strip_prefix("errand-").unwrap_or(errand);
        let short = id.get(..8).unwrap_or(id);
        line.contains(&format!("[{short}#{seq}]")) || line.contains(&format!("[{short}#{seq},"))
    };
    let rooms: Vec<Arc<ErrandRoom>> = hub.errands().rooms.iter().filter(|r| r.id != errand).cloned().collect();
    let mut all = Vec::new();
    for other in rooms {
        let rewritten = {
            let mut inner = other.lock();
            let mut changed = false;
            for e in &mut inner.history {
                if let ErrandEvent::Recalled { text } = e
                    && text.lines().any(cite)
                {
                    *text = text.lines().filter(|l| !cite(l)).collect::<Vec<_>>().join("\n");
                    text.push_str("\n(a line here was removed: it held something the person asked to forget)");
                    changed = true;
                }
            }
            all.push((format!("errand {}", other.id), strings(&serde_json::to_value(&inner.history).unwrap_or_default())));
            changed.then(|| inner.history.iter().filter_map(|e| serde_json::to_string(e).ok()).collect::<Vec<_>>())
        };
        if let (Some(events), Some(store)) = (rewritten, other.store.clone()) {
            let id = other.id.clone();
            let _ = tokio::task::spawn_blocking(move || store.rewrite(&id, events)).await;
        }
    }
    all
}

/// The approval card for a forgotten call showed its arguments: they go too.
fn scrub_approval(event: &mut ErrandEvent, forgotten: &Entry) {
    if let ErrandEvent::Approval { tool, args, destinations, .. } = event
        && forgotten.action.call.as_ref().is_some_and(|c| c.tool == *tool && serde_json::from_str::<Value>(&c.args).ok().as_ref() == Some(&*args))
    {
        *args = Value::String(agora_engine::ledger::FORGOTTEN.into());
        destinations.clear();
    }
}

/// Search what the office still holds about an errand for `wanted`: its stream, the
/// office's messages, and its other ledger entries (an agent may have repeated what it
/// read in a call it made). The text itself is searched (every string in it), not its
/// JSON encoding, where quotes and backslashes would be escaped and never match.
fn scan(wanted: &[String], history: &[ErrandEvent], ledger: &Ledger, messages: String, others: Vec<(String, String)>) -> Scan {
    let mut places = vec![
        ("the errand's history".to_string(), strings(&serde_json::to_value(history).unwrap_or_default())),
        ("the office's messages".to_string(), messages),
    ];
    places.extend(others);
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
    /// A connected tool's call waiting for a person's answer, if one is.
    pub awaiting_approval: Option<Pending>,
}

/// A call waiting for approval, as anyone in the office may see it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Pending {
    pub tool: String,
    pub destinations: Vec<String>,
    /// Who must answer, by name; `None`: whoever may answer for the errand.
    pub approver: Option<String>,
    /// And by member id, as the server checks it.
    pub approver_id: Option<String>,
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
        // The newest approval, if a call is still waiting on it (after a restart nothing
        // is: the waits were in memory).
        let live = self.approvals.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let awaiting_approval = inner.history.iter().rev().find_map(|e| match e {
            ErrandEvent::Approval { id, tool, destinations, approver, .. } => live.get(id).map(|w| Pending { tool: tool.clone(), destinations: destinations.clone(), approver: approver.clone(), approver_id: w.approver.clone() }),
            _ => None,
        });
        drop(live);
        Summary {
            awaiting_approval,
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
    /// The newest errand's id.
    pub(crate) fn newest_errand(&self) -> Option<String> {
        self.errands().rooms.last().map(|r| r.id.clone())
    }

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
    let memory = Arc::clone(&gone.memory);
    tokio::task::spawn_blocking(move || forget_all(&gone.ledger, &memory));
}

fn forget_all(ledger: &Ledger, memory: &agora_engine::memory::Memory) {
    for e in ledger.entries().unwrap_or_default().iter().filter(|e| e.forgotten_at_ms.is_none()) {
        if let Err(e) = ledger.forget(e.seq) {
            eprintln!("couldn't forget an old errand's ledger ({}): {e}", ledger.run_id());
            return;
        }
        let _ = memory.forget_source(ledger.run_id(), e.seq);
    }
}

/// Bring back the errands the store kept (their ledgers are in the journal), and the
/// office's world: Asha's week replayed, errand by errand on the day each ran (the
/// world is deterministic), then moved on to `today`. A forgotten call can't be
/// replayed, so the world may differ there. A red-team errand gets its own copy. An
/// errand cut off by a restart says so.
pub(crate) fn restore(hub: &Hub, kept: Vec<(String, String, Vec<String>)>, pruned: &[String], today: usize) -> usize {
    let mut office = World::demo();
    let restored = restore_errands(hub, kept, pruned, &mut office);
    while office.today < today.min(agora_core::world::DAYS.len() - 1) {
        office.next_day();
    }
    hub.set_office_view(&office);
    if let Ok(mut world) = hub.office.try_lock() {
        *world = office;
    }
    restored
}

fn restore_errands(hub: &Hub, kept: Vec<(String, String, Vec<String>)>, pruned: &[String], office: &mut World) -> usize {
    let (Some(path), Some(store)) = (&hub.config.journal, &hub.store) else { return 0 };
    // Past retention: the store already deleted their streams; forget their ledgers.
    for id in pruned {
        if let Ok(ledger) = Ledger::open(path, id) {
            let _ = ledger.close_unsettled(); // then forgotten with the rest
            forget_all(&ledger, &hub.memory);
        }
    }
    let mut restored = 0;
    for (id, meta, events) in kept {
        let Ok(meta) = serde_json::from_str::<Meta>(&meta) else { continue };
        let Ok(ledger) = Ledger::open(path, &id) else { continue };
        // Calls the office stopped during: on the record as "outcome unknown".
        let _ = ledger.close_unsettled();
        while office.today < meta.day && office.today + 1 < agora_core::world::DAYS.len() {
            office.next_day();
        }
        let world = if meta.poisoned {
            let mut copy = office.clone();
            copy.poison();
            for e in ledger.entries().unwrap_or_default().iter().filter(|e| e.forgotten_at_ms.is_none()) {
                copy.execute(&e.action);
            }
            Arc::new(tokio::sync::Mutex::new(copy))
        } else {
            for e in ledger.entries().unwrap_or_default().iter().filter(|e| e.forgotten_at_ms.is_none()) {
                office.execute(&e.action);
            }
            Arc::clone(&hub.office)
        };
        let mut history: Vec<ErrandEvent> = events.iter().filter_map(|j| serde_json::from_str(j).ok()).collect();
        // Entries the stream never showed (closed as unknown at a restart) join it.
        let shown: Vec<u64> = history.iter().filter_map(|e| if let ErrandEvent::Step { entry } = e { Some(entry.seq) } else { None }).collect();
        let missing: Vec<Entry> = ledger.entries().unwrap_or_default().into_iter().filter(|e| !shown.contains(&e.seq)).collect();
        let unknown = missing.iter().filter(|e| e.outcome.result.starts_with(agora_engine::ledger::UNKNOWN)).count();
        history.extend(missing.into_iter().map(|entry| ErrandEvent::Step { entry }));
        let turns = history.iter().filter_map(|e| if let ErrandEvent::Busy { turn, .. } = e { Some(*turn) } else { None }).max().unwrap_or(0);
        // Cut off: the last turn started (busy) and never went idle.
        let last_busy = history.iter().rposition(|e| matches!(e, ErrandEvent::Busy { .. }));
        let cut_off = last_busy.is_some_and(|at| !history[at..].iter().any(|e| matches!(e, ErrandEvent::Idle)));
        let calls = if unknown == 1 { "1 call".to_string() } else { format!("{unknown} calls") };
        if cut_off {
            let reason = match unknown {
                0 => "the office restarted while this was running".to_string(),
                _ => format!("the office restarted while this was running; {calls} may or may not have happened (see the ledger)"),
            };
            history.push(ErrandEvent::Failed { turn: turns, reason });
            history.push(ErrandEvent::Idle);
        } else if unknown > 0 {
            // It had already stopped (its ledger failed mid-call): still say why these are here.
            history.push(ErrandEvent::Failed { turn: turns, reason: format!("the office stopped during a call; {calls} may or may not have happened (see the ledger)") });
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
            // A restored errand's work is done: its files were never kept.
            files: Mutex::new(None),
            approvals: Mutex::default(),
        available: Mutex::default(),
            only_tools: None,
            at_ms: meta.at_ms,
            day: std::sync::atomic::AtomicUsize::new(meta.day),
            caller: Caller::new(hub.config.muse.clone(), hub.config.baseten.clone(), budget),
            world,
            ledger,
            signer: Arc::clone(&hub.signer),
            store: Some(store.clone()),
            memory: Arc::clone(&hub.memory),
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

/// How long a call waits for the person before it counts as declined.
const APPROVAL_WAIT: std::time::Duration = std::time::Duration::from_mins(10);

/// A call waiting for an answer, and who may give it.
pub(crate) struct Waiting {
    tx: tokio::sync::oneshot::Sender<(bool, String)>,
    /// The member who must answer (the connector's owner: it's their account acting);
    /// `None`: whoever may answer for the errand.
    approver: Option<String>,
}

/// Puts a connected tool's call in front of the person through the errand's stream.
struct RoomApprover(Arc<ErrandRoom>, Arc<Hub>);

impl Approver for RoomApprover {
    fn ask<'a>(&'a self, ask: connected::Ask<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            let room = &self.0;
            // The connector's owner answers: the call acts as them.
            let connector = ask.tool.id.split_once('.').map_or(ask.tool.id.as_str(), |(c, _)| c);
            let owner = self.1.connectors().added_by(connector);
            let id = agora_engine::random_hex(6);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let approver_name = owner.as_ref().map(|(_, name)| name.clone()).or_else(|| room.approver());
            room.approvals.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id.clone(), Waiting { tx, approver: owner.map(|(id, _)| id) });
            room.push(ErrandEvent::Approval { id: id.clone(), tool: ask.tool.id.clone(), args: ask.args.clone(), destinations: ask.destinations.to_vec(), approver: approver_name });
            let answer = tokio::time::timeout(APPROVAL_WAIT, rx).await;
            // Whoever takes the waiting entry decides: an answer that lost the race to
            // the timeout is refused, so the page never shows an approval that didn't run.
            let timed_out = room.approvals.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id).is_some();
            match answer {
                Ok(Ok((true, by))) => Decision::Approved { by },
                Ok(Ok((false, by))) => Decision::Declined { by },
                // The waiting entry went without an answer: the wait was withdrawn.
                Ok(Err(_)) => Decision::Withdrawn,
                Err(_) => {
                    if timed_out {
                        room.push(ErrandEvent::Decided { id, approved: false, by: "nobody (no answer in 10 minutes)".into() });
                    }
                    Decision::Unanswered
                }
            }
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct Answer {
    approve: bool,
}

/// `POST /api/errands/{id}/approvals/{approval} {approve}`: the member who connected
/// the tool (else the bot's owner, else whoever asked) lets a waiting call go ahead,
/// or stops it.
pub(crate) async fn decide(State(hub): State<Arc<Hub>>, Path((id, approval)): Path<(String, String)>, headers: HeaderMap, body: Result<Json<Answer>, JsonRejection>) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    // A CLI key is an agent in someone's terminal: it never answers for the person.
    if crate::cli::key_of(&headers).is_some() {
        return error(StatusCode::FORBIDDEN, "approvals are answered by a person in the office, never with a CLI key");
    }
    let Ok(Json(Answer { approve })) = body else { return error(StatusCode::BAD_REQUEST, "send {\"approve\": true or false}") };
    let Some(room) = hub.errand(&id) else { return error(StatusCode::NOT_FOUND, "no such errand") };
    let me = hub.member(&headers);
    let members = hub.company().members_view();
    let empty_office = members.is_empty();
    let mut waiting = room.approvals.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(w) = waiting.get(&approval) else {
        return error(StatusCode::NOT_FOUND, "nothing is waiting for that answer (it was answered, or timed out)");
    };
    let allowed = match &w.approver {
        // If whoever connected the tool has left, the office's owner answers for it.
        Some(owner) => me.as_ref().is_some_and(|m| m.id == *owner || (m.role == Role::Owner && !members.iter().any(|x| x.id == *owner))),
        None => room.may_approve(me.as_ref(), empty_office),
    };
    if !allowed {
        let who = if w.approver.is_some() { None } else { room.approver() };
        drop(waiting);
        return error(StatusCode::FORBIDDEN, &format!("only {} can answer this", who.unwrap_or_else(|| "the member who connected this tool".into())));
    }
    let Some(w) = waiting.remove(&approval) else { return error(StatusCode::NOT_FOUND, "nothing is waiting for that answer") };
    drop(waiting);
    let by = me.map_or_else(|| "the person".to_string(), |m| m.name);
    if w.tx.send((approve, by.clone())).is_err() {
        // The wait ran out between taking the entry and answering: the call was
        // declined, and the card must say so (the timeout found nothing to close).
        room.push(ErrandEvent::Decided { id: approval, approved: false, by: "nobody (no answer in 10 minutes)".into() });
        return error(StatusCode::GONE, "that call stopped waiting (it timed out)");
    }
    room.push(ErrandEvent::Decided { id: approval, approved: approve, by });
    StatusCode::NO_CONTENT.into_response()
}
