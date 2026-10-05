//! `eta backtest --half-life-days`: replay `land-2026-10-04-fresh-tide` at a
//! caller-chosen base half-life without registering extra ids (#10325).
//! Split out of `eta_cmd.rs`, which is frozen by the file-size ratchet.

use anyhow::{bail, Result};
use loom_daemon::eta::heuristics::{LandFreshTide, LAND_FRESH_TIDE};
use loom_daemon::eta::{Heuristic, Registry};

/// The fresh-tide instance `--half-life-days` asks for, if any.
pub(super) fn fresh_tide_override(
    heuristic: &str,
    compare: Option<&str>,
    half_life_days: Option<f64>,
) -> Result<Option<LandFreshTide>> {
    let Some(days) = half_life_days else {
        return Ok(None);
    };
    if !days.is_finite() || days <= 0.0 {
        bail!("invalid --half-life-days {days}: must be a positive number of days");
    }
    if heuristic != LAND_FRESH_TIDE && compare != Some(LAND_FRESH_TIDE) {
        bail!("--half-life-days only applies to {LAND_FRESH_TIDE}");
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(Some(LandFreshTide::with_half_life((days * 86_400.0).round() as i64)))
}

/// Look `id` up, substituting the override for the fresh-tide id.
pub(super) fn pick<'a>(
    registry: &'a Registry,
    half_life: Option<&'a LandFreshTide>,
    id: &str,
) -> Option<&'a dyn Heuristic> {
    match half_life {
        Some(h) if id == LAND_FRESH_TIDE => Some(h as &dyn Heuristic),
        _ => registry.get(id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_life_days_builds_the_fresh_tide_variant_on_either_side() {
        assert!(fresh_tide_override(LAND_FRESH_TIDE, None, None)
            .unwrap()
            .is_none());
        let a = fresh_tide_override(LAND_FRESH_TIDE, Some("land-v2"), Some(7.0));
        assert_eq!(a.unwrap().unwrap().half_life_sec(), 7 * 86_400);
        let b = fresh_tide_override("land-v2", Some(LAND_FRESH_TIDE), Some(1.0));
        assert_eq!(b.unwrap().unwrap().half_life_sec(), 86_400);
    }

    #[test]
    fn half_life_days_rejects_bad_values_and_other_heuristics() {
        assert!(fresh_tide_override(LAND_FRESH_TIDE, None, Some(0.0)).is_err());
        assert!(fresh_tide_override(LAND_FRESH_TIDE, None, Some(f64::NAN)).is_err());
        assert!(fresh_tide_override("land-v2", None, Some(2.0)).is_err());
    }
}
