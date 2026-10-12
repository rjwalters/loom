//! H4 classification of one in-flight agent: resume or requeue (issue #10830;
//! design `docs/design/daemon-roll-pause-resume.md` §2, §4, §9).
//!
//! Pure: `{kind, age, resume handle}` in, a disposition and a reason out. PR 2
//! (#10831) calls it for every agent in the in-flight snapshot; nothing calls
//! it from the roll path yet.
//!
//! The rules, in order:
//!
//! 1. **Young agents are reset** (`young-agent-reset`): an agent whose session
//!    first started less than `minResumableAgeSecs` ago loses little by being
//!    requeued. The age is measured from the session's FIRST start
//!    (`agent_started_at`, carried across earlier roll resumes), so a resumed
//!    agent keeps its full age. `0` disables the rule.
//! 2. **No resume handle, no session id, or a runtime without resume support**
//!    is `session-not-resumable`.
//! 3. Everything else is `resume`.
//!
//! Sweeps and role runs follow the same rules (operator decision Q7), as do
//! Claude and Codex (Q2).

use std::path::Path;

use super::pause_manifest::{Disposition, ItemKind, Runtime};

/// `autonomous.autoUpdate.pauseRoll.minResumableAgeSecs` default: 5 minutes.
pub const DEFAULT_MIN_RESUMABLE_AGE_SECS: u64 = 300;
/// Env override for [`DEFAULT_MIN_RESUMABLE_AGE_SECS`] (env > config > default).
pub const MIN_RESUMABLE_AGE_ENV: &str = "LOOM_AUTO_UPDATE_PAUSE_ROLL_MIN_RESUMABLE_AGE_SECS";

/// Requeue reason: the agent was younger than `minResumableAgeSecs`.
pub const REASON_YOUNG: &str = "young-agent-reset";
/// Requeue reason: no session to resume.
pub const REASON_NOT_RESUMABLE: &str = "session-not-resumable";

/// What the classifier needs to know about one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifyInput {
    pub kind: ItemKind,
    /// Seconds since the session's first start; `None` when unknown, which is
    /// treated as old (a missing start time must not reset a long-running
    /// agent).
    pub agent_age_secs: Option<u64>,
    /// `None` when the agent has no resume handle at all.
    pub runtime: Option<Runtime>,
    pub session_id: Option<String>,
}

/// The classifier's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub disposition: Disposition,
    /// Set exactly when the disposition is `requeue`.
    pub reason: Option<String>,
}

impl Classification {
    fn requeue(reason: &str) -> Self {
        Classification {
            disposition: Disposition::Requeue,
            reason: Some(reason.to_string()),
        }
    }
}

/// Classify one agent. `min_resumable_age_secs == 0` disables the age rule.
#[must_use]
pub fn classify(input: &ClassifyInput, min_resumable_age_secs: u64) -> Classification {
    if let ItemKind::Unknown(v) = &input.kind {
        return Classification::requeue(&format!("unknown-kind-{v}"));
    }
    if min_resumable_age_secs > 0
        && input
            .agent_age_secs
            .is_some_and(|age| age < min_resumable_age_secs)
    {
        return Classification::requeue(REASON_YOUNG);
    }
    let resumable_runtime = match &input.runtime {
        Some(Runtime::Claude | Runtime::Codex) => true,
        Some(Runtime::Unknown(_)) | None => false,
    };
    let has_session = input
        .session_id
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty());
    if !resumable_runtime || !has_session {
        return Classification::requeue(REASON_NOT_RESUMABLE);
    }
    Classification {
        disposition: Disposition::Resume,
        reason: None,
    }
}

