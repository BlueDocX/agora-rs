//! The one way the engine calls the model: replay from the journal if this call
//! already completed, otherwise check the budget, call, charge, refuse a reply that
//! did not finish, and journal it before anything acts on it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agora_muse::{Host, MuseClient, MuseError, RateLimit, Request, Response, Usage, host_of};
use serde::{Deserialize, Serialize};

use crate::budget::{Budget, OverBudget};
use crate::journal::{Journal, JournalError, request_key};

#[derive(Debug)]
pub enum CallError {
    Model(MuseError),
    OverBudget(OverBudget),
    /// The reply was cut off or failed (e.g. status `incomplete` at the output limit).
    /// It is still charged.
    Unfinished { status: String },
    /// The host's content filter stopped the reply: asking again won't help. It is
    /// still charged.
    Filtered { status: String },
    Journal(JournalError),
    /// The model is served by a host this run has no client for.
    NoHost(String),
    /// A replayed run asked for a reply its tape doesn't hold.
    Replay(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model(e) => write!(f, "{e}"),
            Self::OverBudget(o) => write!(f, "budget spent (${:.4} of ${:.4})", o.spent_usd, o.cap_usd),
            Self::Unfinished { status } => write!(f, "the model's reply did not finish (status \"{status}\")"),
            Self::Filtered { status } => write!(f, "the model host's content filter stopped the reply (status \"{status}\")"),
            Self::Journal(e) => write!(f, "{e}"),
            Self::NoHost(m) => write!(f, "no host is configured for model \"{m}\""),
            Self::Replay(why) => write!(f, "replay: {why}"),
        }
    }
}

impl std::error::Error for CallError {}

/// Calls models for one piece of work, routing each to its host and counting the
/// spend against one budget.
#[derive(Debug)]
pub struct Caller {
    /// Meta's Model API.
    pub(crate) muse: MuseClient,
    /// Baseten, for the open weights, if configured.
    pub(crate) baseten: Option<MuseClient>,
    pub(crate) budget: Budget,
    pub(crate) journal: Option<Journal>,
    pub(crate) tape: Option<Arc<Tape>>,
}

/// Every model reply of a run, in order, so the run can be replayed with no model at
/// all (tests, CI). A reply is found by (prompt, model, how many times that pair was
/// asked), not by the request's exact bytes: a reworded prompt still replays, so the
/// replay checks what the code does with the same replies. A call with no reply left
/// is an error, and [`Tape::unused`] counts replies never asked for.
#[derive(Debug, Default)]
pub struct Tape {
    replaying: bool,
    calls: Mutex<Vec<Taped>>,
    /// Replay: how many replies of each (prompt, model) have been served.
    served: Mutex<HashMap<(String, String), usize>>,
    /// Replay: calls whose request differs from the recorded one (a changed prompt).
    changed: Mutex<usize>,
}

/// One recorded reply, unfinished ones too (the run saw them).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Taped {
    pub prompt: String,
    pub model: String,
    /// SHA-256 of the request body as sent.
    pub request: String,
    pub status: String,
    pub text: String,
    /// Input, cached, output and reasoning tokens.
    pub tokens: [u64; 4],
}

impl Tape {
    /// An empty tape that records.
    #[must_use]
    pub fn recording() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A tape that serves `calls` back.
    #[must_use]
    pub fn replaying(calls: Vec<Taped>) -> Arc<Self> {
        Arc::new(Self { replaying: true, calls: Mutex::new(calls), ..Self::default() })
    }

    /// The recorded calls.
    #[must_use]
    pub fn calls(&self) -> Vec<Taped> {
        lock(&self.calls).clone()
    }

    /// Replay: recorded replies no call asked for.
    #[must_use]
    pub fn unused(&self) -> usize {
        let served: usize = lock(&self.served).values().sum();
        lock(&self.calls).len() - served
    }

    /// Replay: calls served although their request differs from the recorded one.
    #[must_use]
    pub fn changed(&self) -> usize {
        *lock(&self.changed)
    }

    /// Record a reply, or (`Err`) a call that failed: the run saw that too, and a
    /// replay must fail the same call, not hand its reply to the next one.
    fn record(&self, request: &Request, response: Result<&Response, &MuseError>) {
        let (status, text, u) = match response {
            Ok(r) => (r.status.clone(), r.text.clone(), r.usage),
            Err(e) => (FAILED.to_string(), e.to_string(), Usage::default()),
        };
        lock(&self.calls).push(Taped {
            prompt: prompt_of(request),
            model: request.model.clone(),
            request: request_key(request),
            status,
            text,
            tokens: [u.input_tokens, u.cached_tokens, u.output_tokens, u.reasoning_tokens],
        });
    }

