//! Floor-driven roll targets (Issue #10712, part of #10698), and since #10885
//! the only thing that moves a fleet host.
//!
//! The fleet floor (`loom_min_version`, read as three values by
//! [`crate::fleet_sync::floor_knowledge`]) is the fleet's lever for moving
//! hosts, and this module is where the self-update loop acts on it. The loop
//! compares the floor with the running version at startup and on every tick,
//! and acts on that tick. There is no roll window and no per-host jitter.
//!
//! # The rule
//!
//! | Floor knowledge | Running vs floor | This tick |
//! |---|---|---|
//! | no fleet store | n/a | Opt-in `autoUpdate`, unchanged: artifact path and source path behind the settle gate and its ceiling. `target_source = autoupdate`. |
//! | unknown | n/a | No version roll. The note says the floor is not known and why. Fail closed. |
//! | set | below; the newest release is at or above it | Floor roll now, no settle, pinned to that release's exact tag (#10709). `target_source = floor`. |
//! | set | below; the newest release is below it | The typed stall ([`FloorStallReport`]), alerted at ERROR. No roll; dispatch continues. |
//! | set | below; no comparable release resolved | No roll; the next tick asks again. A standing stall is kept (#10866). |
//! | set | at or above | **No roll**, whatever newer release, re-published artifact or source HEAD exists. |
//! | set, but a version does not parse | n/a | No version roll, one WARN. |
//!
//! A host with a fleet store therefore never chases the latest release and
//! never rebuilds itself from source: it moves when `loom_min_version` moves.
//! Backoff and terminal failures still apply to a floor roll. The roll itself
//! is the same pause-and-roll every trigger uses (#10831).
//!
//! "The newest release" is what [`super::AutoUpdateProbe::resolve_artifact`]
//! resolves. For a host below its floor that is the newest published release
//! that carries this platform's binary and its `.sha256`, found by listing
//! releases and walking back past any without them (#11029); it need not be
//! the forge's Latest. Otherwise it is the forge's latest release. A tag with
//! no assets is never a target.
//!
//! # One roll path, one comparator
//!
//! There is no floor-specific roll machinery: the floor only selects the
//! target ([`select_target`]) and tells the settle gate who chose it
//! ([`TargetSource`]). Every version comparison here goes through the floor
//! seam's own strict parser, [`parse_triple`], so the floor is compared by the
//! same rules that validated it.
//!
//! A workspace that needs a newer daemon than this one is a second demand of
//! the same kind (#10719, [`repo_ahead`]): it enters [`select_target`] beside
//! the floor target and is recorded as [`TargetSource::RepoAhead`].
//!
//! "Newest release at or above the floor" is the resolved release when that
//! meets the floor. The resolver already walked back to the newest release
//! with this platform's assets (#11029), so if that one is below the floor, no
//! release with assets satisfies it. The forge's Latest is not assumed to be
//! the newest release.
//!
//! # Other demands (#10719, #10720)
//!
//! A repo ahead of this daemon and a restart-only config change are separate
//! roll demands that act in every row of the table above. Neither is produced
//! here yet. [`select_target`] is the seam: a repo-ahead target joins the
//! floor target there as another candidate.

use super::ArtifactInfo;
use crate::fleet_store::floor::parse_triple;
use crate::fleet_sync::FloorKnowledge;

/// The stall this loop can declare (Issue #10712): the fleet floor
/// (`loom_min_version`) is above the running version and above every published
/// release that carries this platform's assets, so no roll can satisfy it.
///
/// There is nothing to abandon: no roll is armed for an unsatisfiable floor,
/// and **nothing is paused** for it. The host keeps dispatching on its current
/// version and does not roll (#10885: a fleet host does not fall back to the
/// newest release). The report exists so the
/// stall is typed, alerted at ERROR, and visible in `status`, instead of a
/// floor that silently does nothing. (Moved here when #10831 removed the
/// stall-suppression module with the wait-for-zero roll machinery.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorStallReport {
    /// The floor in force, `X.Y.Z`.
    pub floor: String,
    /// The running version, below the floor.
    pub running: String,
    /// The newest release's version that carries this platform's assets, also
    /// below the floor.
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
            "FLEET FLOOR UNSATISFIABLE: loom_min_version {floor} is above every release that publishes \
             this platform's binary (newest {newest}), so this host (running {running}) cannot roll to it. Most likely a \
             typo in the fleet store's loom_min_version. DISPATCH CONTINUES on {running}: the \
             floor never refuses work, and this host does not roll until the floor can be met. \
             Fix the floor, or publish a release at or above {floor}."
        )
    }
}

