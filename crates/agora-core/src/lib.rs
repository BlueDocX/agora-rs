//! Domain types for the Agora office. Pure code: no I/O, no async.

pub mod plan;

pub use plan::{Plan, PlanError, TaskSpec};
