//! #10837: the release pass's comment write is vetted under the workspace's
//! own credential, not the daemon process's.
//!
//! The fleet incident: a daemon whose home workspace belongs to one owner
//! serves another owner's repositories through a per-owner `GH_CONFIG_DIR`
//! (#5401). The tick's gate probed that per-root credential and admitted the
//! root, but the audit comment re-vetted with `may_write_from`, which probes
//! the process credential. That credential cannot write the other owner's
//! repository, so every release and re-park failed before writing anything:
//! `failed=7` on every pass, never a reason in the log.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use super::GhReleaseForge;
use crate::credential_preflight::{clear_owner_root_registry, register_root_gh_config_dir};
use crate::write_scope_test_support::WritableRoot;

/// A `gh` whose permission probe for `repo` reports `push` only under the
/// per-owner credential `owner_dir`, and `pull` under any other. Its write
/// calls (`gh api -X …`) succeed only under `owner_dir` and fail (HTTP 403)
/// under any other credential, so a successful write proves which one it used.
fn owner_scoped_gh(bin: &Path, repo: &str, owner_dir: &Path) -> PathBuf {
    let gh = bin.join("gh");
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = api ] && [ \"$2\" = 'repos/{repo}' ]; then\n\
         if [ \"$GH_CONFIG_DIR\" = '{owner}' ]; then echo '{{\"push\":true}}'; \
         else echo '{{\"pull\":true}}'; fi\n\
         exit 0\n\
         fi\n\
         if [ \"$1\" = api ] && [ \"$2\" = -X ]; then\n\
         if [ \"$GH_CONFIG_DIR\" = '{owner}' ]; then exit 0; fi\n\
         echo 'HTTP 403: write under the wrong credential' >&2; exit 1\n\
         fi\n\
         echo \"fake gh: unexpected call: $*\" >&2\n\
         exit 1\n",
        owner = owner_dir.display(),
    );
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    gh
}

/// Points `LOOM_GH_BIN` at the fake for the test's duration, so a vetting
/// path that ignores the pass's own `gh_bin` still probes the same fake and
/// fails only on the credential, never on the binary.
struct GhBinEnv(Option<String>);

impl GhBinEnv {
    fn set(gh: &Path) -> Self {
        let prev = std::env::var("LOOM_GH_BIN").ok();
        std::env::set_var("LOOM_GH_BIN", gh);
        Self(prev)
    }
}

impl Drop for GhBinEnv {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => std::env::set_var("LOOM_GH_BIN", v),
            None => std::env::remove_var("LOOM_GH_BIN"),
        }
    }
}

fn forge(root: &Path, gh: &Path) -> GhReleaseForge {
    let mut f = GhReleaseForge::new(root, None);
    f.gh_bin = gh.to_path_buf();
    f.repo = None;
    f
}

#[test]
#[serial_test::serial]
fn cross_owner_root_write_is_vetted_under_the_roots_own_credential() {
    clear_owner_root_registry();
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path())
        .unwrap()
        .join("other-owner");
    let fixture = WritableRoot::register(&root);
    let owner_dir = std::fs::canonicalize(dir.path())
        .unwrap()
        .join("gh-config-by-owner");
    std::fs::create_dir_all(&owner_dir).unwrap();
    let bin = tempfile::tempdir().unwrap();
    let gh = owner_scoped_gh(bin.path(), &fixture.repo, &owner_dir);
    let _env = GhBinEnv::set(&gh);

    // Control: with no per-owner credential registered, the probe runs under
    // the process credential (`pull`), so the write must be refused.
    let refused = forge(&root, &gh).write_scope();
    assert!(refused.is_err(), "process credential must not vouch: {refused:?}");

    // The fleet shape: the root is served through its owner's credential,
    // which can write. The tick's gate admits it, and so must the comment.
    register_root_gh_config_dir(&root, &owner_dir);
    assert!(crate::write_scope::root_writable_with(&root, &gh).is_allowed());
    let vetted = forge(&root, &gh).write_scope();
    assert_eq!(vetted, Ok(()), "the comment must use the root's credential");

    // The fixture's drop removes the owner-keyed cached answer; the control's
    // process-keyed one is removed here, so the run leaves no cache entry.
    let scope = crate::write_scope::probe::CacheScope::GitHub;
    if let Some(key) = crate::write_scope::probe::cache_key(None, &scope, &fixture.repo) {
        let _ = std::fs::remove_file(
            crate::write_scope::probe::cache_dir().join(format!("{key}.json")),
        );
    }
    drop(fixture);
    clear_owner_root_registry();
}