/// #10866: the rate limit on the unsatisfiable-floor ERROR line and the
/// record of it that `auto_update_state.json` carries across a restart.
pub mod alert;

/// #10719: the repo-ahead demand, a second floor from the workspaces.
pub mod repo_ahead;

/// Who chose a roll target. The settle gate consults this, so a decision
/// needs no clock to say whether settle applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSource {
    /// The running version is below the fleet floor: roll now, no settle.
    Floor,
    /// A registered workspace's installed Loom needs a newer daemon than this
    /// one (#10719): roll now, no settle. The one roll a fleet host makes
    /// besides the floor's.
    RepoAhead,
    /// An ordinary autoUpdate roll on a host with no fleet store: the settle
    /// gate and its ceiling apply.
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
/// `repo_ahead_target` (#10719) is a second demand with the same rules:
/// `target = max(floor, repo_ahead, autoUpdate)`. It counts only when above
/// `running`, skips settle, and is pinned to its tag. The source is
/// [`TargetSource::Floor`] whenever the floor counts, and
/// [`TargetSource::RepoAhead`] when only the repo-ahead demand does.
///
/// An unparseable version never wins a comparison: an unparseable
/// `floor_target`, `repo_ahead_target` or `running` leaves the decision to
/// the others, and an unparseable `autoupdate_target` never displaces a
/// demanded target.
#[must_use]
pub fn select_target<'a>(
    running: &str,
    floor_target: Option<&'a Release>,
    repo_ahead_target: Option<&'a Release>,
    autoupdate_target: Option<&'a Release>,
) -> Option<Target> {
    let above_running = |target: Option<&'a Release>| -> Option<&'a Release> {
        target.filter(|t| {
            matches!(
                (parse_triple(&t.version), parse_triple(running)),
                (Some(t), Some(r)) if t > r
            )
        })
    };
    let higher_of = |a: &Release, b: Option<&Release>| {
        b.filter(|b| parse_triple(&b.version) > parse_triple(&a.version))
            .map_or_else(|| a.clone(), Clone::clone)
    };
    let (floor, ahead) = (above_running(floor_target), above_running(repo_ahead_target));
    let (demanded, source) = match (floor, ahead) {
        (Some(floor), ahead) => (higher_of(floor, ahead), TargetSource::Floor),
        (None, Some(ahead)) => (ahead.clone(), TargetSource::RepoAhead),
        (None, None) => {
            return autoupdate_target.map(|auto| Target::new(auto, TargetSource::AutoUpdate));
        }
    };
    Some(Target::new(&higher_of(&demanded, autoupdate_target), source))
}

/// What the fleet floor says about this host on one tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FloorVerdict {
    /// No fleet store: not a fleet host. The floor has no effect and the tick
    /// is ordinary opt-in autoUpdate.
    #[default]
    NoStore,
    /// A fleet store is configured but the floor is not known (#10885). No
    /// version roll: a host that may have a floor must not chase latest.
    Unknown {
        /// Why it is not known.
        why: String,
    },
    /// A floor is set, but it or the running version is not `X.Y.Z`, so they
    /// cannot be compared. No version roll. Not reachable with a release
    /// build and a validated store.
    Uncomparable {
        /// The floor in force.
        floor: String,
    },
    /// The running version is at or above the floor: no roll, whatever newer
    /// release exists.
    Satisfied {
        /// The floor in force.
        floor: String,
    },
    /// Below the floor, and `target` (the newest release) satisfies it.
    Below {
        /// The floor in force.
        floor: String,
        /// The release to roll to.
        target: Release,
    },
    /// Below the floor, and no release satisfies it.
    Unsatisfiable(FloorStallReport),
    /// Below the floor, but this tick cannot say whether a release satisfies
    /// it: none resolved, or the latest one's version does not parse. Noted,
    /// not alerted: a resolution failure is already reported on its own, and
    /// the next tick asks again. [`FloorState::observe`] does not let this
    /// replace a standing [`Self::Unsatisfiable`] (#10866).
    Unresolved {
        /// The floor in force.
        floor: String,
        /// The latest release's version when one resolved but is not a plain
        /// `X.Y.Z` (e.g. `0.20.0-rc1`), so it cannot be compared with the
        /// floor. `None` when no release resolved at all.
        unparsed: Option<String>,
    },
}

