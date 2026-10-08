//! Floor-driven roll targets (Issue #10712, part of #10698).
//!
//! `autonomous.autoUpdate` follows the newest release only after the settle
//! gate, and every new target restarts that quiet period. A fleet that needs
//! hosts on a version therefore had no way to force it. The fleet floor
//! (`loom_min_version`, read by [`crate::fleet_sync::loom_min_version`]) is
//! that lever, and this module is where the self-update loop acts on it.
//!
//! # The rule
//!
//! - **Floor unset, or the running version already meets it:** nothing here
//!   changes a decision. The tick is byte-for-byte what it was.
//! - **Running below the floor, and a release satisfies it:** the roll is
//!   *floor-driven*. Its target is the newest release at or above the floor,
//!   pinned to that exact tag (#10709), and it skips the settle gate. A
//!   supersede of a floor-driven roll does not re-wait settle either, because
//!   the tick that re-arms it is still floor-driven. Backoff, terminal
//!   failures and the roll window are kept. The roll itself is the same
//!   pause-and-roll every trigger uses (#10831), recorded with
//!   `target_source = floor`.
//! - **Running below the floor, and no release satisfies it** (most likely a
//!   typo in the store): a typed stall ([`FloorStallReport`]) is recorded and
//!   alerted at ERROR, and the host **keeps dispatching** on its current
//!   version. The floor never refuses work. Ordinary autoUpdate decisions
//!   still apply, unchanged.
//!
//! # One roll path, one comparator
//!
//! There is no floor-specific roll machinery: the floor only selects the
//! target ([`select_target`]) and tells the settle gate who chose it
//! ([`TargetSource`]). Every version comparison here goes through the floor
//! seam's own strict parser, [`parse_triple`], so the floor is compared by the
//! same rules that validated it.
//!
//! "Newest release at or above the floor" is the resolved latest release when
//! that meets the floor: releases are monotonic, so if the latest one is below
//! the floor, no release satisfies it.

use super::ArtifactInfo;
use crate::fleet_store::floor::parse_triple;

/// The stall this loop can declare (Issue #10712): the fleet floor
/// (`loom_min_version`) is above the running version and above every published
/// release, so no roll can satisfy it.
///
/// There is nothing to abandon: no roll is armed for an unsatisfiable floor,
/// and **nothing is paused** for it. The host keeps dispatching on its current
/// version and ordinary autoUpdate rolls still apply. The report exists so the
/// stall is typed, alerted at ERROR, and visible in `status`, instead of a
/// floor that silently does nothing. (Moved here when #10831 removed the
/// stall-suppression module with the wait-for-zero roll machinery.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorStallReport {
    /// The floor in force, `X.Y.Z`.
    pub floor: String,
    /// The running version, below the floor.
    pub running: String,
    /// The newest published release's version, also below the floor.
    pub newest: String,
}

impl FloorStallReport {
    /// The ERROR line and `status` note.
    #[must_use]
    pub fn note(&self) -> String {
        let Self {
            floor,
            running,
            newest,
        } = self;
        format!(
            "FLEET FLOOR UNSATISFIABLE: loom_min_version {floor} is above every published release \
             (newest {newest}), so this host (running {running}) cannot roll to it. Most likely a \
             typo in the fleet store's loom_min_version. DISPATCH CONTINUES on {running}: the \
             floor never refuses work, and ordinary autoUpdate rolls still apply. Fix the floor, \
             or publish a release at or above {floor}."
        )
    }
}

/// Who chose a roll target. The settle gate consults this, so a decision
/// needs no clock to say whether settle applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSource {
    /// The running version is below the fleet floor: roll now, no settle.
    Floor,
    /// An ordinary autoUpdate roll: settle, roll window and ceiling apply.
    AutoUpdate,
}

/// A release a roll could target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The release tag (e.g. `v0.19.900`), what the fetch is pinned to.
    pub tag: String,
    /// Its version (e.g. `0.19.900`).
    pub version: String,
}

impl Release {
    /// The release a resolved artifact describes.
    #[must_use]
    pub fn of(info: &ArtifactInfo) -> Self {
        Self {
            tag: info.tag.clone(),
            version: info.version.clone(),
        }
    }
}

