//! Feeds each launched role tick's outcome into the per-workspace
//! launch-failure hold (`runtime_preference::launch_failure_hold`, #11453).
//!
//! Every failure site in [`super::run_role_with_timeout`] already notes its
//! `loom.failure_class`; [`HoldFeed::failure`] notes it exactly as before and
//! also counts it against the tick's `(workspace root, role, tap)`:
//! a deterministic class ([`super::failure_class::is_deterministic`]) extends
//! the streak, anything else resets it. [`HoldFeed::success`] resets it too.
//!
//! Only a tick whose tap was chosen by a preference walk is fed (its admission
//! carries a [`PreferenceStamp`](crate::runtime_preference::PreferenceStamp)):
//! the hold is only ever read by that walk, so a static binding or an operator
//! pin has nothing to fall through to and is left exactly as it was.
//! `LOOM_LAUNCH_FAILURE_HOLD_SECS=0` feeds nothing at all.

use super::failure_class;
use crate::observability::lifecycle::{note_role_failure, RoleFailure};
use crate::runtime_admission::ResolvedRuntime;
use crate::runtime_preference::launch_failure_hold::{self, ClearReason, Key};
use crate::runtime_preference::Tap;
use std::path::{Path, PathBuf};

/// The tick's hold key, when it has one.
pub(super) struct HoldFeed {
    tracked: Option<(PathBuf, Key, Tap)>,
}

impl HoldFeed {
    pub(super) fn new(root: &Path, role: &str, admission: Option<&ResolvedRuntime>) -> Self {
        let tracked = admission.and_then(|admitted| {
            let stamp = admitted.preference.as_ref()?;
            if launch_failure_hold::hold_secs() == 0 {
                return None;
            }
            let tap = Tap {
                runtime: admitted.runtime.clone(),
                model_profile: stamp.model_profile.clone(),
            };
            let role = crate::runtime_admission::canonical_role(role).unwrap_or(role);
            Some((root.to_path_buf(), Key::new(root, role, &tap), tap))
        });
        Self { tracked }
    }

    /// Note `failure` on the tick's span (unchanged) and count it.
    pub(super) fn failure(&self, failure: RoleFailure) {
        if let Some((root, key, tap)) = &self.tracked {
            let now = now();
            if failure_class::is_deterministic(failure.class()) {
                let config = crate::config_resolver::resolve_effective_config(root);
                let fingerprint = launch_failure_hold::fingerprint(&config, tap);
                launch_failure_hold::record_failure(key, failure.class(), fingerprint, now);
            } else {
                launch_failure_hold::record_reset(key, ClearReason::TransientFailure, now);
            }
        }
        note_role_failure(failure);
    }

    /// The tick succeeded: the streak (and any live hold) is cleared.
    pub(super) fn success(&self) {
        if let Some((_, key, _)) = &self.tracked {
            launch_failure_hold::record_reset(key, ClearReason::Success, now());
        }
    }
}

fn now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