/// Classify the floor against the running version and the newest release.
#[must_use]
pub fn floor_verdict(
    knowledge: &FloorKnowledge,
    running: &str,
    newest: Option<&Release>,
) -> FloorVerdict {
    let floor = match knowledge {
        FloorKnowledge::NoStore => return FloorVerdict::NoStore,
        FloorKnowledge::Unknown(why) => return FloorVerdict::Unknown { why: why.clone() },
        FloorKnowledge::Set(floor) => floor.as_str(),
    };
    let (Some(min), Some(run)) = (parse_triple(floor), parse_triple(running)) else {
        return FloorVerdict::Uncomparable {
            floor: floor.to_string(),
        };
    };
    if run >= min {
        return FloorVerdict::Satisfied {
            floor: floor.to_string(),
        };
    }
    let Some(newest) = newest else {
        return FloorVerdict::Unresolved {
            floor: floor.to_string(),
            unparsed: None,
        };
    };
    match parse_triple(&newest.version) {
        // #10866: a version the floor's own parser rejects says nothing about
        // whether a release satisfies the floor, so it is not a stall.
        None => FloorVerdict::Unresolved {
            floor: floor.to_string(),
            unparsed: Some(newest.version.clone()),
        },
        Some(v) if v >= min => FloorVerdict::Below {
            floor: floor.to_string(),
            target: newest.clone(),
        },
        Some(_) => FloorVerdict::Unsatisfiable(FloorStallReport {
            floor: floor.to_string(),
            running: running.to_string(),
            newest: newest.version.clone(),
        }),
    }
}

/// The loop's floor bookkeeping: the basis set at the start of each tick, the
/// last verdict, and the record of the last unsatisfiable-floor alert.
///
/// The verdict carries the typed stall between ticks, and is the last *known*
/// one: a tick that cannot classify the floor leaves a standing stall in place
/// (see [`Self::observe`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FloorState {
    knowledge: FloorKnowledge,
    running: String,
    verdict: FloorVerdict,
    /// #10866: the stall last alerted on. `Some` only while that stall
    /// stands, or between a restore and the first [`Self::set_basis`].
    alert: Option<alert::FloorAlert>,
    /// #11029: consecutive ticks a host below its floor could not resolve a
    /// release to roll to. Reset by any other reading and by a changed basis.
    unresolved_streak: u32,
}

/// #11029: consecutive below-floor ticks with no resolvable target before the
/// tick is logged at WARN instead of INFO. At the default 900s interval, one
/// hour.
pub const UNRESOLVED_WARN_AFTER: u32 = 4;

impl FloorState {
    /// Set this tick's basis: what is known about the floor and the running
    /// version (`env!("CARGO_PKG_VERSION")` in production). A changed basis
    /// drops the previous verdict and alert record, so fixing the floor
    /// clears a stall at once.
    ///
    /// One exception (#10866): the first call after a restart is a change from
    /// the default basis, and an alert record restored from disk whose floor
    /// and running version are the new basis is kept. Its stall is seeded as
    /// the last-known verdict, so the restart neither drops the stall nor logs
    /// it again. A restored record for any other basis is dropped like the
    /// rest.
    ///
    /// #10885: a basis under which a fleet host cannot be compared with its
    /// floor is logged at WARN here, once per change, not once per tick.
    pub fn set_basis(&mut self, knowledge: FloorKnowledge, running: &str) {
        if self.knowledge == knowledge && self.running == running {
            return;
        }
        self.knowledge = knowledge;
        self.running = running.to_string();
        self.unresolved_streak = 0;
        let floor = self.floor().map(str::to_string);
        self.alert = self
            .alert
            .take()
            .filter(|a| Some(&a.report.floor) == floor.as_ref() && a.report.running == running);
        self.verdict = match &self.alert {
            Some(a) => FloorVerdict::Unsatisfiable(a.report.clone()),
            // Classified from the basis alone until [`Self::observe`] sees
            // this tick's release, so a stall from the old basis is gone at
            // once.
            None => floor_verdict(&self.knowledge, running, None),
        };
        match &self.verdict {
            FloorVerdict::Unknown { why } => log::warn!(
                "auto_update: the fleet floor is not known ({why}) — no version roll until it is; \
                 a fleet host never falls back to chasing the latest release"
            ),
            FloorVerdict::Uncomparable { floor } => log::warn!(
                "auto_update: the fleet floor {floor:?} and the running version {running:?} are \
                 not both X.Y.Z, so they cannot be compared — no version roll"
            ),
            _ => {}
        }
    }

