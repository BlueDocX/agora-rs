//! The one way the engine calls the model: replay from the journal if this call
//! already completed, otherwise check the budget, call, charge, refuse a reply that
//! did not finish, and journal it before anything acts on it.

use agora_muse::{Host, MuseClient, MuseError, Request, Response, host_of};

use crate::budget::{Budget, OverBudget};
use crate::journal::{Journal, JournalError};

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
}

impl Caller {
    /// A caller with no journal (nothing to resume).
    #[must_use]
    pub fn new(muse: MuseClient, baseten: Option<MuseClient>, budget: Budget) -> Self {
        Self { muse, baseten, budget, journal: None }
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
            self.budget.charge_replayed(&request.model, &response);
            return Ok(response);
        }
        self.budget.check().map_err(CallError::OverBudget)?;
        let client = match host_of(&request.model) {
            Some(Host::Meta) => &self.muse,
            Some(Host::Baseten) => self.baseten.as_ref().ok_or_else(|| CallError::NoHost(request.model.clone()))?,
            None => return Err(CallError::NoHost(request.model.clone())),
        };
        let response = client.respond(request).await.map_err(CallError::Model)?;
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
