//! The IPCW conformal wrapper over **any** `land` base (#10524, slice 5).
//!
//! [`IpcwWrap`] applies one of the two IPCW calibrators
//! ([`super::conformal_ipcw`]) to a `land` base given as a parameter: it runs
//! the base, re-identifies the explanation as its own, and calibrates it
//! against the base's own track record (`calibration` rows whose `heuristic`
//! is the base's id).
//!
//! The registered IPCW shadows built this way (`land-2026-10-06-quick-tern`
//! and `-swift-tern` over `land-2026-10-04-twin-otter-b`, `-bold-lark` over
//! keen-wren, #10524) are retired (#10949): the 2026-10-08 walk-forward
//! scored them 7–13 h of pinball4 worse than `land-v1`, with 82–95% of
//! wrapped estimates on the unresolved-tail rule. `IpcwWrap::new(<retired
//! id>, <base>, <calibrator>)` reproduces any of them offline.
//!
//! # Why it is not registered
//!
//! A calibrated base needs evidence before it deserves a slot in the `land`
//! shadow budget (#10525), and the walk-forward evidence so far is against
//! the IPCW wrapper over every base (#10949). So this slice wraps bases
//! **offline**: `eta backtest --wrap ipcw|ipcw-drift` scores `<base>+ipcw` on the replay set, and
//! `--compare <base>` pairs it against the unwrapped base. That is the
//! comparison the #10524 acceptance asks for ("pinball no worse than the
//! unwrapped base, paired CI"), for the priority model (#10508), the hazard
//! simulator (#10523) or any other `land` base.
//!
//! Registering a wrapped base later is a new datestamped id built from this
//! type ([`IpcwWrap::new`] takes a `&'static str` id); ids stay immutable.
//!
//! # Rules
//!
//! - **`land` only.** The calibration evidence is landings; [`IpcwWrap::new`]
//!   refuses any other kind.
//! - **Never twice.** A base whose explanation already carries a
//!   `calibration` or `recalibration` record (even-lark) is left as the
//!   base answered, only re-identified: a second
//!   shift would overwrite the first record and the explanation would no
//!   longer recompute.
//! - **Transform order is the recompute's.** A regime-adjusted base
//!   (brisk-petrel, #10528) is calibrated on its raw quantiles and the
//!   regime factor re-applied last, as [`run_explanation`] replays it.
//! - **Degrades to the base.** A refusal, a base without p90, or too few
//!   effective landings is the base's answer unchanged (re-identified, with
//!   no `calibration` record).
//! - **Point-in-time.** Exactly [`super::conformal_ipcw`]'s: no estimate
//!   made at or after `as_of`, and no outcome known at or after it, is used
//!   as an event.
//!
//! Pure: no clock, no file, no forge.

use super::conformal_ipcw;
use super::heuristics::LAND_EVEN_LARK;
use super::history::StageSamples;
use super::regime;
use super::simulate::run_explanation;
use super::{estimate_id, EstimateInput, Explanation, Heuristic, Kind, Tier};

/// The registered `land` heuristics that already calibrate their own
/// estimate. Wrapping one is the identity (see "Never twice"), so `eta
/// backtest --wrap` refuses them.
pub const CALIBRATED: &[&str] = &[LAND_EVEN_LARK];

/// Which IPCW calibrator wraps the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Calibrator {
    /// [`conformal_ipcw::calibrate`] (retired quick-tern's).
    Ipcw,
    /// [`conformal_ipcw::calibrate_drift_aware`] (retired swift-tern's).
    IpcwDrift,
}

impl Calibrator {
    /// Every calibrator, in [`Self::name`] order.
    pub const ALL: [Calibrator; 2] = [Calibrator::Ipcw, Calibrator::IpcwDrift];