    /// The floor in force, when one is set.
    #[must_use]
    pub fn floor(&self) -> Option<&str> {
        match &self.knowledge {
            FloorKnowledge::Set(floor) => Some(floor),
            _ => None,
        }
    }

    /// Whether this host reads a fleet store (#10885). A fleet host rolls only
    /// for the floor: [`Self::observe`] returning a target is the one version
    /// roll it makes.
    #[must_use]
    pub fn fleet_host(&self) -> bool {
        self.knowledge != FloorKnowledge::NoStore
    }

    /// Why a fleet host is not rolling this tick (#10885), for the tick's
    /// skip reason. `unchased` names a newer release, a re-published artifact
    /// or a source HEAD that exists and is deliberately not acted on.
    #[must_use]
    pub fn hold_reason(&self, unchased: Option<&str>) -> String {
        let running = &self.running;
        match &self.verdict {
            FloorVerdict::Satisfied { floor } => format!(
                "fleet floor {floor} is met by running {running}{} — a fleet host rolls only \
                 when loom_min_version moves or a workspace needs a newer daemon",
                unchased.map_or_else(String::new, |what| format!("; not chasing {what}"))
            ),
            FloorVerdict::Unknown { why } => format!(
                "fleet floor not known ({why}) — no version roll until it is (a fleet host \
                 never falls back to chasing the latest release)"
            ),
            FloorVerdict::Uncomparable { floor } => format!(
                "fleet floor {floor:?} and running version {running:?} are not both X.Y.Z and \
                 cannot be compared — no version roll"
            ),
            // The note suffix says the rest for these.
            FloorVerdict::Unsatisfiable(_) => {
                "no release meets the fleet floor — not rolling".to_string()
            }
            FloorVerdict::Unresolved { .. } if self.unresolved_escalated() => format!(
                "STILL BELOW THE FLEET FLOOR: running {running} has had no release to roll to \
                 for {} consecutive ticks — not rolling. Check that a release at or above the \
                 floor publishes this platform's binary and .sha256",
                self.unresolved_streak
            ),
            FloorVerdict::Unresolved { .. } => {
                "below the fleet floor with no release to roll to this tick — not rolling"
                    .to_string()
            }
            FloorVerdict::Below { .. } | FloorVerdict::NoStore => {
                "no version roll this tick".to_string()
            }
        }
    }

    /// Classify against this tick's newest release (`None` when none
    /// resolved) and return the floor target when the host is below a
    /// satisfiable floor.
    ///
    /// #10866: an [`FloorVerdict::Unresolved`] reading does not replace a
    /// standing [`FloorVerdict::Unsatisfiable`], so a tick whose resolver
    /// failed keeps the stall and its alert. The basis is unchanged whenever
    /// that happens, because [`Self::set_basis`] resets the verdict on a
    /// change. Every other reading replaces the verdict.
    pub fn observe(&mut self, newest: Option<&Release>) -> Option<Release> {
        let seen = floor_verdict(&self.knowledge, &self.running, newest);
        let keep_stall = matches!(seen, FloorVerdict::Unresolved { .. })
            && matches!(self.verdict, FloorVerdict::Unsatisfiable(_));
        let unresolved = matches!(seen, FloorVerdict::Unresolved { .. });
        self.unresolved_streak = if unresolved && !keep_stall {
            self.unresolved_streak.saturating_add(1)
        } else {
            0
        };
        if !keep_stall {
            self.verdict = seen;
        }
        if self.stall().is_none() {
            // A stall that returns later is a new start.
            self.alert = None;
        }
        match &self.verdict {
            FloorVerdict::Below { target, .. } => Some(target.clone()),
            _ => None,
        }
    }

