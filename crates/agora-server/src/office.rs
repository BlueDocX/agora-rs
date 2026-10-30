//! The office across days: its clock, its memory, and questions about the week.
//!
//! - `GET /api/office`: today, and what the office remembers (facts with their
//!   sources, conflicts, open threads, counts).
//! - `POST /api/office/day`: start the next day (the owner, once someone owns the
//!   office). What arrives that day (replies, new mail) lands in Asha's world, the
//!   memory's forgetting rules run, and the company is told.
//! - `POST /api/memory/ask {"question"}`: ask your bot about the whole week. It
//!   answers from recall built by code (every line cited), and the answer goes
//!   through the guard, checked against every errand's ledger. Answers
//!   `{message, correction, check, recalled, refs}`; `refs[i]` maps entry `i + 1` of
//!   the check back to its errand and seq.
//! - `POST /api/memory/facts/{id}/keep`: settle a conflict (this one stands).
//!   `POST /api/memory/facts/{id}/restore`: undo a retirement (rollback).
//!   `DELETE /api/memory/facts/{id}`: forget a fact. The owner decides, once someone
//!   owns the office.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use agora_engine::guard::{self, Author, Checkers};
use agora_engine::memory::{self, Fact, Stats, Thread};
use agora_engine::{Budget, Caller};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::company::Role;
use crate::errands::{ERRAND_CAP_USD, views};
use crate::store::Op;
use crate::{Hub, authorized, error, unauthorized};

const MAX_QUESTION_CHARS: usize = 300;
/// Errands a question about the week is checked against: the most recent ones.
const WEEK_ERRANDS: usize = 50;

#[derive(Serialize)]
struct View {
    day: usize,
    day_name: &'static str,
    facts: Vec<Fact>,
    threads: Vec<Thread>,
    stats: Stats,
    /// Size by day: [day, active, conflicts, retired, open threads].
    growth: Vec<[usize; 5]>,
}

pub(crate) async fn view(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let day = hub.day();
    let read = (hub.memory.facts(), hub.memory.threads(), hub.memory.stats(), hub.memory.growth());
    match read {
        (Ok(facts), Ok(threads), Ok(stats), Ok(growth)) => Json(View { day, day_name: agora_core::world::DAYS[day.min(6)], facts, threads, stats, growth }).into_response(),
        _ => error(StatusCode::INTERNAL_SERVER_ERROR, "the office's memory can't be read"),
    }
}

/// The owner decides about the office's days and memory, once someone owns it.
fn may_run_office(hub: &Hub, headers: &HeaderMap) -> Option<String> {
    let me = hub.member(headers);
    let anyone = hub.company().members_view().is_empty();
    match me {
        Some(m) if m.role == Role::Owner => Some(m.name),
        _ if anyone => Some("the person".to_string()),
        _ => None,
    }
}

fn not_owner() -> Response {
    error(StatusCode::FORBIDDEN, "only the office's owner can do that")
}

#[derive(Serialize)]
struct NewDay {
    day: usize,
    day_name: &'static str,
    arrived: Vec<String>,
    retired_facts: usize,
}

pub(crate) async fn next_day(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    if may_run_office(&hub, &headers).is_none() {
        return not_owner();
    }
    // A bot at work holds the office: the day changes when it's done, not under it.
    let Ok(mut world) = hub.office.try_lock() else { return error(StatusCode::CONFLICT, "a bot is working in the office; start the next day when it's done") };
    if world.today + 1 >= agora_core::world::DAYS.len() {
        return error(StatusCode::CONFLICT, "it's already Sunday: the week is over");
    }
    let arrived = world.next_day();
    let day = world.today;
    hub.set_office_view(&world);
    drop(world);
    hub.today.store(day, Ordering::Relaxed);
    if let Some(store) = &hub.store {
        store.send(Op::Kv { key: "day".into(), value: day.to_string() });
    }
    let (expired, over) = hub.memory.consolidate(day).unwrap_or_default();
    let alert = hub.memory.record_growth(day.saturating_sub(1)).ok().flatten();
    let day_name = agora_core::world::DAYS[day];
    let what = if arrived.is_empty() { "nothing new arrived".to_string() } else { arrived.join("; ") };
    let mut company = hub.company();
    company.note("office", format!("It's {day_name}: {what}"), None);
    if let Some(alert) = alert {
        company.note("office", alert, None);
    }
    drop(company);
    Json(NewDay { day, day_name, arrived, retired_facts: expired + over }).into_response()
}

#[derive(Deserialize)]
pub(crate) struct Ask {
    question: String,
}

