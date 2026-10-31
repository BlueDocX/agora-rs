//! One run: plan a brief, do each task, check each result, assemble the deliverable.
//!
//! A task's result is accepted only when a verifier finds every acceptance criterion
//! met. A rejected draft gets one retry with the exact failed criteria; if it is
//! still rejected, the task is left for a person and only the tasks that depend on
//! it are skipped.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agora_core::Team;
use agora_muse::{Effort, Host, Message, MuseClient, MuseError, Request, host_of};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::budget::{Budget, BudgetError, Spend};
use crate::call::{CallError, Caller};
use crate::events::{EventSender, ReviewedBy, RunEvent, TaskView, emit};
use crate::journal::Journal;
use crate::planner::{self, Planned, PlannerError, Task};
use crate::scheduler::{Outcome, run_ready_queue};
use crate::CHARTER;

const WORKER_ROLE: &str = "You are a specialist. Do the one task you are given, \
using the inputs from finished tasks where relevant. Return only the result itself, \
in Markdown, with no preamble and no notes about your process.";

const VERIFIER_ROLE: &str = "You are the reviewer. Check the result against each \
acceptance criterion, strictly and literally. List every criterion that is not fully \
met, copied word for word. Say your confidence is low only if a criterion is \
ambiguous or cannot be judged from the text.";

#[derive(Debug, Clone)]
pub struct RunConfig {
    /// The planner, and every worker whose agent doesn't name its own model.
    pub model: String,
    pub max_parallel: usize,
    /// Spending cap in USD; `f64::INFINITY` for none.
    pub cap_usd: f64,
    /// The reviewer's model (e.g. the open weights on Baseten). `None` = `model`.
    pub reviewer_model: Option<String>,
    /// Baseten, for models it serves. `None` = those models can't be used.
    pub baseten: Option<MuseClient>,
    /// Live progress for watchers. `None` = nobody is watching.
    pub events: Option<EventSender>,
    /// Who does the work: each task goes to one member, who works in its role.
    /// `None` = anonymous specialists. Check it with [`Team::validate`] first.
    pub team: Option<Team>,
}

#[derive(Debug)]
pub enum TaskError {
    Call(CallError),
    /// The reviewer still rejected it after a retry. Needs a person.
    Rejected { failed_criteria: Vec<String> },
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Call(e) => write!(f, "{e}"),
            Self::Rejected { failed_criteria } => write!(f, "rejected: {}", failed_criteria.join("; ")),
        }
    }
}

