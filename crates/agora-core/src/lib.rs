//! Domain types for the Agora office. Pure code: no I/O, no async.

pub mod checks;
pub mod plan;

pub use checks::Limits;
pub use plan::{Plan, PlanError, TaskSpec};