/// The roll target one tick selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The exact tag to install.
    pub tag: String,
    /// Its version.
    pub version: String,
    /// Who chose it.
    pub source: TargetSource,
}

impl Target {
    fn new(release: &Release, source: TargetSource) -> Self {
        Self {
            tag: release.tag.clone(),
            version: release.version.clone(),
            source,
        }
    }
}

/// Select this tick's roll target.
///
/// `floor_target` counts only when it is above `running` (a floor target at
/// or below the running version is no reason to roll). When it counts, the
/// roll is floor-driven: the target is the higher of the two candidates (ties
/// go to the floor target), and its source is [`TargetSource::Floor`] either
/// way, because holding a higher target behind settle would keep the host
/// below the floor. Without one, the autoUpdate target is returned unchanged.
///
/// An unparseable version never wins a comparison: an unparseable
/// `floor_target` or `running` leaves the decision to autoUpdate, and an
/// unparseable `autoupdate_target` never displaces a floor target.
#[must_use]
pub fn select_target(
    running: &str,
    floor_target: Option<&Release>,
    autoupdate_target: Option<&Release>,
) -> Option<Target> {
    let floor = floor_target.filter(|f| {
        matches!(
            (parse_triple(&f.version), parse_triple(running)),
            (Some(f), Some(r)) if f > r
        )
    });
    match (floor, autoupdate_target) {
        (None, None) => None,
        (None, Some(auto)) => Some(Target::new(auto, TargetSource::AutoUpdate)),
        (Some(floor), auto) => {
            let higher = auto
                .filter(|a| parse_triple(&a.version) > parse_triple(&floor.version))
                .unwrap_or(floor);
            Some(Target::new(higher, TargetSource::Floor))
        }
    }
}

/// What the fleet floor says about this host on one tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FloorVerdict {
    /// No floor in force, or it cannot be compared (an unparseable floor or
    /// running version): no effect, which is exactly the pre-floor behaviour.
    #[default]
    Unset,
    /// The running version is at or above the floor: no effect.
    Satisfied,
    /// Below the floor, and `target` (the newest release) satisfies it.
    Below {
        /// The floor in force.
        floor: String,
        /// The release to roll to.
        target: Release,
    },
    /// Below the floor, and no release satisfies it.
    Unsatisfiable(FloorStallReport),
    /// Below the floor, but no release resolved this tick, so whether one
    /// satisfies it is unknown. Noted, not alerted: a resolution failure is
    /// already reported on its own, and the next tick asks again.
    Unresolved {
        /// The floor in force.
        floor: String,
    },
}

/// Classify the floor against the running version and the newest release.
#[must_use]
pub fn floor_verdict(floor: Option<&str>, running: &str, newest: Option<&Release>) -> FloorVerdict {
    let Some(floor) = floor else {
        return FloorVerdict::Unset;
    };
    let (Some(min), Some(run)) = (parse_triple(floor), parse_triple(running)) else {
        return FloorVerdict::Unset;
    };
    if run >= min {
        return FloorVerdict::Satisfied;
    }
    let Some(newest) = newest else {
        return FloorVerdict::Unresolved {
            floor: floor.to_string(),
        };
    };
    if parse_triple(&newest.version).is_some_and(|v| v >= min) {
        FloorVerdict::Below {
            floor: floor.to_string(),
            target: newest.clone(),
        }
    } else {
        FloorVerdict::Unsatisfiable(FloorStallReport {
            floor: floor.to_string(),
            running: running.to_string(),
            newest: newest.version.clone(),
        })
    }
}

/// The loop's floor bookkeeping: the basis set at the start of each tick and
/// the last verdict, which carries the typed stall between ticks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FloorState {
    floor: Option<String>,
    running: String,
    verdict: FloorVerdict,
}

impl FloorState {
    /// Set this tick's basis: the floor in force and the running version
    /// (`env!("CARGO_PKG_VERSION")` in production). A changed basis drops the
    /// previous verdict, so removing the floor clears a stall at once.
    pub fn set_basis(&mut self, floor: Option<String>, running: &str) {
        if self.floor != floor || self.running != running {
            self.floor = floor;
            self.running = running.to_string();
            self.verdict = FloorVerdict::Unset;
        }
    }

