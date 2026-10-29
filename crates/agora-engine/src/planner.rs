//! Brief → validated plan.
//!
//! The model answers in a fixed JSON schema (constrained decoding), then
//! [`Plan::new`] checks the graph. An invalid graph goes back to the model with the
//! exact error, up to [`MAX_ATTEMPTS`] times.

use std::fmt::Write as _;

use agora_core::{Limits, Plan, PlanError, TaskSpec, Team};
use agora_muse::{Effort, Message, Request, Role};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::CHARTER;
use crate::call::{CallError, Caller, Route};

const MAX_ATTEMPTS: usize = 3;
pub const MAX_TASKS: usize = 8;

const PLANNER_ROLE: &str = "You are the planner. Split the brief into at most 8 tasks that \
specialists can do independently. Give each task a short kebab-case id, a title, \
instructions a specialist can follow without seeing the other tasks, the ids it \
depends on (only when it truly needs their output), and 2-4 acceptance criteria a \
reviewer can check by reading the result. When the brief implies a length, set \
max_words for the task (0 = no limit). When the result needs particular sections, list \
their heading text in required_headings, without # marks (empty = none). These two are checked exactly by \
code, so don't repeat them as acceptance criteria. Prefer wide plans over long chains.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub instructions: String,
    pub deps: Vec<String>,
    pub acceptance: Vec<String>,
    /// 0 = no limit. Checked by code, not by a model.
    pub max_words: u32,
    /// Markdown headings the result must contain. Checked by code.
    pub required_headings: Vec<String>,
    /// The team member doing it (an agent id). `None` when the run has no team.
    #[serde(default)]
    pub agent: Option<String>,
}

impl Task {
    #[must_use]
    pub fn limits(&self) -> Limits {
        Limits { max_words: self.max_words, required_headings: self.required_headings.clone() }
    }
}

#[derive(Deserialize)]
struct Draft {
    tasks: Vec<Task>,
}

/// A validated plan with the task details, both indexed the same way.
#[derive(Debug, Clone)]
pub struct Planned {
    pub plan: Plan,
    pub tasks: Vec<Task>,
    /// How many planner calls it took (1 = valid first time).
    pub attempts: usize,
}

#[derive(Debug)]
pub enum PlannerError {
    Call(CallError),
    /// Still invalid after [`MAX_ATTEMPTS`]; holds the last problem.
    Invalid(String),
}

impl std::fmt::Display for PlannerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Call(e) => write!(f, "planner call failed: {e}"),
            Self::Invalid(why) => write!(f, "no valid plan after {MAX_ATTEMPTS} attempts: {why}"),
        }
    }
}

impl std::error::Error for PlannerError {}

fn schema(team: Option<&Team>) -> Value {
    let strings = json!({"type": "array", "items": {"type": "string"}});
    let mut task = json!({
        "type": "object",
        "properties": {
            "id": {"type": "string"},
            "title": {"type": "string"},
            "instructions": {"type": "string"},
            "deps": strings,
            "acceptance": strings,
            "max_words": {"type": "integer", "minimum": 0},
            "required_headings": strings
        },
        "required": ["id", "title", "instructions", "deps", "acceptance", "max_words", "required_headings"],
        "additionalProperties": false
    });
    // With a team, every task names who does it, and only a real member can be named.
    if let Some(team) = team {
        let ids: Vec<&str> = team.agents.iter().map(|a| a.id.as_str()).collect();
        task["properties"]["agent"] = json!({"type": "string", "enum": ids});
        task["required"].as_array_mut().expect("required is an array").push(json!("agent"));
    }
    json!({
        "type": "object",
        "properties": {"tasks": {"type": "array", "items": task}},
        "required": ["tasks"],
        "additionalProperties": false
    })
}

/// The team, as the planner sees it.
fn roster(team: &Team) -> String {
    let mut out = format!(
        "Your team is {} ({}). Give each task to the one member whose role fits it best, \
         by id in the agent field; spread the work when roles allow.\n",
        team.name, team.kind
    );
    for a in &team.agents {
        let _ = writeln!(out, "- {}: {}, {}", a.id, a.name, a.role);
    }
    out
}

