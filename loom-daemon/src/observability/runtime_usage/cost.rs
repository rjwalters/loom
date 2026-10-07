//! USD estimates for `loom.runtime.usage` spans, from the daemon's own rate
//! card (Issue #9204, folded into #9303).
//!
//! One pricing table: the resync-delivered `.loom/pricing.json` asset when it
//! loaded cleanly, else the card compiled into this build
//! ([`crate::activity::pricing_card`], [`ModelPricing`]). Each model row is
//! priced with its 5-minute and 1-hour cache writes at their own rates.
//!
//! **A model the card does not know gets no estimate.** `ModelPricing::for_model`
//! prices an unknown id as Sonnet; that fallback is a fabricated number, and
//! "unknown is never exported as a number" is the rule these spans follow, so
//! this module uses the `Option`-returning lookup and omits the USD attributes
//! instead. The estimate also ignores `speed`/`service_tier` (the card has no
//! per-tier rows), which the schema docs say.

use crate::activity::pricing_card::{self, PricingCard};
use crate::activity::resource_usage::ModelPricing;
use crate::script_helpers::sweep_experiment::ModelUsageTotals;
use crate::telemetry::trace::TraceAttributes;

/// The date the **compiled** rate card was last verified. Held equal to the
/// shipped `defaults/pricing.json`'s `verified_on` by a test, since the two
/// cards are themselves held in agreement.
pub const COMPILED_VERIFIED_ON: &str = "2026-09-18";

/// `true` the first time this process sees `model` unpriced, so the WARN fires
/// once per model rather than once per usage span (#10750).
fn first_unpriced_sighting(model: &str) -> bool {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
        std::sync::Mutex::new(None);
    let mut guard = SEEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .get_or_insert_with(std::collections::HashSet::new)
        .insert(model.to_string())
}

/// The rate card an estimate is computed from, and its reproducible identity.
#[derive(Debug, Clone, Copy)]
pub struct Pricing<'a> {
    /// `None` = the compiled card.
    card: Option<&'a PricingCard>,
}

impl Pricing<'static> {
    /// The card in effect for this process.
    #[must_use]
    pub fn active() -> Self {
        Self {
            card: pricing_card::active(),
        }
    }
}

impl<'a> Pricing<'a> {
    /// An explicit card (`None` = compiled), for tests.
    #[must_use]
    pub fn with(card: Option<&'a PricingCard>) -> Self {
        Self { card }
    }

    /// `asset` or `compiled` (`loom.pricing.source`).
    #[must_use]
    pub fn source(&self) -> &'static str {
        if self.card.is_some() {
            "asset"
        } else {
            "compiled"
        }
    }

    /// The card's `verified_on` date (`loom.pricing.verified_on`).
    #[must_use]
    pub fn verified_on(&self) -> String {
        self.card.map_or_else(
            || COMPILED_VERIFIED_ON.to_string(),
            |c| c.verified_on().format("%Y-%m-%d").to_string(),
        )
    }

    /// `model`'s rates, or `None` when no row matches.
    #[must_use]
    pub fn rates(&self, model: &str) -> Option<ModelPricing> {
        ModelPricing::resolve_with(self.card, model)
    }

    /// The USD estimate for one model's totals, or `None` for an unknown model.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn usd(&self, row: &ModelUsageTotals) -> Option<f64> {
        let p = self.rates(&row.model)?;
        let per_1k = |tokens: i64, rate: f64| tokens as f64 / 1000.0 * rate;
        Some(
            per_1k(row.input, p.input_cost_per_1k)
                + per_1k(row.output, p.output_cost_per_1k)
                + per_1k(row.cache_read, p.cache_read_cost_per_1k)
                + per_1k(row.cache_write_5m, p.cache_write_cost_per_1k)
                + per_1k(row.cache_write_1h, p.cache_write_1h_cost_per_1k),
        )
    }

    /// The cost attributes for `row`: `loom.cost.usd_estimate`,
    /// `gen_ai.cost.usd_estimate`, `loom.pricing.{verified_on,source}` — or
    /// none at all for a model the card does not know.
    #[must_use]
    pub fn attributes(&self, row: &ModelUsageTotals) -> TraceAttributes {
        let Some(usd) = self.usd(row) else {
            let has_tokens = row.input != 0
                || row.output != 0
                || row.cache_read != 0
                || row.cache_write_5m != 0
                || row.cache_write_1h != 0;
            if has_tokens && first_unpriced_sighting(&row.model) {
                log::warn!(
                    "loom.runtime.usage exported without a USD estimate: model '{}' is not on \
                     the pricing card (add a row to defaults/pricing.json and the compiled card)",
                    row.model
                );
            }
            return TraceAttributes::new();
        };
        let usd = format!("{usd:.6}");
        [
            ("loom.cost.usd_estimate", usd.clone()),
            ("gen_ai.cost.usd_estimate", usd),
            ("loom.pricing.verified_on", self.verified_on()),
            ("loom.pricing.source", self.source().to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }
}

#[cfg(test)]
mod unpriced_tests {
    use super::first_unpriced_sighting;

    #[test]
    fn unpriced_warning_is_deduplicated_per_model() {
        assert!(first_unpriced_sighting("unit-test-unpriced-a"));
        assert!(!first_unpriced_sighting("unit-test-unpriced-a"));
        assert!(first_unpriced_sighting("unit-test-unpriced-b"));
    }
}