    /// The target for an actionable artifact `info`, given this tick's
    /// `floor_target` from [`Self::observe`] and the repo-ahead target from
    /// [`repo_ahead::RepoAheadState::observe`] (#10719).
    #[must_use]
    pub fn select(
        &self,
        floor_target: Option<&Release>,
        repo_ahead_target: Option<&Release>,
        info: &ArtifactInfo,
    ) -> Target {
        let auto = Release::of(info);
        select_target(&self.running, floor_target, repo_ahead_target, Some(&auto))
            .unwrap_or_else(|| Target::new(&auto, TargetSource::AutoUpdate))
    }

    /// Who is driving a roll decided on this tick's observation (#10831: the
    /// `target_source` the pause manifest records). Below a satisfiable floor
    /// every roll is floor-driven, whichever release it lands on
    /// ([`select_target`]); otherwise it is an ordinary autoUpdate roll, which
    /// only a host with no fleet store makes (#10885).
    #[must_use]
    pub fn roll_source(&self) -> TargetSource {
        match &self.verdict {
            FloorVerdict::Below { .. } => TargetSource::Floor,
            _ => TargetSource::AutoUpdate,
        }
    }

    /// Whether a below-floor host has gone [`UNRESOLVED_WARN_AFTER`]
    /// consecutive ticks without a release to roll to (#11029). The tick
    /// logs at WARN while this holds.
    #[must_use]
    pub fn unresolved_escalated(&self) -> bool {
        self.unresolved_streak >= UNRESOLVED_WARN_AFTER
    }

