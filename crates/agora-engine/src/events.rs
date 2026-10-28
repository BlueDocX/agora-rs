//! What a run reports as it happens, for anything watching it live (the office UI,
//! the glasses view). Sending never blocks the run, and a run with no watcher, or
//! whose watcher went away, is unaffected.

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskView {
    pub id: String,
    pub title: String,
    pub deps: Vec<String>,
}

/// How a draft was judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewedBy {
    /// Word limits / required headings / empty, checked in code. Free.
    Code,
    /// One model review.
    Model,
    /// The first reviewer was unsure, so three reviews voted.
    Vote,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    Planned { tasks: Vec<TaskView>, attempts: usize },
    TaskStarted { task: String },
    Drafted { task: String, draft: u32, words: usize, spent_usd: f64 },
    Reviewed { task: String, draft: u32, unmet: Vec<String>, by: ReviewedBy, spent_usd: f64 },
    TaskAccepted { task: String, drafts: u32, reviews: u32 },
    /// Left for a person (or failed outright); `reason` says which.
    TaskFailed { task: String, reason: String },
    /// Not run, because a task it needs failed.
    TaskSkipped { task: String },
    /// `cost_usd` is the run's total, including calls replayed from the journal.
    Finished { cost_usd: f64, calls: u32, replayed: u32, elapsed_ms: u64, deliverable: String },
    /// The run could not start or could not plan. Sent by whoever ran it, since
    /// `run` reports this as its error.
    RunFailed { reason: String },
}

pub type EventSender = UnboundedSender<RunEvent>;

pub(crate) fn emit(events: Option<&EventSender>, event: RunEvent) {
    if let Some(tx) = events {
        let _ = tx.send(event); // a watcher that left is not the run's problem
    }
}
