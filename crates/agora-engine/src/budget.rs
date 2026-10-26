//! A spending cap shared by every call in a run.
//!
//! The check happens before each call and the charge after it, so a run can overshoot
//! the cap by at most the calls already in flight (one per worker slot).

use std::sync::Mutex;

use agora_muse::{Price, Response, Usage, price_for};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Spend {
    pub usage: Usage,
    pub cost_usd: f64,
    pub calls: u32,
}

#[derive(Debug)]
pub struct Budget {
    cap_usd: f64,
    price: Price,
    spent: Mutex<Spend>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BudgetError {
    /// An uncapped run is not allowed, so every model needs a published price.
    UnpricedModel(String),
    /// The cap must be a positive, finite number of dollars.
    InvalidCap(f64),
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnpricedModel(m) => write!(f, "no published price for model \"{m}\", so its spend can't be capped"),
            Self::InvalidCap(c) => write!(f, "the budget cap must be a positive number of dollars, not {c}"),
        }
    }
}

impl std::error::Error for BudgetError {}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverBudget {
    pub spent_usd: f64,
    pub cap_usd: f64,
}

impl Budget {
    /// # Errors
    /// [`BudgetError`] if `model` has no published price or `cap_usd` is not a
    /// positive finite number (NaN would never trip the cap).
    pub fn new(model: &str, cap_usd: f64) -> Result<Self, BudgetError> {
        if !(cap_usd.is_finite() && cap_usd > 0.0) {
            return Err(BudgetError::InvalidCap(cap_usd));
        }
        let price = price_for(model).ok_or_else(|| BudgetError::UnpricedModel(model.to_string()))?;
        Ok(Self { cap_usd, price, spent: Mutex::new(Spend::default()) })
    }

    /// # Errors
    /// [`OverBudget`] once the run has spent its cap.
    pub fn check(&self) -> Result<(), OverBudget> {
        let spent = self.spend().cost_usd;
        if spent >= self.cap_usd { Err(OverBudget { spent_usd: spent, cap_usd: self.cap_usd }) } else { Ok(()) }
    }

    pub fn charge(&self, response: &Response) {
        let cost = self.price.cost_usd(&response.usage);
        let mut spent = self.spent.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        spent.usage += response.usage;
        spent.cost_usd += cost;
        spent.calls += 1;
    }

    #[must_use]
    pub fn spend(&self) -> Spend {
        *self.spent.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use agora_muse::RateLimit;

    fn response(input: u64, output: u64) -> Response {
        Response {
            id: String::new(),
            model: "muse-spark-1.3".into(),
            status: "completed".into(),
            text: String::new(),
            usage: Usage { input_tokens: input, output_tokens: output, ..Usage::default() },
            rate_limit: RateLimit::default(),
        }
    }

    #[test]
    fn stops_once_the_cap_is_reached() {
        let budget = Budget::new("muse-spark-1.3", 0.01).unwrap();
        assert!(budget.check().is_ok());
        budget.charge(&response(4_000, 1_000)); // 0.005 + 0.00425
        assert!(budget.check().is_ok());
        budget.charge(&response(4_000, 1_000));
        assert!(budget.check().is_err());
        assert_eq!(budget.spend().calls, 2);
    }

    #[test]
    fn refuses_an_unpriced_model_and_caps_that_cannot_cap() {
        assert!(matches!(Budget::new("some-unpriced-model", 1.0), Err(BudgetError::UnpricedModel(_))));
        for cap in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            assert!(matches!(Budget::new("muse-spark-1.3", cap), Err(BudgetError::InvalidCap(_))), "{cap}");
        }
    }
}