    fn replay(&self, request: &Request) -> Result<Response, CallError> {
        let key = (prompt_of(request), request.model.clone());
        let mut served = lock(&self.served);
        let n = served.entry(key.clone()).or_insert(0);
        let taped = lock(&self.calls)
            .iter()
            .filter(|t| (&t.prompt, &t.model) == (&key.0, &key.1))
            .nth(*n)
            .cloned()
            .ok_or_else(|| CallError::Replay(format!("the tape has no reply left for {} on {}", key.0, key.1)))?;
        *n += 1;
        if taped.request != request_key(request) {
            *lock(&self.changed) += 1;
        }
        if taped.status == FAILED {
            return Err(CallError::Model(MuseError::Transport(taped.text)));
        }
        let [input_tokens, cached_tokens, output_tokens, reasoning_tokens] = taped.tokens;
        Ok(Response {
            id: format!("tape-{}-{n}", key.0),
            model: taped.model,
            status: taped.status,
            text: taped.text,
            usage: Usage { input_tokens, cached_tokens, output_tokens, reasoning_tokens },
            rate_limit: RateLimit::default(),
        })
    }
}

/// The status a taped call that failed is recorded with.
const FAILED: &str = "call-failed";

fn prompt_of(request: &Request) -> String {
    request.prompt_cache_key.clone().unwrap_or_else(|| "a call".into())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Caller {
    /// A caller with no journal (nothing to resume).
    #[must_use]
    pub fn new(muse: MuseClient, baseten: Option<MuseClient>, budget: Budget) -> Self {
        Self { muse, baseten, budget, journal: None, tape: None }
    }

    /// This caller records every reply on `tape`, or (a replaying tape) answers every
    /// call from it and never reaches a model.
    #[must_use]
    pub fn with_tape(mut self, tape: Arc<Tape>) -> Self {
        self.tape = Some(tape);
        self
    }

    #[must_use]
    pub fn spend(&self) -> crate::Spend {
        self.budget.spend()
    }

    /// Replayed replies cost nothing now but count against the cap: the run already
    /// paid for them.
    ///
    /// # Errors
    /// [`CallError`] when the host fails, the budget is spent or the reply is cut off.
    pub async fn call(&self, request: &Request) -> Result<Response, CallError> {
        let claim = self.journal.as_ref().map(|j| (j, j.claim(request)));
        if let Some((journal, (key, n))) = &claim
            && let Some(response) = journal.replay(key, *n).map_err(CallError::Journal)?
        {
            if let Some(tape) = self.tape.as_deref().filter(|t| !t.replaying) {
                tape.record(request, Ok(&response));
            }
            self.budget.charge_replayed(&request.model, &response);
            return Ok(response);
        }
        self.budget.check().map_err(CallError::OverBudget)?;
        let response = match self.tape.as_deref() {
            Some(tape) if tape.replaying => tape.replay(request)?,
            tape => {
                let client = match host_of(&request.model) {
                    Some(Host::Meta) => &self.muse,
                    Some(Host::Baseten) => self.baseten.as_ref().ok_or_else(|| CallError::NoHost(request.model.clone()))?,
                    None => return Err(CallError::NoHost(request.model.clone())),
                };
                let response = client.respond(request).await;
                if let Some(tape) = tape {
                    tape.record(request, response.as_ref());
                }
                response.map_err(CallError::Model)?
            }
        };
        self.budget.charge(&request.model, &response);
        if response.status != "completed" {
            // Say which prompt it was, so a cut-off can be traced.
            let which = request.prompt_cache_key.as_deref().unwrap_or("a call");
            let status = format!("{} in {which} on {}", response.status, request.model);
            return Err(if response.status == "content_filter" { CallError::Filtered { status } } else { CallError::Unfinished { status } });
        }
        // A paid, finished reply is never thrown away because the journal couldn't
        // store it; the run only loses the ability to replay that one call.
        if let Some((journal, (key, n))) = &claim {
            journal.record(key, *n, &response);
        }
        Ok(response)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use agora_muse::Message;

    use super::*;

    fn taped(prompt: &str, status: &str, text: &str) -> Taped {
        Taped { prompt: prompt.into(), model: "m".into(), request: String::new(), status: status.into(), text: text.into(), tokens: [0; 4] }
    }

    #[test]
    fn a_tape_replays_in_order_fails_what_failed_and_counts_what_is_left() {
        let tape = Tape::replaying(vec![taped("p", "completed", "one"), taped("p", FAILED, "the host hung up"), taped("q", "completed", "other"), taped("p", "completed", "three")]);
        let ask = || Request::new("m", vec![Message::user("hi".to_string())]).cache_key("p");
        assert_eq!(tape.replay(&ask()).unwrap().text, "one");
        // The call that failed when recorded fails again, so the next reply isn't shifted onto it.
        assert!(matches!(tape.replay(&ask()), Err(CallError::Model(MuseError::Transport(t))) if t == "the host hung up"));
        assert_eq!(tape.replay(&ask()).unwrap().text, "three");
        assert!(matches!(tape.replay(&ask()), Err(CallError::Replay(_))), "no reply left is its own error");
        assert_eq!(tape.unused(), 1, "q was never asked for");
        assert_eq!(tape.changed(), 3, "the requests differ from the (empty) recorded digests");
    }
}