/// Resolve `minResumableAgeSecs`: env, then
/// `.loom/config.json → autonomous.autoUpdate.pauseRoll.minResumableAgeSecs`,
/// then [`DEFAULT_MIN_RESUMABLE_AGE_SECS`]. `0` is a real value (rule off), so
/// unlike most `autoUpdate` knobs it is not filtered out; a non-integer is
/// ignored at its tier.
#[must_use]
pub fn resolve_min_resumable_age_secs(repo_root: &Path) -> u64 {
    if let Some(v) = std::env::var(MIN_RESUMABLE_AGE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return v;
    }
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    crate::config_resolver::get_path(
        &effective,
        "autonomous.autoUpdate.pauseRoll.minResumableAgeSecs",
    )
    .and_then(serde_json::Value::as_u64)
    .unwrap_or(DEFAULT_MIN_RESUMABLE_AGE_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "4910f978-64b9-4654-942a-dae6514e859c";

    fn input(
        kind: ItemKind,
        age: Option<u64>,
        runtime: Option<Runtime>,
        sid: Option<&str>,
    ) -> ClassifyInput {
        ClassifyInput {
            kind,
            agent_age_secs: age,
            runtime,
            session_id: sid.map(str::to_string),
        }
    }

    fn resume() -> Classification {
        Classification {
            disposition: Disposition::Resume,
            reason: None,
        }
    }

    #[test]
    fn the_five_minute_boundary_for_every_kind_and_runtime() {
        for kind in [ItemKind::Sweep, ItemKind::RoleRun] {
            for rt in [Runtime::Claude, Runtime::Codex] {
                let at = |age| {
                    classify(&input(kind.clone(), Some(age), Some(rt.clone()), Some(SID)), 300)
                };
                assert_eq!(at(299), Classification::requeue(REASON_YOUNG), "{kind:?} {rt:?} 299");
                assert_eq!(at(300), resume(), "{kind:?} {rt:?} 300");
                assert_eq!(at(301), resume(), "{kind:?} {rt:?} 301");
                assert_eq!(at(0), Classification::requeue(REASON_YOUNG), "{kind:?} {rt:?} 0");
            }
        }
    }

    #[test]
    fn zero_disables_the_age_rule() {
        let c = classify(&input(ItemKind::Sweep, Some(1), Some(Runtime::Claude), Some(SID)), 0);
        assert_eq!(c, resume());
    }

    #[test]
    fn an_unknown_start_time_is_treated_as_old() {
        let c = classify(&input(ItemKind::RoleRun, None, Some(Runtime::Codex), Some(SID)), 300);
        assert_eq!(c, resume());
    }

    #[test]
    fn a_missing_session_is_not_resumable_for_both_runtimes_and_kinds() {
        for kind in [ItemKind::Sweep, ItemKind::RoleRun] {
            for rt in [Some(Runtime::Claude), Some(Runtime::Codex)] {
                for sid in [None, Some(""), Some("  ")] {
                    let c = classify(&input(kind.clone(), Some(600), rt.clone(), sid), 300);
                    assert_eq!(
                        c,
                        Classification::requeue(REASON_NOT_RESUMABLE),
                        "{kind:?} {rt:?} {sid:?}"
                    );
                }
            }
        }
        let no_handle = classify(&input(ItemKind::Sweep, Some(600), None, None), 300);
        assert_eq!(no_handle, Classification::requeue(REASON_NOT_RESUMABLE));
        let other = classify(
            &input(ItemKind::Sweep, Some(600), Some(Runtime::Unknown("pi".into())), Some(SID)),
            300,
        );
        assert_eq!(other, Classification::requeue(REASON_NOT_RESUMABLE));
    }

    #[test]
    fn youth_is_checked_before_resumability() {
        let c = classify(&input(ItemKind::Sweep, Some(10), None, None), 300);
        assert_eq!(c, Classification::requeue(REASON_YOUNG));
    }

    #[test]
    fn an_unknown_kind_is_requeued_by_name() {
        let c = classify(
            &input(ItemKind::Unknown("epic".into()), Some(900), Some(Runtime::Claude), Some(SID)),
            300,
        );
        assert_eq!(c, Classification::requeue("unknown-kind-epic"));
    }

    #[test]
    fn the_knob_resolves_config_then_default() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        // Env is process-global; this test only exercises the config and
        // default tiers and does not set the env var.
        if std::env::var(MIN_RESUMABLE_AGE_ENV).is_ok() {
            return;
        }
        assert_eq!(resolve_min_resumable_age_secs(tmp.path()), DEFAULT_MIN_RESUMABLE_AGE_SECS);
        std::fs::write(
            tmp.path().join(".loom/config.json"),
            r#"{"autonomous":{"autoUpdate":{"pauseRoll":{"minResumableAgeSecs":0}}}}"#,
        )
        .unwrap();
        assert_eq!(resolve_min_resumable_age_secs(tmp.path()), 0);
    }
}