/// Why a run could not start or could not plan. Task failures are not run errors:
/// they are reported per task in [`RunReport`].
#[derive(Debug)]
pub enum RunError {
    Budget(BudgetError),
    /// A model the run would use has no host configured (e.g. Baseten without a key).
    NoHost(String),
    /// `max_parallel` must be at least 1.
    NoWorkers,
    Plan(PlannerError),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => write!(f, "{e}"),
            Self::NoHost(m) => write!(f, "model \"{m}\" needs a host this office doesn't have (is its API key set?)"),
            Self::NoWorkers => write!(f, "max_parallel must be at least 1"),
            Self::Plan(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RunError {}

#[derive(Debug, Clone)]
pub struct Accepted {
    pub text: String,
    /// 1 = accepted first time, 2 = accepted after the retry.
    pub drafts: u32,
    pub reviews: u32,
}

#[derive(Debug)]
pub struct RunReport {
    pub planned: Planned,
    pub outcomes: Vec<Outcome<Accepted, TaskError>>,
    pub spend: Spend,
    /// Calls answered from the journal instead of the model (free now; already paid).
    pub replayed: u32,
    /// Replies the journal failed to store: used, but not replayable on resume.
    pub journal_write_failures: u32,
    /// Set when the run is journaled; pass it to resume.
    pub run_id: Option<String>,
    pub elapsed: Duration,
}

impl RunReport {
    /// The final results, in dependency order. An accepted result is left out only
    /// when every task that built on it was accepted too (an accepted "assemble" task
    /// replaces its inputs); if any of them failed, it is shown so nothing is lost.
    /// A complete run with one final result shows it as is; a partial run says what
    /// is missing.
    #[must_use]
    pub fn deliverable(&self) -> String {
        let plan = &self.planned.plan;
        let done = |i: usize| self.outcomes[i].is_done();
        let finals: Vec<(usize, &Accepted)> = plan
            .topo_order()
            .iter()
            .filter_map(|&i| match &self.outcomes[i] {
                Outcome::Done(a) if plan.dependents(i).is_empty() || !plan.dependents(i).iter().all(|&d| done(d)) => {
                    Some((i, a))
                }
                _ => None,
            })
            .collect();
        let missing: Vec<&str> =
            (0..plan.len()).filter(|&i| !done(i)).map(|i| self.planned.tasks[i].title.as_str()).collect();
        if let ([(_, only)], true) = (finals.as_slice(), missing.is_empty()) {
            return format!("{}\n", only.text.trim());
        }
        let mut out = String::new();
        for (i, accepted) in finals {
            let _ = write!(out, "## {}\n\n{}\n\n", self.planned.tasks[i].title, accepted.text.trim());
        }
        if !missing.is_empty() {
            let _ = writeln!(out, "_Not included, needs a person: {}._", missing.join("; "));
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Confidence {
    High,
    Low,
}

#[derive(Debug, Deserialize)]
struct Verdict {
    pass: bool,
    failed_criteria: Vec<String>,
    confidence: Confidence,
}

impl Verdict {
    /// A pass that still lists failures is not a pass.
    fn passed(&self) -> bool {
        self.pass && self.failed_criteria.is_empty()
    }
}

/// The majority decides. Accepted → no failures; otherwise every criterion any
/// reviewer flagged, or a generic reason if none was named.
fn tally(votes: &[Verdict]) -> Vec<String> {
    if votes.iter().filter(|v| v.passed()).count() * 2 > votes.len() {
        return Vec::new();
    }
    let mut failed: Vec<String> = votes.iter().flat_map(|v| v.failed_criteria.iter().cloned()).collect();
    failed.sort();
    failed.dedup();
    if failed.is_empty() {
        failed.push("the reviewer did not accept the result".into());
    }
    failed
}

fn verdict_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pass": {"type": "boolean"},
            "failed_criteria": {"type": "array", "items": {"type": "string"}},
            "confidence": {"type": "string", "enum": ["high", "low"]}
        },
        "required": ["pass", "failed_criteria", "confidence"],
        "additionalProperties": false
    })
}

fn bullets(items: &[String]) -> String {
    items.iter().map(|item| format!("- {item}")).collect::<Vec<_>>().join("\n")
}

struct Ctx {
    caller: Caller,
    model: String,
    reviewer_model: String,
    brief: String,
    tasks: Vec<Task>,
    team: Option<Team>,
    deps: Vec<Vec<usize>>,
    dependents: Vec<Vec<usize>>,
    /// Tasks already reported as skipped, so a task behind two failures is reported once.
    skipped: Mutex<Vec<bool>>,
    /// Accepted text per task, written before the task reports done, so a dependent
    /// task (which starts only after that) always finds its inputs.
    accepted: Mutex<Vec<Option<String>>>,
    events: Option<EventSender>,
}

impl Ctx {
    async fn call(&self, request: Request) -> Result<agora_muse::Response, TaskError> {
        self.caller.call(&request).await.map_err(TaskError::Call)
    }

    /// The model task `i` runs on: its agent's own, or the run's.
    fn worker_model(&self, i: usize) -> &str {
        let agent = self.team.as_ref().zip(self.tasks[i].agent.as_deref()).and_then(|(t, a)| t.agent(a));
        agent.and_then(|a| a.model.as_deref()).unwrap_or(&self.model)
    }

    fn task_prompt(&self, i: usize) -> String {
        let task = &self.tasks[i];
        let mut prompt = format!(
            "Brief: {}\n\nYour task: {}\n{}\n\nAcceptance criteria:\n{}\n",
            self.brief,
            task.title,
            task.instructions,
            bullets(&task.acceptance)
        );
        if task.max_words > 0 {
            let _ = writeln!(prompt, "- at most {} words", task.max_words);
        }
        for heading in &task.required_headings {
            let heading = heading.trim().trim_start_matches('#').trim();
            if !heading.is_empty() {
                let _ = writeln!(prompt, "- has a Markdown heading \"{heading}\" (written with #)");
            }
        }
        let accepted = self.accepted.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for &d in &self.deps[i] {
            if let Some(text) = &accepted[d] {
                let _ = write!(prompt, "\nInput from finished task \"{}\":\n{}\n", self.tasks[d].title, text);
            }
        }
        prompt
    }

    async fn review(&self, i: usize, draft: &str) -> Result<Verdict, TaskError> {
        let task = &self.tasks[i];
        let criteria = bullets(&task.acceptance);
        let request = Request::new(
            &self.reviewer_model,
            vec![
                Message::system(CHARTER),
                Message::system(VERIFIER_ROLE),
                Message::user(format!("Task: {}\n\nAcceptance criteria:\n{criteria}\n\nResult:\n{draft}", task.title)),
            ],
        )
        .effort(Effort::Low)
        .json_schema("Verdict", verdict_schema())
        // A verdict is tiny, but constrained decoding on some hosts can emit long
        // whitespace runs inside the JSON (seen live on Baseten); cap the damage.
        .max_output_tokens(8_000)
        .cache_key("agora-verifier");
        let response = self.call(request).await?;
        // Decoding is constrained to the schema, so a parse failure is an API fault.
        serde_json::from_str(&response.text).map_err(|e| TaskError::Call(CallError::Model(MuseError::Decode(e.to_string()))))
    }

