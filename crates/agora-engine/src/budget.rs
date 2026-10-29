//! What a run spends, and its optional cap (`f64::INFINITY` = no cap: spend is
//! still counted and reported).
//!
//! The check happens before each call and the charge after it, so a run can overshoot
//! the cap by at most the calls already in flight (one per worker slot).

use std::collections::HashMap;
use std::sync::Mutex;

use agora_muse::{Price, Response, Usage, price_for};

/// What this process paid for. Replies replayed from the journal are counted in
/// `replayed_usd` instead: they still count against the cap (the run paid for them
/// earlier), so resuming never resets a run's budget.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Spend {
    pub usage: Usage,
    pub cost_usd: f64,
    pub calls: u32,
    pub replayed_usd: f64,
}

#[derive(Debug)]
pub struct Budget {
    cap_usd: f64,
    /// Every model the run may call, with its price. Checked up front.
    prices: HashMap<String, Price>,
    /// For a model that wasn't listed (a bug): the highest listed rates, so a
    /// mistake can only over-count, never make calls free.
    fallback: Price,
    spent: Mutex<Spend>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BudgetError {
    /// An uncapped run is not allowed, so every model needs a published price.
    UnpricedModel(String),
    /// The cap must be a positive number of dollars (or infinity for none).
    InvalidCap(f64),
    /// A budget must price at least one model.
    NoModels,
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnpricedModel(m) => write!(f, "no published price for model \"{m}\", so its spend can't be capped"),
            Self::InvalidCap(c) => write!(f, "the budget cap must be a positive number of dollars (or none), not {c}"),
            Self::NoModels => write!(f, "a budget needs at least one priced model"),
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
    /// One cap shared by every model the run uses; `f64::INFINITY` means no cap.
    ///
    /// # Errors
    /// [`BudgetError`] if any of `models` has no published price (spend must always
    /// be counted) or `cap_usd` is not positive (NaN, which would silently never trip,
    /// is rejected too).
    pub fn new(models: &[&str], cap_usd: f64) -> Result<Self, BudgetError> {
        if cap_usd.is_nan() || cap_usd <= 0.0 {
            return Err(BudgetError::InvalidCap(cap_usd));
        }
        if models.is_empty() {
            return Err(BudgetError::NoModels);
        }
        let prices: HashMap<String, Price> = models
            .iter()
            .map(|&m| price_for(m).map(|p| (m.to_string(), p)).ok_or_else(|| BudgetError::UnpricedModel(m.to_string())))
            .collect::<Result<_, _>>()?;
        let fallback = prices.values().fold(Price { input: 0.0, cached_input: 0.0, output: 0.0 }, |a, b| Price {
            input: a.input.max(b.input),
            cached_input: a.cached_input.max(b.cached_input),
            output: a.output.max(b.output),
        });
        Ok(Self { cap_usd, prices, fallback, spent: Mutex::new(Spend::default()) })
    }

    fn price(&self, model: &str) -> Price {
        self.prices.get(model).copied().unwrap_or(self.fallback)
    }

    /// # Errors
    /// [`OverBudget`] once the run has spent its cap.
    pub fn check(&self) -> Result<(), OverBudget> {
        let Spend { cost_usd, replayed_usd, .. } = self.spend();
        let total = cost_usd + replayed_usd;
        if total >= self.cap_usd { Err(OverBudget { spent_usd: total, cap_usd: self.cap_usd }) } else { Ok(()) }
    }

    /// Charge a reply to `model` (the model the request named).
    pub fn charge(&self, model: &str, response: &Response) {
        let cost = self.price(model).cost_usd(&response.usage);
        let mut spent = self.spent.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        spent.usage += response.usage;
        spent.cost_usd += cost;
        spent.calls += 1;
    }

    /// Count a reply replayed from the journal against the cap, not as a new call.
    pub fn charge_replayed(&self, model: &str, response: &Response) {
        let cost = self.price(model).cost_usd(&response.usage);
        self.spent.lock().unwrap_or_else(std::sync::PoisonError::into_inner).replayed_usd += cost;
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
        let budget = Budget::new(&["muse-spark-1.3"], 0.01).unwrap();
        assert!(budget.check().is_ok());
        budget.charge("muse-spark-1.3", &response(4_000, 1_000)); // 0.005 + 0.00425
        assert!(budget.check().is_ok());
        budget.charge("muse-spark-1.3", &response(4_000, 1_000));
        assert!(budget.check().is_err());
        assert_eq!(budget.spend().calls, 2);
    }

    #[test]
    fn each_model_is_charged_at_its_own_price() {
        let budget = Budget::new(&["muse-spark-1.3-contributor", "muse-wonder-1.0"], 1.0).unwrap();
        budget.charge("muse-spark-1.3-contributor", &response(1_000_000, 0)); // $0.10
        budget.charge("muse-wonder-1.0", &response(1_000_000, 0)); // $4.00 → only the cap's share matters
        assert!((budget.spend().cost_usd - 4.10).abs() < 1e-9);
        // An unlisted model is priced at the highest known rate.
        let before = budget.spend().cost_usd;
        budget.charge("not-listed", &response(0, 1_000_000));
        assert!((budget.spend().cost_usd - before - 20.0).abs() < 1e-9);
    }

    #[test]
    fn replayed_replies_count_against_the_cap() {
        let budget = Budget::new(&["muse-spark-1.3"], 0.01).unwrap();
        budget.charge_replayed("muse-spark-1.3", &response(8_000, 2_000)); // 0.0185
        assert!(budget.check().is_err());
        assert_eq!(budget.spend().calls, 0);
    }

    #[test]
    fn refuses_an_unpriced_model_and_caps_that_cannot_cap() {
        assert!(matches!(Budget::new(&["muse-spark-1.3", "some-unpriced-model"], 1.0), Err(BudgetError::UnpricedModel(_))));
        assert!(matches!(Budget::new(&[], 1.0), Err(BudgetError::NoModels)));
        // Infinity is allowed (no cap); NaN would never trip, so it is refused.
        for cap in [f64::NAN, f64::NEG_INFINITY, 0.0, -1.0] {
            assert!(matches!(Budget::new(&["muse-spark-1.3"], cap), Err(BudgetError::InvalidCap(_))), "{cap}");
        }
    }

    #[test]
    fn no_cap_never_stops_a_run_but_still_counts() {
        let budget = Budget::new(&["muse-spark-1.3"], f64::INFINITY).unwrap();
        for _ in 0..1000 {
            budget.charge("muse-spark-1.3", &response(1_000_000, 1_000_000));
        }
        assert!(budget.check().is_ok());
        assert!(budget.spend().cost_usd > 1000.0);
        assert!(Budget::new(&["muse-spark-1.3"], f64::NAN).is_err());
    }
}
