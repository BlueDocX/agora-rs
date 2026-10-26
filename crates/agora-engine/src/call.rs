//! The one way the engine calls the model: replay from the journal if this call
//! already completed, otherwise check the budget, call, charge, refuse a reply that
//! did not finish, and journal it before anything acts on it.

use agora_muse::{MuseClient, MuseError, Request, Response};

use crate::budget::{Budget, OverBudget};
use crate::journal::{Journal, JournalError};

#[derive(Debug)]
pub enum CallError {
    Model(MuseError),
    OverBudget(OverBudget),
    /// The reply was cut off or failed (e.g. status `incomplete` at the output limit).
    /// It is still charged.
    Unfinished { status: String },
    Journal(JournalError),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model(e) => write!(f, "{e}"),
            Self::OverBudget(o) => write!(f, "budget spent (${:.4} of ${:.4})", o.spent_usd, o.cap_usd),
            Self::Unfinished { status } => write!(f, "the model's reply did not finish (status \"{status}\")"),
            Self::Journal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CallError {}

#[derive(Debug)]
pub(crate) struct Caller {
    pub muse: MuseClient,
    pub budget: Budget,
    pub journal: Option<Journal>,
}

impl Caller {
    /// Replayed replies cost nothing now but count against the cap: the run already
    /// paid for them.
    pub async fn call(&self, request: &Request) -> Result<Response, CallError> {
        let claim = self.journal.as_ref().map(|j| (j, j.claim(request)));
        if let Some((journal, (key, n))) = &claim
            && let Some(response) = journal.replay(key, *n).map_err(CallError::Journal)?
        {
            self.budget.charge_replayed(&response);
            return Ok(response);
        }
        self.budget.check().map_err(CallError::OverBudget)?;
        let response = self.muse.respond(request).await.map_err(CallError::Model)?;
        self.budget.charge(&response);
        if response.status != "completed" {
            return Err(CallError::Unfinished { status: response.status });
        }
        // A paid, finished reply is never thrown away because the journal couldn't
        // store it; the run only loses the ability to replay that one call.
        if let Some((journal, (key, n))) = &claim {
            journal.record(key, *n, &response);
        }
        Ok(response)
    }
}
