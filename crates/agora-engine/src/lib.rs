//! The Agora engine: plans a brief, schedules the tasks, checks every result.

pub mod alibi;
pub mod budget;
pub mod call;
pub mod chat;
pub mod connected;
pub mod errand;
pub mod events;
pub mod guard;
pub mod journal;
pub mod ledger;
pub mod memory;
pub mod planner;
pub mod run;
pub mod scheduler;
pub mod signing;

pub use budget::{Budget, BudgetError, OverBudget, Spend};
pub use call::{CallError, Caller};
pub use events::{EventSender, ReviewedBy, RunEvent, TaskView};
pub use journal::{Journal, JournalError, RunRecord};
pub use ledger::{Broken, Entry, Ledger};
pub use planner::{Planned, PlannerError, Task};
pub use run::{Accepted, RunConfig, RunError, RunReport, TaskError, run};
pub use scheduler::{Outcome, run_ready_queue, run_waves};
pub use signing::{SignedHead, Signer, random_hex};

/// The first message of every call, identical across roles so its tokens are cached.
pub const CHARTER: &str = "You work in Agora, a virtual office where AI \
specialists and people deliver work together. Every result is checked against \
written acceptance criteria before anyone builds on it, so be specific, complete \
and honest about what you could not do. Never invent facts, figures, names or \
sources; when something is unknown, say so plainly.";
