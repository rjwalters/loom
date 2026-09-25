//! Supersede-not-stack for an already-armed drain-and-restart roll (#8514).
//!
//! Before this module, [`super::run_tick`] short-circuited on
//! [`DrainTrigger::roll_in_progress`](super::DrainTrigger::roll_in_progress)
//! whenever *any* roll was armed. A newer release published while a roll was
//! sitting pending — dispatch paused, waiting for in-flight sweeps to reach
//! zero — was therefore ignored until that roll completed or spent its whole
//! paused-dispatch budget. The host spent up to two hours paused converging on
//! a binary that had already been superseded.
//!
//! The decision is pure and lives here (rather than in `auto_update.rs`, which
//! is over `.loom/docs/file-size-policy.md`'s threshold and frozen) so every
//! branch is a unit-test assertion rather than something only reachable by
//! driving a real daemon to a real deadline.
//!
//! **Conservative by construction.** The pause this replaces exists to protect
//! a #6007 fail-safe whose earlier, simpler form deadlocked the work finder, so
//! a supersede only happens when *all* of these hold:
//!
//! - the armed roll is **pending** (it has already survived a deadline refusal)
//!   — a first-attempt drain is still inside the deadline it was given and is
//!   left completely alone;
//! - it is a **relaunch** roll, never a `fleet drain` teardown (`then_exit`);
//! - it carries a **known target** — a roll this daemon's auto-updater armed
//!   and labelled, never an operator's `restart --drain`;
//! - a release artifact **resolves, is actionable, and has a different
//!   identity** than that target.
//!
//! Anything else keeps pre-#8514 behaviour exactly: skip the tick.

use super::{ArtifactInfo, ArtifactResolution};

/// The identity of the roll that is currently armed, as reported by the live
/// drain state (#8514).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArmedRoll {
    /// The artifact identity this roll was triggered for
    /// ([`artifact_roll_target`]), or `None` for a drain the auto-updater did
    /// not arm (an operator `restart --drain`, a source-path roll).
    pub target: Option<String>,
    /// `true` once the roll has survived at least one deadline refusal and is
    /// being retained with dispatch paused.
    pub pending: bool,
    /// `true` when the armed drain's terminal action is "stop and stay down"
    /// (a `fleet drain` teardown), which is never superseded.
    pub then_exit: bool,
}

/// What a tick should do about a roll that is already armed (#8514).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArmedRollAction {
    /// Keep the pre-#8514 behaviour: skip this tick with `reason` as the note.
    Skip(String),
    /// Discard the armed roll and let this tick roll to `to` instead.
    Supersede {
        /// The superseded roll's target identity.
        from: String,
        /// The newly resolved artifact's identity.
        to: String,
    },
}

/// The identity a roll is keyed on: the release tag plus the published asset
/// checksum when one is known.
///
/// The checksum is part of the key because a re-published release (same tag,
/// different bytes) is a genuinely different roll target — the same reason
/// [`super::ArtifactRollRecord`] uses `(version, asset_sha256)` as its
/// convergence key.
#[must_use]
pub fn artifact_roll_target(info: &ArtifactInfo) -> String {
    match info
        .asset_sha256
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(sha) => format!("{}@{sha}", info.tag),
        None => info.tag.clone(),
    }
}

/// Decide what to do about an armed roll, given what this tick resolved.
///
/// `armed` is `None` when the trigger cannot describe the armed roll (an older
/// trigger implementation, a test fake) — treated exactly like an unknown
/// target: skip, never supersede.
#[must_use]
pub fn decide_armed_roll(
    armed: Option<&ArmedRoll>,
    artifact: &ArtifactResolution,
) -> ArmedRollAction {
    const ARMED: &str = "a drain-and-restart roll is already armed (dispatch paused, waiting for \
                         in-flight sweeps to reach zero) — skipping this tick";

    let Some(armed) = armed else {
        return ArmedRollAction::Skip(ARMED.to_string());
    };
    if armed.then_exit {
        return ArmedRollAction::Skip(format!(
            "{ARMED} [the armed drain is a then-exit teardown: it is never superseded by a newer \
             release]"
        ));
    }
    if !armed.pending {
        return ArmedRollAction::Skip(format!(
            "{ARMED} [first attempt, still inside its own deadline]"
        ));
    }
    let Some(target) = armed.target.as_deref() else {
        return ArmedRollAction::Skip(format!(
            "{ARMED} [this daemon did not arm it, so it carries no artifact target to compare \
             against]"
        ));
    };
    // Not `is_actionable()`-worthy ⇒ there is nothing newer to roll to: an
    // unresolved artifact, one already installed, or an older release from a
    // stale/wrong repo (#8513). Any of those must leave the pending roll alone.
    let ArtifactResolution::Resolved(info) = artifact else {
        return ArmedRollAction::Skip(format!(
            "{ARMED} [pending roll targets {target}; no newer artifact resolved this tick]"
        ));
    };
    if !artifact.is_actionable() {
        return ArmedRollAction::Skip(format!(
            "{ARMED} [pending roll targets {target}; the resolved release is not newer]"
        ));
    }
    let resolved = artifact_roll_target(info);
    if resolved == target {
        return ArmedRollAction::Skip(format!(
            "{ARMED} [the pending roll already targets {target}]"
        ));
    }
    ArmedRollAction::Supersede {
        from: target.to_string(),
        to: resolved,
    }
}

