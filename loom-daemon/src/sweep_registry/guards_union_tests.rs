//! Regression coverage for non-closing PR references (#7757).
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::sweep_registry::test_support::open_pr_guard_rest_fallback_registry;
use serial_test::serial;
use tempfile::tempdir;

pub(super) fn empty_graphql_registry(ws: &Path, pr: &str, status: i32) -> (SweepRegistry, PathBuf) {
    let (reg, log) = open_pr_guard_rest_fallback_registry(ws, pr, status, false);
    let fake = ws.join("fake-gh.sh");
    let script = std::fs::read_to_string(&fake).unwrap();
    let failure = "printf 'gh: rate limit exceeded\\n' >&2\nexit 1";
    assert!(script.contains(failure));
    let empty = r#"printf '%s\n' '{"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[]}}}}}'
exit 0"#;
    std::fs::write(fake, script.replace(failure, empty)).unwrap();
    (reg, log)
}

#[test]
fn empty_graphql_consults_timeline_union() {
    for (pr, status, expected) in [
        ("5460", 0, OpenPrProbe::Open(5460)),
        ("", 0, OpenPrProbe::NoneOpen),
        ("", 1, OpenPrProbe::ProbeFailed),
        ("malformed", 0, OpenPrProbe::ProbeFailed),
    ] {
        let dir = tempdir().unwrap();
        let (reg, log) = empty_graphql_registry(dir.path(), pr, status);
        assert_eq!(reg.probe_open_linked_pr_transports(5240), expected);
        let calls = std::fs::read_to_string(log).unwrap();
        assert_eq!(calls.lines().filter(|s| s.contains("api graphql")).count(), 1);
        assert_eq!(
            calls
                .lines()
                .filter(|s| s.contains("/issues/5240/timeline"))
                .count(),
            1
        );
    }
}

#[test]
#[serial]
fn nonclosing_pr_refuses_dispatch_before_label_mutation() {
    let dir = tempdir().unwrap();
    let (mut reg, log) = empty_graphql_registry(dir.path(), "5460", 0);
    let err = reg
        .dispatch(&SweepKind::Issue(5240), None, None, None, None)
        .expect_err("non-closing PR must prevent duplicate dispatch");
    assert!(err.downcast_ref::<OpenPrDispatchError>().is_some(), "{err}");
    let calls = std::fs::read_to_string(log).unwrap();
    assert!(!calls.contains("issue edit"), "{calls}");
}
