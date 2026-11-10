//! Tests for the ratification lane's two-session dispatch (#13189).

use super::*;
use crate::role_runner::ratification_dispatch::{
    dispatch, role_name, Dispatched, InstructionSource, KeyOutcome, PUBLIC_SOURCES_ONLY,
};
use crate::role_runner::ratification_lane::{Key, Pin};
use std::collections::HashMap;

const EE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MKT_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const PR_REF: &str = "owner/repo#7@cccccccccccccccccccccccccccccccccccccccc";

fn pin(path: &str, commit: &str) -> Pin {
    Pin {
        repo: "keys/instructions".into(),
        path: path.into(),
        commit: commit.into(),
    }
}

struct Src {
    files: HashMap<String, Result<String, String>>,
    private: Option<bool>,
}

impl Src {
    fn ok(private: Option<bool>) -> Self {
        let mut files = HashMap::new();
        files.insert("ee.md".into(), Ok("EE-INSTRUCTIONS".into()));
        files.insert("market.md".into(), Ok("MARKET-INSTRUCTIONS".into()));
        Src { files, private }
    }
}

impl InstructionSource for Src {
    fn fetch(&self, pin: &Pin) -> Result<String, String> {
        self.files
            .get(&pin.path)
            .cloned()
            .unwrap_or_else(|| Err("missing".into()))
    }
    fn repo_is_private(&self, _repo: &str) -> Option<bool> {
        self.private
    }
}

#[derive(Default)]
struct Recorder {
    calls: Vec<(String, String)>,
}

impl RoleInvocationRunner for Recorder {
    fn invoke(&mut self, role: &str, prompt: &str) -> RoleTickOutcome {
        self.calls.push((role.to_string(), prompt.to_string()));
        RoleTickOutcome::Success
    }
}

fn run(src: &Src, ee: &Pin, mkt: &Pin) -> (Dispatched, Recorder) {
    let mut r = Recorder::default();
    let d = dispatch(ee, mkt, "owner/repo", PR_REF, src, &mut r);
    (d, r)
}

#[test]
fn one_pr_yields_two_sessions_with_disjoint_context() {
    let src = Src::ok(Some(true));
    let (d, r) = run(&src, &pin("ee.md", EE_SHA), &pin("market.md", MKT_SHA));
    assert_eq!(r.calls.len(), 2);
    assert_eq!(r.calls[0].0, role_name(Key::Ee));
    assert_eq!(r.calls[1].0, role_name(Key::Market));
    assert_ne!(r.calls[0].0, r.calls[1].0);
    // Only the PR reference is shared; neither prompt carries the other's text.
    assert!(r.calls[0].1.contains("EE-INSTRUCTIONS") && r.calls[0].1.contains(PR_REF));
    assert!(r.calls[1].1.contains("MARKET-INSTRUCTIONS") && r.calls[1].1.contains(PR_REF));
    assert!(!r.calls[0].1.contains("MARKET-INSTRUCTIONS"));
    assert!(!r.calls[1].1.contains("EE-INSTRUCTIONS"));
    assert_eq!(d.get(Key::Ee), &KeyOutcome::Ran(RoleTickOutcome::Success));
    assert_eq!(d.get(Key::Market), &KeyOutcome::Ran(RoleTickOutcome::Success));
}

#[test]
fn branch_or_short_ref_pin_fails_only_that_session_closed() {
    let src = Src::ok(Some(true));
    for bad in ["main", "abc1234", ""] {
        let (d, r) = run(&src, &pin("ee.md", bad), &pin("market.md", MKT_SHA));
        assert!(matches!(d.ee, KeyOutcome::FailedClosed(_)), "{bad:?}");
        assert!(matches!(d.market, KeyOutcome::Ran(_)));
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].0, role_name(Key::Market));
    }
}