    /// Whether the floor is unknown or cannot be compared, so no version roll
    /// is possible and `status` must say why instead of "up to date" (#11029).
    #[must_use]
    pub fn floor_blind(&self) -> bool {
        matches!(self.verdict, FloorVerdict::Unknown { .. } | FloorVerdict::Uncomparable { .. })
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
    /// floor, `""` otherwise (a held tick's own reason names the floor; see
    /// [`Self::hold_reason`]). A floor-driven roll's note
    /// is the roll's outcome, so this is what names its cause there.
    #[must_use]
    pub fn note_suffix(&self) -> String {
        match &self.verdict {
            FloorVerdict::Below { floor, target } => format!(
                " [floor-driven: running {} is below the fleet floor {floor}; target {}]",
                self.running, target.tag
            ),
            FloorVerdict::Unsatisfiable(report) => format!(" [{}]", report.note()),
            FloorVerdict::Unresolved {
                floor,
                unparsed: None,
            } => format!(
                " [running {} is below the fleet floor {floor}, but no release resolved this \
                 tick to roll to]",
                self.running
            ),
            FloorVerdict::Unresolved {
                floor,
                unparsed: Some(version),
            } => format!(
                " [running {} is below the fleet floor {floor}, but the latest release's version \
                 {version:?} is not X.Y.Z, so it cannot be compared with the floor this tick]",
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
            select_target("0.19.800", None, None, Some(&auto)),
            Some(Target {
                tag: "v0.19.900".to_string(),
                version: "0.19.900".to_string(),
                source: TargetSource::AutoUpdate,
            })
        );
        assert_eq!(select_target("0.19.800", None, None, None), None);
    }

    #[test]
    fn a_floor_target_makes_the_roll_floor_driven_and_takes_the_max() {
        let floor = rel("0.19.900");
        // Tie: the floor target, floor-driven.
        let t = select_target("0.19.800", Some(&floor), None, Some(&rel("0.19.900"))).unwrap();
        assert_eq!((t.tag.as_str(), t.source), ("v0.19.900", TargetSource::Floor));
        // A higher autoUpdate target wins the tag, still floor-driven.
        let t = select_target("0.19.800", Some(&floor), None, Some(&rel("0.19.950"))).unwrap();
        assert_eq!((t.tag.as_str(), t.source), ("v0.19.950", TargetSource::Floor));
        // A lower one does not.
        let t = select_target("0.19.800", Some(&floor), None, Some(&rel("0.19.850"))).unwrap();
        assert_eq!((t.tag.as_str(), t.source), ("v0.19.900", TargetSource::Floor));
        // No autoUpdate target at all.
        let t = select_target("0.19.800", Some(&floor), None, None).unwrap();
        assert_eq!((t.tag.as_str(), t.source), ("v0.19.900", TargetSource::Floor));
        // Numeric, not lexical: 0.19.1000 > 0.19.999.
        let t = select_target("0.19.800", Some(&floor), None, Some(&rel("0.19.1000"))).unwrap();
        assert_eq!(t.tag, "v0.19.1000");
    }

    #[test]
    fn a_floor_target_not_above_running_is_ignored() {
        let auto = rel("0.19.950");
        for running in ["0.19.900", "0.19.901"] {
            let t = select_target(running, Some(&rel("0.19.900")), None, Some(&auto)).unwrap();
            assert_eq!(t.source, TargetSource::AutoUpdate, "{running}");
        }
    }

    #[test]
    fn unparseable_versions_never_win() {
        let auto = rel("0.19.950");
        let t = select_target("dev", Some(&rel("0.19.900")), None, Some(&auto)).unwrap();
        assert_eq!(t.source, TargetSource::AutoUpdate);
        let bad_floor = Release {
            tag: "vX".to_string(),
            version: "0.19.x".to_string(),
        };
        let t = select_target("0.19.800", Some(&bad_floor), None, Some(&auto)).unwrap();
        assert_eq!(t.source, TargetSource::AutoUpdate);
        let bad_auto = Release {
            tag: "vY".to_string(),
            version: "garbage".to_string(),
        };
        let t = select_target("0.19.800", Some(&rel("0.19.900")), None, Some(&bad_auto)).unwrap();
        assert_eq!((t.tag.as_str(), t.source), ("v0.19.900", TargetSource::Floor));
    }

    fn set(floor: &str) -> FloorKnowledge {
        FloorKnowledge::Set(floor.to_string())
    }

    #[test]
    fn floor_verdict_table() {
        let newest = rel("0.19.900");
        assert_eq!(
            floor_verdict(&FloorKnowledge::NoStore, "0.19.800", Some(&newest)),
            FloorVerdict::NoStore
        );
        // #10885: an unknown floor is its own verdict, never "no floor".
        assert_eq!(
            floor_verdict(&FloorKnowledge::Unknown("why".to_string()), "0.19.800", Some(&newest)),
            FloorVerdict::Unknown {
                why: "why".to_string()
            }
        );
        // Uncomparable inputs are never a stall, and never "no floor" either.
        for (floor, running) in [("0.19", "0.19.800"), ("0.19.850", "dev")] {
            assert_eq!(
                floor_verdict(&set(floor), running, Some(&newest)),
                FloorVerdict::Uncomparable {
                    floor: floor.to_string()
                }
            );
        }
        assert_eq!(
            floor_verdict(&set("0.19.800"), "0.19.800", Some(&newest)),
            FloorVerdict::Satisfied {
                floor: "0.19.800".to_string()
            }
        );
        assert_eq!(
            floor_verdict(&set("0.19.850"), "0.19.800", Some(&newest)),
            FloorVerdict::Below {
                floor: "0.19.850".to_string(),
                target: newest.clone(),
            }
        );
        // A release exactly at the floor satisfies it.
        assert!(matches!(
            floor_verdict(&set("0.19.900"), "0.19.800", Some(&newest)),
            FloorVerdict::Below { .. }
        ));
        assert_eq!(
            floor_verdict(&set("0.19.999"), "0.19.800", Some(&newest)),
            FloorVerdict::Unsatisfiable(FloorStallReport {
                floor: "0.19.999".to_string(),
                running: "0.19.800".to_string(),
                newest: "0.19.900".to_string(),
            })
        );
        assert_eq!(
            floor_verdict(&set("0.19.850"), "0.19.800", None),
            FloorVerdict::Unresolved {
                floor: "0.19.850".to_string(),
                unparsed: None,
            }
        );
        // #10866: a latest version the floor's parser rejects is unresolved,
        // never a stall, whichever side of the floor it "looks" like.
        for version in ["0.19.900-rc1", "v0.19.900", "garbage"] {
            let odd = Release {
                tag: "vX".to_string(),
                version: version.to_string(),
            };
            for floor in ["0.19.850", "0.19.999"] {
                assert_eq!(
                    floor_verdict(&set(floor), "0.19.800", Some(&odd)),
                    FloorVerdict::Unresolved {
                        floor: floor.to_string(),
                        unparsed: Some(version.to_string()),
                    },
                    "{version} against {floor}"
                );
            }
        }
    }

    /// #10866 item 4: an unresolved tick keeps a standing stall; every other
    /// reading replaces it.
    #[test]
    fn a_below_floor_host_escalates_after_consecutive_unresolved_ticks() {
        let mut state = FloorState::default();
        state.set_basis(FloorKnowledge::Set("0.19.850".to_string()), "0.19.800");
        for tick in 1..=UNRESOLVED_WARN_AFTER {
            assert!(!state.unresolved_escalated(), "tick {tick}");
            state.observe(None);
        }
        assert!(state.unresolved_escalated());
        assert!(state
            .hold_reason(None)
            .contains("STILL BELOW THE FLEET FLOOR"));
        // Any other reading resets the streak.
        state.observe(Some(&rel("0.19.900")));
        assert!(!state.unresolved_escalated());
    }

    #[test]
    fn an_unknown_floor_is_floor_blind_and_says_why() {
        let mut state = FloorState::default();
        state.set_basis(FloorKnowledge::Unknown("store offline".to_string()), "0.19.900");
        state.observe(Some(&rel("0.19.900")));
        assert!(state.floor_blind());
        assert!(state.hold_reason(None).contains("store offline"));
        let mut met = FloorState::default();
        met.set_basis(FloorKnowledge::Set("0.19.800".to_string()), "0.19.900");
        met.observe(Some(&rel("0.19.900")));
        assert!(!met.floor_blind());
    }

    #[test]
    fn an_unresolved_tick_keeps_a_standing_stall() {
        let mut state = FloorState::default();
        state.set_basis(FloorKnowledge::Set("9.0.0".to_string()), "0.19.800");
        // No prior stall: unresolved is just unresolved.
        assert_eq!(state.observe(None), None);
        assert!(matches!(state.verdict(), FloorVerdict::Unresolved { .. }));
        assert!(state.stall().is_none());
        assert!(state.note_suffix().contains("no release resolved"));

        assert_eq!(state.observe(Some(&rel("0.19.900"))), None);
        let stall = state.stall().cloned().unwrap();
        // Resolution fails, then the latest version is unparseable.
        let odd = Release {
            tag: "v1.0.0-rc1".to_string(),
            version: "1.0.0-rc1".to_string(),
        };
        for newest in [None, Some(&odd)] {
            assert_eq!(state.observe(newest), None);
            assert_eq!(state.stall(), Some(&stall), "{newest:?}");
            assert!(state.note_suffix().contains("FLEET FLOOR UNSATISFIABLE"));
        }
        // A fresh unsatisfiable reading updates `newest`.
        state.observe(Some(&rel("0.19.901")));
        assert_eq!(state.stall().unwrap().newest, "0.19.901");
        // A satisfying release clears it.
        assert_eq!(state.observe(Some(&rel("9.0.0"))), Some(rel("9.0.0")));
        assert!(state.stall().is_none());
        // So does removing the floor, with the stall standing again first.
        state.observe(Some(&rel("0.19.900")));
        state.observe(None);
        assert!(state.stall().is_some());
        state.set_basis(FloorKnowledge::NoStore, "0.19.800");
        assert!(state.stall().is_none());
        assert_eq!(state.observe(None), None);
        assert_eq!(state.verdict(), &FloorVerdict::NoStore);
    }

    #[test]
    fn a_changed_basis_drops_the_stall() {
        let mut state = FloorState::default();
        state.set_basis(FloorKnowledge::Set("9.0.0".to_string()), "0.19.800");
        assert_eq!(state.observe(Some(&rel("0.19.900"))), None);
        assert!(state.stall().is_some());
        // Same basis next tick: the stall stands until the next observation.
        state.set_basis(FloorKnowledge::Set("9.0.0".to_string()), "0.19.800");
        assert!(state.stall().is_some());
        // The operator fixes the typo: cleared before anything is observed.
        state.set_basis(FloorKnowledge::NoStore, "0.19.800");
        assert!(state.stall().is_none());
        assert_eq!(state.note_suffix(), "");
    }
}