    /// Review a draft. When the reviewer is unsure, take two more reviews and go
    /// with the majority. Returns the failed criteria (empty = accepted) and the
    /// number of reviews used.
    async fn check(&self, i: usize, draft: &str) -> Result<(Vec<String>, u32), TaskError> {
        if draft.trim().is_empty() {
            return Ok((vec!["the result is empty".into()], 0));
        }
        // Exact checks first: free, instant, and a model can't miscount.
        let broken = agora_core::checks::check(draft, &self.tasks[i].limits());
        if !broken.is_empty() {
            return Ok((broken, 0));
        }
        let first = self.review(i, draft).await?;
        if first.confidence == Confidence::High {
            return Ok((tally(&[first]), 1));
        }
        // Both extra reviews run to completion (and are charged) even if one fails;
        // a failed review is a missing vote, not a failed task.
        let (second, third) = tokio::join!(self.review(i, draft), self.review(i, draft));
        let mut votes = vec![first];
        let mut error = None;
        for review in [second, third] {
            match review {
                Ok(verdict) => votes.push(verdict),
                Err(e) => error = Some(e),
            }
        }
        if votes.len() == 1 {
            return Err(error.expect("both extra reviews failed"));
        }
        let used = u32::try_from(votes.len()).unwrap_or(u32::MAX);
        Ok((tally(&votes), used))
    }

    fn emit(&self, event: RunEvent) {
        emit(self.events.as_ref(), event);
    }

    fn spent(&self) -> f64 {
        let s = self.caller.budget.spend();
        s.cost_usd + s.replayed_usd
    }

    /// [`Self::attempt`], reporting how the task ended.
    async fn work(&self, i: usize) -> Result<Accepted, TaskError> {
        let task = self.tasks[i].id.clone();
        self.emit(RunEvent::TaskStarted { task: task.clone() });
        let result = self.attempt(i).await;
        match &result {
            Ok(a) => self.emit(RunEvent::TaskAccepted { task, drafts: a.drafts, reviews: a.reviews }),
            Err(e) => {
                self.emit(RunEvent::TaskFailed { task, reason: e.to_string() });
                self.report_skipped_behind(i);
            }
        }
        result
    }

    /// Everything downstream of a failed task will be skipped by the scheduler; say
    /// so now rather than when the whole run ends.
    fn report_skipped_behind(&self, failed: usize) {
        let mut skipped = self.skipped.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stack = self.dependents[failed].clone();
        while let Some(t) = stack.pop() {
            if !std::mem::replace(&mut skipped[t], true) {
                self.emit(RunEvent::TaskSkipped { task: self.tasks[t].id.clone() });
                stack.extend_from_slice(&self.dependents[t]);
            }
        }
    }

    /// Who is doing task `i`, in the words its instructions start with.
    fn persona(&self, i: usize) -> Option<String> {
        let team = self.team.as_ref()?;
        let agent = team.agent(self.tasks[i].agent.as_deref()?)?;
        Some(format!("You are {}, on the {} team. Your role: {}.{}", agent.name, team.name, agent.role, agent.briefing()))
    }