#[test]
fn failed_or_empty_fetch_fails_closed_without_launching() {
    let mut src = Src::ok(Some(true));
    src.files.insert("market.md".into(), Err("HTTP 404".into()));
    let (d, r) = run(&src, &pin("ee.md", EE_SHA), &pin("market.md", MKT_SHA));
    assert!(matches!(d.market, KeyOutcome::FailedClosed(_)));
    assert!(matches!(d.ee, KeyOutcome::Ran(_)));
    assert_eq!(r.calls.len(), 1);

    src.files.insert("ee.md".into(), Ok("  \n".into()));
    src.files.insert("market.md".into(), Err("boom".into()));
    let (d, r) = run(&src, &pin("ee.md", EE_SHA), &pin("market.md", MKT_SHA));
    assert!(matches!(d.ee, KeyOutcome::FailedClosed(_)));
    assert!(matches!(d.market, KeyOutcome::FailedClosed(_)));
    assert!(r.calls.is_empty());
}

#[test]
fn public_or_unknown_visibility_constrains_market_only() {
    for private in [Some(false), None] {
        let src = Src::ok(private);
        let (_, r) = run(&src, &pin("ee.md", EE_SHA), &pin("market.md", MKT_SHA));
        assert!(r.calls[1].1.contains(PUBLIC_SOURCES_ONLY), "{private:?}");
        assert!(!r.calls[0].1.contains(PUBLIC_SOURCES_ONLY));
    }
    let src = Src::ok(Some(true));
    let (_, r) = run(&src, &pin("ee.md", EE_SHA), &pin("market.md", MKT_SHA));
    assert!(!r.calls[1].1.contains(PUBLIC_SOURCES_ONLY));
}

/// Issue #13749: the production runner rejects a role name that
/// `resolve_and_admit` does not know, which the `Recorder` stub never
/// exercised. This runner applies the real admission to each role name the
/// lane dispatches.
struct AdmittingRunner {
    root: std::path::PathBuf,
    admitted: Vec<String>,
    rejected: Vec<String>,
}

impl RoleInvocationRunner for AdmittingRunner {
    fn invoke(&mut self, role: &str, _prompt: &str) -> RoleTickOutcome {
        match crate::runtime_admission::resolve_and_admit(&self.root, role, None) {
            Ok(_) => {
                self.admitted.push(role.to_string());
                RoleTickOutcome::Success
            }
            Err(e) => {
                self.rejected.push(role.to_string());
                RoleTickOutcome::RuntimeRejected(e)
            }
        }
    }
}

#[test]
fn lane_sessions_pass_real_runtime_admission() {
    let _env = crate::runtime_selection_test_support::ClearedRuntimeSelectionEnv::new();
    let d = tempfile::tempdir().unwrap();
    for sub in ["defaults/roles", "defaults/runtimes", "defaults/scripts"] {
        std::fs::create_dir_all(d.path().join(sub)).unwrap();
    }
    std::fs::write(d.path().join("defaults/roles/judge.json"), "{}").unwrap();
    std::fs::write(
        d.path().join("defaults/runtimes/claude.json"),
        r#"{"runtime":"claude","capabilities":{"mcp":"yes"}}"#,
    )
    .unwrap();
    let adapter = d.path().join("defaults/scripts/spawn-claude.sh");
    std::fs::write(&adapter, "#!/bin/sh\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let src = Src::ok(Some(true));
    let mut r = AdmittingRunner {
        root: d.path().to_path_buf(),
        admitted: vec![],
        rejected: vec![],
    };
    let out = dispatch(
        &pin("ee.md", EE_SHA),
        &pin("market.md", MKT_SHA),
        "owner/repo",
        PR_REF,
        &src,
        &mut r,
    );
    assert!(r.rejected.is_empty(), "rejected: {:?}", r.rejected);
    assert_eq!(r.admitted, vec![role_name(Key::Ee), role_name(Key::Market)]);
    assert_eq!(out.get(Key::Ee), &KeyOutcome::Ran(RoleTickOutcome::Success));
    assert_eq!(out.get(Key::Market), &KeyOutcome::Ran(RoleTickOutcome::Success));
}