/// #11437: the re-park body write and the label writes (`run_for_root`'s park
/// forge) are vetted under the root's own credential too.
#[test]
#[serial_test::serial]
fn park_forge_writes_are_vetted_under_the_roots_own_credential() {
    use crate::operator_decision::cli::{Forge, GhForge};

    clear_owner_root_registry();
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("other-owner");
    let fixture = WritableRoot::register(&root);
    let owner_dir = base.join("gh-config-by-owner");
    std::fs::create_dir_all(&owner_dir).unwrap();
    let bin = tempfile::tempdir().unwrap();
    let gh = owner_scoped_gh(bin.path(), &fixture.repo, &owner_dir);
    let _env = GhBinEnv::set(&gh);
    let refused =
        |e: &Result<(), String>| e.as_ref().is_err_and(|m| m.contains("refusing the write"));

    // CLI default: the process credential (`pull`) refuses, as before.
    let mut cli = GhForge::new(root.clone(), None);
    assert!(refused(&cli.set_body(1, "x")), "default stays process-credential");

    // Cross-owner root, no registered credential: still refused.
    let mut park = GhForge::for_root_credential(root.clone(), None, gh.clone());
    assert!(refused(&park.set_body(1, "x")));

    // With the owner's WRITE credential registered, the body and label
    // writes pass the scope check and succeed. The fake fails any write not
    // made under the owner's `GH_CONFIG_DIR`, so `Ok` proves the credential.
    register_root_gh_config_dir(&root, &owner_dir);
    let mut park = GhForge::for_root_credential(root.clone(), None, gh.clone());
    assert_eq!(park.set_body(1, "x"), Ok(()));
    assert_eq!(park.add_labels(1, &["loom:blocked".to_string()]), Ok(()));
    assert_eq!(park.remove_label(1, "loom:issue"), Ok(()));
    // An explicit repo is vetted and written the same way.
    let mut park =
        GhForge::for_root_credential(root.clone(), Some(fixture.repo.clone()), gh.clone());
    assert_eq!(park.set_body(1, "x"), Ok(()));
    // The CLI default keeps vetting under the process credential.
    let mut cli = GhForge::new(root.clone(), None);
    assert!(refused(&cli.set_body(1, "x")), "CLI callers are unchanged");

    // Negative: a root whose own credential is read-only still refuses.
    let ro_root = base.join("ro-owner");
    let ro = WritableRoot::read_only(&ro_root, None);
    register_root_gh_config_dir(&ro_root, &owner_dir);
    let mut park = GhForge::for_root_credential(ro_root, None, ro.gh.clone());
    assert!(refused(&park.set_body(1, "x")), "read-only root credential refuses");

    let scope = crate::write_scope::probe::CacheScope::GitHub;
    if let Some(key) = crate::write_scope::probe::cache_key(None, &scope, &fixture.repo) {
        let _ = std::fs::remove_file(
            crate::write_scope::probe::cache_dir().join(format!("{key}.json")),
        );
    }
    drop((fixture, ro));
    clear_owner_root_registry();
}

