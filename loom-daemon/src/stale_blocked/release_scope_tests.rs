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
/// per-owner credential `owner_dir`, and `pull` under any other.
fn owner_scoped_gh(bin: &Path, repo: &str, owner_dir: &Path) -> PathBuf {
    let gh = bin.join("gh");
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = api ] && [ \"$2\" = 'repos/{repo}' ]; then\n\
         if [ \"$GH_CONFIG_DIR\" = '{owner}' ]; then echo '{{\"push\":true}}'; \
         else echo '{{\"pull\":true}}'; fi\n\
         exit 0\n\
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