/// The note published when a pending roll is superseded (#8514), so `status`
/// and the daemon log both explain why the pause ended early.
#[must_use]
pub fn supersede_note(from: &str, to: &str) -> String {
    format!(
        "a newer release artifact ({to}) was published while the roll to {from} was still pending \
         — superseding it: dispatch resumes, and the roll re-arms for {to} rather than waiting \
         out the paused-dispatch budget for a binary that is already stale"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn info(tag: &str, version: &str, installed: &str, sha: Option<&str>) -> ArtifactInfo {
        ArtifactInfo {
            repo: "rjwalters/loom".to_string(),
            tag: tag.to_string(),
            version: version.to_string(),
            published_at: None,
            asset_sha256: sha.map(str::to_string),
            target: None,
            installed_version: Some(installed.to_string()),
            installed_sha256: Some("aaaa".to_string()),
        }
    }

    fn newer() -> ArtifactResolution {
        ArtifactResolution::Resolved(info("v0.19.30", "0.19.30", "0.19.24", Some("bbbb")))
    }

    fn pending(target: &str) -> ArmedRoll {
        ArmedRoll {
            target: Some(target.to_string()),
            pending: true,
            then_exit: false,
        }
    }

    #[test]
    fn a_newer_artifact_supersedes_a_pending_roll() {
        let action = decide_armed_roll(Some(&pending("v0.19.24@aaaa")), &newer());
        assert_eq!(
            action,
            ArmedRollAction::Supersede {
                from: "v0.19.24@aaaa".to_string(),
                to: "v0.19.30@bbbb".to_string(),
            }
        );
    }

    #[test]
    fn the_same_artifact_never_supersedes_its_own_roll() {
        let action = decide_armed_roll(Some(&pending("v0.19.30@bbbb")), &newer());
        assert!(matches!(action, ArmedRollAction::Skip(_)), "{action:?}");
    }

    #[test]
    fn a_first_attempt_roll_is_left_inside_its_own_deadline() {
        let armed = ArmedRoll {
            target: Some("v0.19.24@aaaa".to_string()),
            pending: false,
            then_exit: false,
        };
        let action = decide_armed_roll(Some(&armed), &newer());
        assert!(
            matches!(&action, ArmedRollAction::Skip(r) if r.contains("first attempt")),
            "{action:?}"
        );
    }

    #[test]
    fn a_teardown_drain_is_never_superseded() {
        let armed = ArmedRoll {
            target: Some("v0.19.24@aaaa".to_string()),
            pending: true,
            then_exit: true,
        };
        let action = decide_armed_roll(Some(&armed), &newer());
        assert!(
            matches!(&action, ArmedRollAction::Skip(r) if r.contains("teardown")),
            "{action:?}"
        );
    }

    #[test]
    fn an_untargeted_roll_is_never_superseded() {
        let armed = ArmedRoll {
            target: None,
            pending: true,
            then_exit: false,
        };
        let action = decide_armed_roll(Some(&armed), &newer());
        assert!(
            matches!(&action, ArmedRollAction::Skip(r) if r.contains("no artifact target")),
            "{action:?}"
        );
        // And a trigger that cannot describe its roll at all behaves the same.
        assert!(matches!(decide_armed_roll(None, &newer()), ArmedRollAction::Skip(_)));
    }

    #[test]
    fn an_unresolved_or_older_artifact_leaves_the_pending_roll_alone() {
        let unresolved = ArtifactResolution::Unresolved("rate limited".to_string());
        assert!(matches!(
            decide_armed_roll(Some(&pending("v0.19.24@aaaa")), &unresolved),
            ArmedRollAction::Skip(_)
        ));
        // #8513's stale-repo shape: the resolved release is OLDER than installed.
        let older =
            ArtifactResolution::Resolved(info("v0.18.0", "0.18.0", "0.19.24", Some("cccc")));
        let action = decide_armed_roll(Some(&pending("v0.19.24@aaaa")), &older);
        assert!(
            matches!(&action, ArmedRollAction::Skip(r) if r.contains("not newer")),
            "{action:?}"
        );
    }

    #[test]
    fn the_roll_target_keys_on_tag_and_asset_checksum() {
        assert_eq!(
            artifact_roll_target(&info("v0.19.30", "0.19.30", "0.19.24", Some("bbbb"))),
            "v0.19.30@bbbb"
        );
        // A republished tag with different bytes is a different target.
        assert_ne!(
            artifact_roll_target(&info("v0.19.30", "0.19.30", "0.19.24", Some("bbbb"))),
            artifact_roll_target(&info("v0.19.30", "0.19.30", "0.19.24", Some("cccc")))
        );
        // No published checksum ⇒ the tag alone, never an empty key.
        assert_eq!(artifact_roll_target(&info("v0.19.30", "0.19.30", "0.19.24", None)), "v0.19.30");
        assert_eq!(
            artifact_roll_target(&info("v0.19.30", "0.19.30", "0.19.24", Some("  "))),
            "v0.19.30"
        );
    }
}
