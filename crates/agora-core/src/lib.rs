//! Domain types for the Agora office. Pure code: no I/O, no async.

pub mod checks;
pub mod plan;
pub mod team;
pub mod world;

pub use checks::Limits;
pub use plan::{Plan, PlanError, TaskSpec};
pub use team::{Agent, Team, preset, presets};
pub use world::{Action, Outcome, TOOLS, Tool, Weight, World};
