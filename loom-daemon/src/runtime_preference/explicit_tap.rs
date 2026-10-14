//! Per-dispatch explicit tap (#11284): an operator-named `runtime` +
//! optional `modelProfile` for the coding phases (Builder, Doctor) of one
//! sweep.
//!
//! Like any other operator pin, an explicit tap **disables fall-through**: if
//! it is not admitted for the role, or its credential pool has nothing
//! spawnable right now, the dispatch is refused with the reason. It is never
//! silently handed down the configured preference list.

use super::availability::{availability_in, BinaryEnv};
use super::resolve::{SkipReason, Tap};
use super::{DispatchAdmission, PreferenceStamp};
use crate::runtime_admission::{RuntimeRejection, RuntimeSource};
use std::path::Path;

/// Roles an explicit per-dispatch tap may steer. Curator, Judge and the
/// orchestrator keep their role policy.
pub const EXPLICIT_TAP_ROLES: [&str; 2] = ["builder", "doctor"];

/// Validate the shape of a requested tap (no I/O).
///
/// # Errors
/// An actionable message for a blank runtime or a profile without a runtime.
pub fn parse_explicit_tap(
    runtime: Option<&str>,
    model_profile: Option<&str>,
) -> Result<Option<Tap>, String> {
    fn clean(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|v| !v.is_empty())
    }
    match (clean(runtime), clean(model_profile)) {
        (None, None) => Ok(None),
        (None, Some(profile)) => Err(format!(
            "modelProfile {profile:?} was given without a runtime; \
             name the runtime too (e.g. runtime=\"opencode\")"
        )),
        (Some(runtime), profile) => {
            let ok = |s: &str| {
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            };
            if !ok(runtime) || profile.is_some_and(|p| !ok(p)) {
                return Err(format!(
                    "invalid runtime/modelProfile {runtime:?}/{profile:?}: \
                     only letters, digits, '-', '_' and '.' are allowed"
                ));
            }
            Ok(Some(Tap {
                runtime: runtime.to_string(),
                model_profile: profile.map(str::to_string),
            }))
        }
    }
}

/// Resolve `role` under an explicit `tap`, refusing instead of falling through.
///
/// # Errors
/// A [`RuntimeRejection`] naming the tap and the reason when the role is not
/// steerable, the tap is not admitted for the role, or it has no spawnable
/// credential right now.
pub fn resolve_for_dispatch_tap(
    root: &Path,
    role: &str,
    tap: &Tap,
) -> Result<DispatchAdmission, RuntimeRejection> {
    resolve_for_dispatch_tap_in(root, role, tap, &BinaryEnv::from_process())
}

fn resolve_for_dispatch_tap_in(
    root: &Path,
    role: &str,
    tap: &Tap,
    binary_env: &BinaryEnv,
) -> Result<DispatchAdmission, RuntimeRejection> {
    let refuse = |reason: String, unmet: Vec<String>| RuntimeRejection {
        role: role.to_string(),
        runtime: tap.runtime.clone(),
        source: RuntimeSource::Explicit,
        unmet_capabilities: unmet,
        reason,
    };
    if !EXPLICIT_TAP_ROLES.contains(&role) {
        return Err(refuse(
            format!(
                "explicit tap {tap} is not accepted for role {role:?}: only builder and doctor \
                 phases can be steered per dispatch; {role:?} keeps its role policy"
            ),
            vec![],
        ));
    }
    let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
    let admitted = crate::runtime_admission::resolve_and_admit_profiled(
        root,
        role,
        Some(&tap.runtime),
        tap.model_profile.as_deref(),
        crate::runtime_admission::AdmissionContext::Host,
    )
    .map_err(|r| {
        let reason = format!("explicit tap {tap} refused for {role}: {}", r.reason);
        refuse(reason, r.unmet_capabilities)
    })?;
    let state = availability_in(root, tap, &admitted, now, binary_env);
    if let Some(skip) = state.skip_reason() {
        let kind = match &skip {
            SkipReason::Unavailable { .. } => "unavailable",
            _ => "not spawnable",
        };
        return Err(refuse(
            format!(
                "explicit tap {tap} is {kind} for {role} right now ({skip}); \
                 refusing rather than falling through to the preference list"
            ),
            vec![],
        ));
    }
    let mut admitted = admitted;
    admitted.preference = Some(PreferenceStamp {
        tier: 0,
        tap: tap.to_string(),
        marker: format!(
            "{}order={tap} tier=0 tap={tap} source=explicit",
            super::resolve::PREFERENCE_LOG_MARKER
        ),
        model_profile: tap.model_profile.clone(),
        model: None,
        draw: None,
    });
    Ok(DispatchAdmission {
        admitted: Some(admitted),
        backstop: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_fields_mean_no_tap() {
        assert_eq!(parse_explicit_tap(None, None), Ok(None));
        assert_eq!(parse_explicit_tap(Some("  "), Some("")), Ok(None));
    }

    #[test]
    fn runtime_alone_and_with_profile_parse() {
        let t = parse_explicit_tap(Some("codex"), None).unwrap().unwrap();
        assert_eq!((t.runtime.as_str(), t.model_profile), ("codex", None));
        let t = parse_explicit_tap(Some(" opencode "), Some("zai-flash"))
            .unwrap()
            .unwrap();
        assert_eq!(t.to_string(), "opencode:zai-flash");
    }

    #[test]
    fn profile_without_runtime_and_bad_chars_are_actionable_errors() {
        let e = parse_explicit_tap(None, Some("zai-flash")).unwrap_err();
        assert!(e.contains("without a runtime"), "{e}");
        let e = parse_explicit_tap(Some("open code"), None).unwrap_err();
        assert!(e.contains("invalid"), "{e}");
        assert!(parse_explicit_tap(Some("opencode"), Some("a/b")).is_err());
    }

    #[test]
    fn judge_curator_and_orchestrator_are_not_steerable() {
        let dir = tempfile::tempdir().unwrap();
        for role in ["judge", "curator", "sweep-lifecycle"] {
            let err =
                resolve_for_dispatch_tap(dir.path(), role, &Tap::runtime("claude")).unwrap_err();
            assert!(err.reason.contains("keeps its role policy"), "{}", err.reason);
            assert_eq!(err.source, RuntimeSource::Explicit);
        }
    }

    #[test]
    fn unknown_runtime_is_refused_without_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_for_dispatch_tap(dir.path(), "builder", &Tap::runtime("no-such-rt"))
            .unwrap_err();
        assert!(err.reason.contains("explicit tap no-such-rt"), "{}", err.reason);
    }
}