/// #11437: a repeated write-failure warning is suppressed per (workspace,
/// reason) for an hour, and the next one reports how many identical ones
/// were swallowed.
#[test]
fn failure_warning_same_reason_is_suppressed_with_a_counter() {
    use super::{failure_warn_due_in, FailureWarnings, FAILURE_WARN_SECS};
    use std::time::{Duration, Instant};

    let mut map = FailureWarnings::new();
    let mut failure_warn_due =
        |slug: &str, why: &str, now: Instant| failure_warn_due_in(&mut map, slug, why, now);

    let t0 = Instant::now();
    let (slug, why) = ("/tmp/rate-limit-same-11437", "refusing the write (#9548): pull only");
    assert_eq!(failure_warn_due(slug, why, t0), Some(0));
    assert_eq!(failure_warn_due(slug, why, t0 + Duration::from_secs(60)), None);
    assert_eq!(failure_warn_due(slug, why, t0 + Duration::from_secs(120)), None);
    let later = t0 + Duration::from_secs(FAILURE_WARN_SECS + 1);
    assert_eq!(failure_warn_due(slug, why, later), Some(2));
    assert_eq!(failure_warn_due(slug, why, later + Duration::from_secs(1)), None);
}

/// #11437: a different failure reason inside the window warns immediately —
/// it is never folded into the previous reason's "identical" count — and the
/// first reason keeps its own window and counter.
#[test]
fn failure_warning_changed_reason_warns_immediately() {
    use super::{failure_warn_due_in, FailureWarnings, FAILURE_WARN_SECS};
    use std::time::{Duration, Instant};

    let mut map = FailureWarnings::new();
    let mut failure_warn_due =
        |slug: &str, why: &str, now: Instant| failure_warn_due_in(&mut map, slug, why, now);

    let t0 = Instant::now();
    let slug = "/tmp/rate-limit-changed-11437";
    let (refusal, write_err) = ("refusing the write (#9548): pull only", "HTTP 422: invalid");
    assert_eq!(failure_warn_due(slug, refusal, t0), Some(0));
    assert_eq!(failure_warn_due(slug, refusal, t0 + Duration::from_secs(10)), None);
    // The new reason is visible at once, with nothing counted against it.
    assert_eq!(failure_warn_due(slug, write_err, t0 + Duration::from_secs(20)), Some(0));
    assert_eq!(failure_warn_due(slug, write_err, t0 + Duration::from_secs(30)), None);
    // Flipping back to the first reason within its window stays suppressed.
    assert_eq!(failure_warn_due(slug, refusal, t0 + Duration::from_secs(40)), None);
    // Each reason reports only its own repeats when its window lapses.
    let later = t0 + Duration::from_secs(FAILURE_WARN_SECS + 25);
    assert_eq!(failure_warn_due(slug, refusal, later), Some(2));
    assert_eq!(failure_warn_due(slug, write_err, later), Some(1));
}

/// #11437: workspaces keep separate windows and counters, even for the same
/// failure reason.
#[test]
fn failure_warning_counters_are_per_workspace() {
    use super::{failure_warn_due_in, FailureWarnings, FAILURE_WARN_SECS};
    use std::time::{Duration, Instant};

    let mut map = FailureWarnings::new();
    let mut failure_warn_due =
        |slug: &str, why: &str, now: Instant| failure_warn_due_in(&mut map, slug, why, now);

    let t0 = Instant::now();
    let why = "refusing the write (#9548): pull only";
    let (a, b) = ("/tmp/rate-limit-ws-a-11437", "/tmp/rate-limit-ws-b-11437");
    assert_eq!(failure_warn_due(a, why, t0), Some(0));
    assert_eq!(failure_warn_due(b, why, t0), Some(0), "b is not suppressed by a");
    assert_eq!(failure_warn_due(a, why, t0 + Duration::from_secs(5)), None);
    assert_eq!(failure_warn_due(a, why, t0 + Duration::from_secs(6)), None);
    assert_eq!(failure_warn_due(b, why, t0 + Duration::from_secs(7)), None);
    let later = t0 + Duration::from_secs(FAILURE_WARN_SECS + 10);
    assert_eq!(failure_warn_due(a, why, later), Some(2));
    assert_eq!(failure_warn_due(b, why, later), Some(1));
}