/// Check a draft beyond what the graph checks: size, reviewable criteria, and
/// (with a team) that each task went to a member.
fn validate(tasks: Vec<Task>, team: Option<&Team>) -> Result<(Plan, Vec<Task>), String> {
    if tasks.len() > MAX_TASKS {
        return Err(format!("the plan has {} tasks; use at most {MAX_TASKS}", tasks.len()));
    }
    if let Some(t) = tasks.iter().find(|t| t.acceptance.is_empty()) {
        return Err(format!("task \"{}\" has no acceptance criteria", t.id));
    }
    if let Some(team) = team
        && let Some(t) = tasks.iter().find(|t| t.agent.as_deref().and_then(|a| team.agent(a)).is_none())
    {
        return Err(format!("task \"{}\" must go to one of the team's agent ids", t.id));
    }
    let specs: Vec<TaskSpec> = tasks.iter().map(|t| TaskSpec { id: t.id.clone(), deps: t.deps.clone() }).collect();
    let plan = Plan::new(&specs).map_err(|e: PlanError| e.to_string())?;
    Ok((plan, tasks))
}

/// # Errors
/// A model or budget error, or [`PlannerError::Invalid`] if the model never produced
/// a valid plan.
pub(crate) async fn plan(caller: &Caller, model: &str, brief: &str, team: Option<&Team>) -> Result<Planned, PlannerError> {
    // The roster is caller-written text (custom teams), so it is part of the user's
    // message, not a system instruction.
    let ask = match team {
        Some(team) => format!("{}\nBrief: {brief}", roster(team)),
        None => brief.to_string(),
    };
    let mut input = vec![Message::system(CHARTER), Message::system(PLANNER_ROLE), Message::user(ask)];
    let mut last_problem = String::new();
    for attempt in 1..=MAX_ATTEMPTS {
        let request = Request::new(model, input.clone())
            .effort(Effort::Medium)
            .json_schema("Plan", schema(team))
            .cache_key("agora-planner");
        let response = caller.call(Route::Main, &request).await.map_err(PlannerError::Call)?;
        let checked = serde_json::from_str::<Draft>(&response.text)
            .map_err(|e| format!("the reply is not valid plan JSON ({e})"))
            .and_then(|draft| validate(draft.tasks, team));
        match checked {
            Ok((plan, tasks)) => return Ok(Planned { plan, tasks, attempts: attempt }),
            Err(problem) => {
                input.push(Message { role: Role::Assistant, content: response.text });
                input.push(Message::user(format!("That plan is invalid: {problem}. Return a corrected plan.")));
                last_problem = problem;
            }
        }
    }
    Err(PlannerError::Invalid(last_problem))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, deps: &[&str]) -> Task {
        Task {
            id: id.into(),
            title: id.into(),
            instructions: String::new(),
            deps: deps.iter().map(|d| (*d).to_string()).collect(),
            acceptance: vec!["is checked".into()],
            max_words: 0,
            required_headings: Vec::new(),
            agent: None,
        }
    }

    #[test]
    fn rejects_too_many_tasks_and_missing_criteria() {
        let many: Vec<Task> = (0..=MAX_TASKS).map(|i| task(&format!("t{i}"), &[])).collect();
        assert!(validate(many, None).unwrap_err().contains("at most"));
        let mut t = task("a", &[]);
        t.acceptance.clear();
        assert!(validate(vec![t], None).unwrap_err().contains("no acceptance criteria"));
    }

    #[test]
    fn with_a_team_every_task_goes_to_a_member() {
        let team = agora_core::presets().remove(0);
        let mut t = task("a", &[]);
        assert!(validate(vec![t.clone()], Some(&team)).unwrap_err().contains("agent ids"));
        t.agent = Some("nobody".into());
        assert!(validate(vec![t.clone()], Some(&team)).unwrap_err().contains("agent ids"));
        t.agent = Some(team.agents[0].id.clone());
        assert!(validate(vec![t], Some(&team)).is_ok());
        assert_eq!(schema(Some(&team))["properties"]["tasks"]["items"]["properties"]["agent"]["enum"][0], team.agents[0].id);
        assert!(schema(None)["properties"]["tasks"]["items"]["properties"].get("agent").is_none());
    }

    #[test]
    fn passes_graph_errors_through_in_words() {
        let err = validate(vec![task("a", &["b"]), task("b", &["a"])], None).unwrap_err();
        assert!(err.contains("cycle"), "{err}");
    }
}
