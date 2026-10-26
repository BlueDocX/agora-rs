//! The Agora engine: plans a brief, schedules the tasks, checks every result.

pub mod budget;
pub mod call;
pub mod journal;
pub mod planner;
pub mod run;
pub mod scheduler;

pub use budget::{Budget, BudgetError, OverBudget, Spend};
pub use call::CallError;
pub use journal::{Journal, JournalError, RunRecord};
pub use planner::{Planned, PlannerError, Task};
pub use run::{Accepted, RunConfig, RunError, RunReport, TaskError, run};
pub use scheduler::{Outcome, run_ready_queue, run_waves};

/// The first message of every call, identical across roles so its tokens are cached.
pub(crate) const CHARTER: &str = "You work in Agora, a virtual office where AI \
specialists and people deliver work together. Every result is checked against \
written acceptance criteria before anyone builds on it, so be specific, complete \
and honest about what you could not do. Never invent facts, figures, names or \
sources; when something is unknown, say so plainly.";