#[derive(Serialize)]
struct Answer {
    agent: String,
    message: String,
    correction: Option<String>,
    rewrites: usize,
    check: guard::Checked,
    recalled: String,
    refs: Vec<(String, u64)>,
    spent_usd: f64,
}

/// The bot answers from its record, never from its own impressions.
fn prompt(agent: &str, day_name: &str, recalled: &str, question: &str) -> String {
    format!(
        "You are {agent}, Asha Rao's personal assistant. It is {day_name}.\n\nYour record of the week (written by the system from your ledger and the office's memory; \
         cite a [ref] when you use a line, and if something isn't here, say you don't know rather than guess):\n{}\n\nAsha asks: {question} \
         Answer in a few plain sentences.",
        if recalled.trim().is_empty() { "(nothing yet)" } else { recalled }
    )
}

pub(crate) async fn ask(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Result<Json<Ask>, JsonRejection>) -> Response {
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
    let agent = hub.member(&headers).map_or_else(|| "Scout".to_string(), |m| m.bot);
    // Reserve the question's whole cap now; give back what it didn't spend after.
    if let Err(committed) = hub.reserve(ERRAND_CAP_USD) {
        return error(StatusCode::PAYMENT_REQUIRED, &format!("the office's errand budget is spent (${committed:.2} of ${:.2})", hub.config.errands_cap_usd));
    }
    let mut record = hub.record("");
    // The week check reads the most recent errands, not the office's whole history.
    let older = record.len().saturating_sub(WEEK_ERRANDS);
    record.drain(..older);
    let errands = views(&record);
    let recalled = memory::recall(&hub.memory, &errands, &question);
    let (entries, refs) = memory::combined(&errands);
    let model = hub.config.model.clone();
    let second = hub.second_checker();
    let priced: Vec<&str> = [model.as_str()].into_iter().chain(second.as_deref()).collect();
    let budget = match Budget::new(&priced, ERRAND_CAP_USD) {
        Ok(b) => b,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let caller = Caller::new(hub.config.muse.clone(), hub.config.baseten.clone(), budget);
    let day_name = agora_core::world::DAYS[hub.day().min(6)];
    let request = agora_muse::Request::new(&model, vec![agora_muse::Message::system(agora_engine::CHARTER), agora_muse::Message::user(prompt(&agent, day_name, &recalled, &question))])
        .effort(agora_muse::Effort::Low)
        .cache_key("agora-memory-ask")
        .max_output_tokens(3000);
    let draft = match caller.call(&request).await {
        Ok(r) => r.text.trim().to_string(),
        Err(e) => {
            hub.retire_spend(caller.spend().cost_usd - ERRAND_CAP_USD);
            return error(StatusCode::BAD_GATEWAY, &format!("the bot couldn't answer: {e}"));
        }
    };
    let checkers = Checkers { first: &model, second: second.as_deref() };
    let guarded = guard::deliver(&caller, checkers, Author { model: &model, owner: "Asha Rao", persona: "" }, &draft, &entries, &|_, _| {}).await.unwrap_or_else(|_| guard::withheld(&entries));
    let spent_usd = caller.spend().cost_usd;
    hub.retire_spend(spent_usd - ERRAND_CAP_USD);
    Json(Answer {
        agent,
        message: guarded.delivered.message.clone(),
        correction: guarded.correction,
        rewrites: guarded.drafts.len().saturating_sub(1),
        check: guarded.delivered,
        recalled,
        refs,
        spent_usd,
    })
    .into_response()
}

pub(crate) async fn keep_fact(State(hub): State<Arc<Hub>>, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    let Some(who) = may_run_office(&hub, &headers) else { return not_owner() };
    match hub.memory.resolve(id, &who) {
        Ok(fact) => Json(fact).into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, "no such fact in the office's memory"),
    }
}

/// Undo a retirement: the fact is live again (or a conflict, if it disagrees).
pub(crate) async fn restore_fact(State(hub): State<Arc<Hub>>, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    if may_run_office(&hub, &headers).is_none() {
        return not_owner();
    }
    match hub.memory.restore(id) {
        Ok(fact) => Json(fact).into_response(),
        Err(_) => error(StatusCode::NOT_FOUND, "no retired fact with that id"),
    }
}

pub(crate) async fn delete_fact(State(hub): State<Arc<Hub>>, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    if !authorized(&hub, &headers) {
        return unauthorized();
    }
    if may_run_office(&hub, &headers).is_none() {
        return not_owner();
    }
    match hub.memory.delete_fact(id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "no such fact"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
