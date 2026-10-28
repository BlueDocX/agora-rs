//! Per-token prices from dev.meta.ai/docs/pricing-rate-limits (read 2026-10-26) and,
//! for open weights, from the hosting partner.

use crate::Usage;

/// US dollars per 1M tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub cached_input: f64,
    pub output: f64,
}

impl Price {
    /// Exact cost of one call. Cached tokens are a subset of input tokens and are
    /// billed at the cached rate; reasoning tokens are part of output tokens.
    #[must_use]
    pub fn cost_usd(&self, usage: &Usage) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let (input, cached, output) =
            (usage.input_tokens as f64, usage.cached_tokens as f64, usage.output_tokens as f64);
        ((input - cached) * self.input + cached * self.cached_input + output * self.output) / 1_000_000.0
    }
}

/// The price for a model id, or `None` for a model without a published token price.
#[must_use]
pub fn price_for(model: &str) -> Option<Price> {
    match model {
        // Muse Spark Open on Baseten is priced the same (baseten.co/library/muse-spark-open,
        // read 2026-10-29).
        "muse-spark-1.3"
        | "muse-spark-1.2"
        | "muse-spark-1.1"
        | "meta-models/muse-spark-open"
        | "meta-models/muse-spark-open-1026" => Some(Price { input: 1.25, cached_input: 0.15, output: 4.25 }),
        "muse-spark-1.3-contributor" | "muse-spark-1.2-contributor" => {
            Some(Price { input: 0.10, cached_input: 0.002, output: 0.20 })
        }
        "muse-wonder-1.0" => Some(Price { input: 4.00, cached_input: 0.20, output: 20.00 }),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cached_tokens_are_billed_at_the_cached_rate() {
        let usage = Usage { input_tokens: 1_000_000, cached_tokens: 800_000, output_tokens: 100_000, reasoning_tokens: 0 };
        let cost = price_for("muse-spark-1.3").unwrap().cost_usd(&usage);
        // 200k × 1.25 + 800k × 0.15 + 100k × 4.25 = 0.25 + 0.12 + 0.425
        assert!((cost - 0.795).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn unknown_models_have_no_price() {
        assert!(price_for("sam-3.1").is_none());
    }
}