    /// Classify against this tick's newest release (`None` when none
    /// resolved) and return the floor target when the host is below a
    /// satisfiable floor.
    pub fn observe(&mut self, newest: Option<&Release>) -> Option<Release> {
        self.verdict = floor_verdict(self.floor.as_deref(), &self.running, newest);
        match &self.verdict {
            FloorVerdict::Below { target, .. } => Some(target.clone()),
            _ => None,
        }
    }

    /// The target for an actionable artifact `info`, given this tick's
    /// `floor_target` from [`Self::observe`].
    #[must_use]
    pub fn select(&self, floor_target: Option<&Release>, info: &ArtifactInfo) -> Target {
        let auto = Release::of(info);
        select_target(&self.running, floor_target, Some(&auto))
            .unwrap_or_else(|| Target::new(&auto, TargetSource::AutoUpdate))
    }

    /// Who is driving a roll decided on this tick's observation (#10831: the
    /// `target_source` the pause manifest records). Below a satisfiable floor
    /// every roll is floor-driven, whichever release it lands on
    /// ([`select_target`]); otherwise it is an ordinary autoUpdate roll.
    #[must_use]
    pub fn roll_source(&self) -> TargetSource {
        match &self.verdict {
            FloorVerdict::Below { .. } => TargetSource::Floor,
            _ => TargetSource::AutoUpdate,
        }
    }

    /// The last verdict.
    #[must_use]
    pub fn verdict(&self) -> &FloorVerdict {
        &self.verdict
    }

    /// The standing unsatisfiable-floor stall, if any.
    #[must_use]
    pub fn stall(&self) -> Option<&FloorStallReport> {
        match &self.verdict {
            FloorVerdict::Unsatisfiable(report) => Some(report),
            _ => None,
        }
    }

    /// The suffix a floor-driven fetch's reason carries, `""` otherwise.
    #[must_use]
    pub fn why_suffix(&self, target: &Target) -> String {
        match (&self.verdict, target.source) {
            (FloorVerdict::Below { floor, .. }, TargetSource::Floor) => format!(
                " [floor-driven: running {} is below the fleet floor {floor}; rolling to the exact \
                 tag {} without waiting for settle]",
                self.running, target.tag
            ),
            _ => String::new(),
        }
    }