    async fn attempt(&self, i: usize) -> Result<Accepted, TaskError> {
        let task = &self.tasks[i].id;
        // The persona comes from the team, which a caller can write, so it travels in
        // the user message with the brief, never with system authority.
        let prompt = match self.persona(i) {
            Some(who) => format!("{who}\n\n{}", self.task_prompt(i)),
            None => self.task_prompt(i),
        };
        let mut input = vec![Message::system(CHARTER), Message::system(WORKER_ROLE), Message::user(prompt)];
        // A draft that only misses a code-checked limit doesn't use up the model
        // review's one retry; three drafts is the hard cap either way.
        let (mut drafts, mut reviews, mut model_rejections) = (0, 0, 0);
        loop {
            drafts += 1;
            let request = Request::new(self.worker_model(i), input.clone()).effort(Effort::Low).cache_key("agora-worker");
            let draft = self.call(request).await?.text;
            let words = agora_core::checks::word_count(&draft);
            self.emit(RunEvent::Drafted { task: task.clone(), draft: drafts, words, spent_usd: self.spent() });
            let (failed, used) = self.check(i, &draft).await?;
            reviews += used;
            let by = match used {
                0 => ReviewedBy::Code,
                1 => ReviewedBy::Model,
                _ => ReviewedBy::Vote,
            };
            self.emit(RunEvent::Reviewed {
                task: task.clone(),
                draft: drafts,
                unmet: failed.clone(),
                by,
                spent_usd: self.spent(),
            });
            if failed.is_empty() {
                self.accepted.lock().unwrap_or_else(std::sync::PoisonError::into_inner)[i] = Some(draft.clone());
                return Ok(Accepted { text: draft, drafts, reviews });
            }
            if used > 0 {
                model_rejections += 1;
            }
            if model_rejections == 2 || drafts == 3 {
                return Err(TaskError::Rejected { failed_criteria: failed });
            }
            input.push(Message::assistant(draft));
            input.push(Message::user(format!(
                "This result does not meet these criteria yet:\n{}\nFix exactly these and return the full corrected result.",
                bullets(&failed)
            )));
        }
    }
}

impl RunConfig {
    /// Every model the run may call (planner, reviewer, each agent's own), each
    /// checked to have a host this config can reach. Call it before journaling a run.
    ///
    /// # Errors
    /// [`RunError::NoHost`] for the first model without a reachable host.
    pub fn check_hosts(&self) -> Result<Vec<&str>, RunError> {
        let mut models: Vec<&str> = vec![&self.model];
        models.extend(self.reviewer_model.as_deref());
        models.extend(self.team.iter().flat_map(|t| &t.agents).filter_map(|a| a.model.as_deref()));
        models.sort_unstable();
        models.dedup();
        let reachable = |m: &str| match host_of(m) {
            Some(Host::Meta) => true,
            Some(Host::Baseten) => self.baseten.is_some(),
            None => false,
        };
        match models.iter().find(|m| !reachable(m)) {
            Some(m) => Err(RunError::NoHost((*m).to_string())),
            None => Ok(models),
        }
    }
}

/// Plan the brief, run every task, and return what happened.
///
/// With a `journal`, every completed reply is stored as it arrives, and a run
/// resumed from the same journal replays them instead of paying again.
///
/// # Errors
/// [`RunError`] for an unusable config or a failed plan. Task failures are reported
/// per task in the report.
pub async fn run(muse: MuseClient, config: &RunConfig, brief: &str, journal: Option<Journal>) -> Result<RunReport, RunError> {
    let started = Instant::now();
    if config.max_parallel == 0 {
        return Err(RunError::NoWorkers);
    }
    let reviewer_model = config.reviewer_model.clone().unwrap_or_else(|| config.model.clone());
    let models = config.check_hosts()?;
    let budget = Budget::new(&models, config.cap_usd).map_err(RunError::Budget)?;
    let caller = Caller { muse, baseten: config.baseten.clone(), budget, journal };
    if let Some(team) = &config.team {
        emit(config.events.as_ref(), RunEvent::Staffed { team: team.clone() });
    }
    let planned = planner::plan(&caller, &config.model, brief, config.team.as_ref()).await.map_err(RunError::Plan)?;
    let views = planned
        .tasks
        .iter()
        .map(|t| TaskView { id: t.id.clone(), title: t.title.clone(), deps: t.deps.clone(), agent: t.agent.clone() })
        .collect();
    emit(config.events.as_ref(), RunEvent::Planned { tasks: views, attempts: planned.attempts });

    let plan = &planned.plan;
    let deps = (0..plan.len()).map(|i| plan.deps(i).to_vec()).collect();
    let dependents = (0..plan.len()).map(|i| plan.dependents(i).to_vec()).collect();
    let ctx = Arc::new(Ctx {
        caller,
        model: config.model.clone(),
        reviewer_model,
        brief: brief.to_string(),
        tasks: planned.tasks.clone(),
        team: config.team.clone(),
        deps,
        dependents,
        skipped: Mutex::new(vec![false; plan.len()]),
        accepted: Mutex::new(vec![None; plan.len()]),
        events: config.events.clone(),
    });
    let priority = plan.remaining_path(&vec![1.0; plan.len()]);
    let outcomes = run_ready_queue(plan, &priority, config.max_parallel, |i| {
        let ctx = Arc::clone(&ctx);
        async move { ctx.work(i).await }
    })
    .await;

    let journal = ctx.caller.journal.as_ref();
    let report = RunReport {
        spend: ctx.caller.budget.spend(),
        replayed: journal.map_or(0, Journal::replayed),
        journal_write_failures: journal.map_or(0, Journal::write_failures),
        run_id: journal.map(|j| j.run_id().to_string()),
        planned,
        outcomes,
        elapsed: started.elapsed(),
    };
    ctx.emit(RunEvent::Finished {
        // The same total the live `spent_usd` figures show (replayed calls included).
        cost_usd: report.spend.cost_usd + report.spend.replayed_usd,
        calls: report.spend.calls,
        replayed: report.replayed,
        elapsed_ms: u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
        deliverable: report.deliverable(),
    });
    Ok(report)
}