    /// The name `eta backtest --wrap` takes, and the suffix of an offline
    /// wrapped id (`<base>+<name>`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Calibrator::Ipcw => "ipcw",
            Calibrator::IpcwDrift => "ipcw-drift",
        }
    }

    /// The calibrator named `name`, if any.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name.trim())
    }

    /// The id an offline wrap of `base` reports under: `<base>+<name>`.
    /// Never a registered id (`+` appears in none).
    #[must_use]
    pub fn wrapped_id(self, base: &str) -> String {
        format!("{base}+{}", self.name())
    }
}

/// `base`'s estimate, calibrated by IPCW split-conformal against `base`'s
/// own track record, under the id `id`.
#[derive(Debug, Clone)]
pub struct IpcwWrap<H> {
    id: &'static str,
    base: H,
    calibrator: Calibrator,
}

impl<H: Heuristic> IpcwWrap<H> {
    /// `base` wrapped as `id`; `None` unless `base` predicts `land`.
    #[must_use]
    pub fn new(id: &'static str, base: H, calibrator: Calibrator) -> Option<Self> {
        (base.kind() == Kind::Land).then_some(IpcwWrap {
            id,
            base,
            calibrator,
        })
    }

    /// The wrapped base's id: the `heuristic` its calibration rows carry.
    #[must_use]
    pub fn base_id(&self) -> &'static str {
        self.base.id()
    }
}

impl<H: Heuristic> Heuristic for IpcwWrap<H> {
    fn id(&self) -> &'static str {
        self.id
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    /// A candidate (#10525).
    fn tier(&self) -> Tier {
        Tier::Candidate
    }

    /// As its base.
    fn models_hold(&self) -> bool {
        self.base.models_hold()
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut explanation = self.base.estimate(input, history);
        explanation.heuristic = self.id.to_string();
        explanation.estimate_id = estimate_id(&input.subject, Kind::Land, self.id, input.as_of);
        if explanation.result.is_none()
            || explanation.calibration.is_some()
            || explanation.recalibration.is_some()
        {
            return explanation;
        }
        let Some(regime) = explanation.regime_adjustment.clone() else {
            return self.calibrate(explanation, history);
        };
        // A regime-adjusted base (brisk-petrel, #10528). The recompute
        // ([`run_explanation`]) applies calibration to the *raw* base and the
        // regime factor last; both round, so they do not commute on the
        // served integers. Calibrate the raw base, then re-apply the factor,
        // so the answer replays from its own fields. Unrecomputable or
        // uncalibrated: the base as it answered.
        let mut raw = explanation.clone();
        raw.regime_adjustment = None;
        let Some(raw_q) = run_explanation(&raw) else {
            return explanation;
        };
        set_quantiles(&mut raw, raw_q);
        let mut calibrated = self.calibrate(raw, history);
        let Some(q) = calibrated
            .calibration
            .is_some()
            .then(|| calibrated.quantiles_with_p90())
            .flatten()
        else {
            return explanation;
        };
        set_quantiles(&mut calibrated, regime::scale(q, regime.factor));
        calibrated.regime_adjustment = Some(regime);
        calibrated.enforce_cap();
        calibrated
    }
}

impl<H: Heuristic> IpcwWrap<H> {
    fn calibrate(&self, explanation: Explanation, history: &StageSamples) -> Explanation {
        let base = self.base.id();
        match self.calibrator {
            Calibrator::Ipcw => conformal_ipcw::calibrate(explanation, &history.calibration, base),
            Calibrator::IpcwDrift => {
                conformal_ipcw::calibrate_drift_aware(explanation, &history.calibration, base)
            }
        }
    }
}

/// Serve `q` as `explanation`'s answer.
fn set_quantiles(explanation: &mut Explanation, q: (i64, i64, i64, i64)) {
    let as_of = explanation.as_of;
    if let Some(result) = explanation.result.as_mut() {
        (result.p25_sec, result.p50_sec, result.p75_sec) = (q.0, q.1, q.2);
        result.p90_sec = Some(q.3);
        result.eta_p50_at = as_of + chrono::Duration::seconds(q.1);
    }
}