    /// The suffix a tick's `status` note carries while the host is below the
    /// floor, `""` when the floor is unset or met. A floor-driven roll's note
    /// is the roll's outcome, so this is what names its cause there.
    #[must_use]
    pub fn note_suffix(&self) -> String {
        match &self.verdict {
            FloorVerdict::Below { floor, target } => format!(
                " [floor-driven: running {} is below the fleet floor {floor}; target {}]",
                self.running, target.tag
            ),
            FloorVerdict::Unsatisfiable(report) => format!(" [{}]", report.note()),
            FloorVerdict::Unresolved { floor } => format!(
                " [running {} is below the fleet floor {floor}, but no release resolved this \
                 tick to roll to]",
                self.running
            ),
            _ => String::new(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn rel(version: &str) -> Release {
        Release {
            tag: format!("v{version}"),
            version: version.to_string(),
        }
    }

    #[test]
    fn select_target_without_a_floor_target_is_the_autoupdate_target() {
        let auto = rel("0.19.900");
        assert_eq!(
            select_target("0.19.800", None, Some(&auto)),
            Some(Target {
                tag: "v0.19.900".to_string(),
                version: "0.19.900".to_string(),
                source: TargetSource::AutoUpdate,
            })
        );
        assert_eq!(select_target("0.19.800", None, None), None);
    }

    #[test]
    fn a_floor_target_makes_the_roll_floor_driven_and_takes_the_max() {
        let floor = rel("0.19.900");
        // Tie: the floor target, floor-driven.
        let t = select_target("0.19.800", Some(&floor), Some(&rel("0.19.900"))).unwrap();
        assert_eq!((t.tag.as_str(), t.source), ("v0.19.900", TargetSource::Floor));
        // A higher autoUpdate target wins the tag, still floor-driven.
        let t = select_target("0.19.800", Some(&floor), Some(&rel("0.19.950"))).unwrap();
        assert_eq!((t.tag.as_str(), t.source), ("v0.19.950", TargetSource::Floor));
        // A lower one does not.
        let t = select_target("0.19.800", Some(&floor), Some(&rel("0.19.850"))).unwrap();
        assert_eq!((t.tag.as_str(), t.source), ("v0.19.900", TargetSource::Floor));
        // No autoUpdate target at all.
        let t = select_target("0.19.800", Some(&floor), None).unwrap();
        assert_eq!((t.tag.as_str(), t.source), ("v0.19.900", TargetSource::Floor));
        // Numeric, not lexical: 0.19.1000 > 0.19.999.
        let t = select_target("0.19.800", Some(&floor), Some(&rel("0.19.1000"))).unwrap();
        assert_eq!(t.tag, "v0.19.1000");
    }

    #[test]
    fn a_floor_target_not_above_running_is_ignored() {
        let auto = rel("0.19.950");
        for running in ["0.19.900", "0.19.901"] {
            let t = select_target(running, Some(&rel("0.19.900")), Some(&auto)).unwrap();
            assert_eq!(t.source, TargetSource::AutoUpdate, "{running}");
        }
    }

    #[test]
    fn unparseable_versions_never_win() {
        let auto = rel("0.19.950");
        let t = select_target("dev", Some(&rel("0.19.900")), Some(&auto)).unwrap();
        assert_eq!(t.source, TargetSource::AutoUpdate);
        let bad_floor = Release {
            tag: "vX".to_string(),
            version: "0.19.x".to_string(),
        };
        let t = select_target("0.19.800", Some(&bad_floor), Some(&auto)).unwrap();
        assert_eq!(t.source, TargetSource::AutoUpdate);
        let bad_auto = Release {
            tag: "vY".to_string(),
            version: "garbage".to_string(),
        };
        let t = select_target("0.19.800", Some(&rel("0.19.900")), Some(&bad_auto)).unwrap();
        assert_eq!((t.tag.as_str(), t.source), ("v0.19.900", TargetSource::Floor));
    }

    #[test]
    fn floor_verdict_table() {
        let newest = rel("0.19.900");
        assert_eq!(floor_verdict(None, "0.19.800", Some(&newest)), FloorVerdict::Unset);
        // Uncomparable inputs are no floor, never a stall.
        assert_eq!(floor_verdict(Some("0.19"), "0.19.800", Some(&newest)), FloorVerdict::Unset);
        assert_eq!(floor_verdict(Some("0.19.850"), "dev", Some(&newest)), FloorVerdict::Unset);
        assert_eq!(
            floor_verdict(Some("0.19.800"), "0.19.800", Some(&newest)),
            FloorVerdict::Satisfied
        );
        assert_eq!(
            floor_verdict(Some("0.19.850"), "0.19.800", Some(&newest)),
            FloorVerdict::Below {
                floor: "0.19.850".to_string(),
                target: newest.clone(),
            }
        );
        // A release exactly at the floor satisfies it.
        assert!(matches!(
            floor_verdict(Some("0.19.900"), "0.19.800", Some(&newest)),
            FloorVerdict::Below { .. }
        ));
        assert_eq!(
            floor_verdict(Some("0.19.999"), "0.19.800", Some(&newest)),
            FloorVerdict::Unsatisfiable(FloorStallReport {
                floor: "0.19.999".to_string(),
                running: "0.19.800".to_string(),
                newest: "0.19.900".to_string(),
            })
        );
        assert_eq!(
            floor_verdict(Some("0.19.850"), "0.19.800", None),
            FloorVerdict::Unresolved {
                floor: "0.19.850".to_string()
            }
        );
    }

    #[test]
    fn a_changed_basis_drops_the_stall() {
        let mut state = FloorState::default();
        state.set_basis(Some("9.0.0".to_string()), "0.19.800");
        assert_eq!(state.observe(Some(&rel("0.19.900"))), None);
        assert!(state.stall().is_some());
        // Same basis next tick: the stall stands until the next observation.
        state.set_basis(Some("9.0.0".to_string()), "0.19.800");
        assert!(state.stall().is_some());
        // The operator fixes the typo: cleared before anything is observed.
        state.set_basis(None, "0.19.800");
        assert!(state.stall().is_none());
        assert_eq!(state.note_suffix(), "");
    }
}
