//! The one way the engine calls the model: check the budget, call, charge, and
//! refuse a reply that did not finish.

use agora_muse::{MuseClient, MuseError, Request, Response};

use crate::budget::{Budget, OverBudget};

#[derive(Debug)]
pub enum CallError {
    Model(MuseError),
    OverBudget(OverBudget),
    /// The reply was cut off or failed (e.g. status `incomplete` at the output limit).
    /// It is still charged.
    Unfinished { status: String },
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model(e) => write!(f, "{e}"),
            Self::OverBudget(o) => write!(f, "budget spent (${:.4} of ${:.4})", o.spent_usd, o.cap_usd),
            Self::Unfinished { status } => write!(f, "the model's reply did not finish (status \"{status}\")"),
        }
    }
}

impl std::error::Error for CallError {}

pub(crate) async fn call(muse: &MuseClient, budget: &Budget, request: &Request) -> Result<Response, CallError> {
    budget.check().map_err(CallError::OverBudget)?;
    let response = muse.respond(request).await.map_err(CallError::Model)?;
    budget.charge(&response);
    if response.status != "completed" {
        return Err(CallError::Unfinished { status: response.status });
    }
    Ok(response)
}
