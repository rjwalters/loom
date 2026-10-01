//! Library-test fixture: a workspace this installation may write to (#9548).
//!
//! Every daemon pass that writes runs [`crate::write_scope`]'s real gate
//! first, in tests as in production; nothing in the gate knows it is under
//! test. So a test that drives one of those passes needs a root the real
//! gate admits, which is what a real workspace is:
//!
//! - a git checkout whose only GitHub remote is `origin`, pointing at a
//!   repository unique to this fixture (`loom-fixture/<id>`), so no two tests
//!   share a cached permission answer;
//! - Loom installed (a `.loom/` directory), which makes that origin managed
//!   here; and
//! - a `gh` that answers the permission probe (`gh api repos/OWNER/REPO`) for
//!   that repository, with `push` or, for a negative control, `pull`. Every
//!   other call is handed to the test's own fake `gh` when it has one.
//!
//! The fake `gh` is given to the gate the way production gives it the `gh`
//! its writes use: as the pass's `gh_bin`, the runner's or sweep config's
//! `gh_bin`, or `LOOM_GH_BIN` for code that reads it.
//!
//! A test that reaches the gate holds the default `serial_test` key: the
//! gate reads the process-global `LOOM_REPO` / `GH_REPO`, whose test writers
//! hold that key, and a concurrent write would retarget it. A test that
//! needs another key too nests, `#[serial(key)] fn t() { t_body() }` around
//! `#[serial] fn t_body()`, that key outside and the default inside (the
//! order `role_collision` documents).

use std::path::{Path, PathBuf};
use std::process::Command;

/// A registered, managed checkout and the fake `gh` that vouches for it.
pub(crate) struct WritableRoot {
    root: PathBuf,
    /// `owner/repo` of the fixture's origin.
    pub(crate) repo: String,
    /// The fake `gh` to hand to the gate (and, through it, to the pass).
    pub(crate) gh: PathBuf,
    _bin: tempfile::TempDir,
}

impl WritableRoot {
    /// `root`, registered, with a `gh` that reports `push` and fails
    /// everything else.
    pub(crate) fn register(root: &Path) -> Self {
        Self::build(root, "push", None)
    }

    /// `root`, registered, with a `gh` that reports `push` and hands every
    /// other call to `inner`.
    pub(crate) fn register_with_gh(root: &Path, inner: &Path) -> Self {
        Self::build(root, "push", Some(inner))
    }

    /// A negative control: `root` is registered exactly as above, but the
    /// credential only has `pull`, so the real gate must refuse it.
    pub(crate) fn read_only(root: &Path, inner: Option<&Path>) -> Self {
        Self::build(root, "pull", inner)
    }

    fn build(root: &Path, permission: &str, inner: Option<&Path>) -> Self {
        std::fs::create_dir_all(root).expect("create fixture root");
        let id = uuid::Uuid::new_v4().simple().to_string();
        let repo = format!("loom-fixture/w{}", &id[..12]);
        if !root.join(".git").exists() {
            git(root, &["init", "-q"]);
        }
        let url = format!("https://github.com/{repo}.git");
        let has_origin = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(root)
            .output()
            .is_ok_and(|o| o.status.success());
        if has_origin {
            git(root, &["remote", "set-url", "origin", &url]);
        } else {
            git(root, &["remote", "add", "origin", &url]);
        }
        std::fs::create_dir_all(root.join(".loom")).expect("create .loom");

        let bin = tempfile::tempdir().expect("fake gh dir");
        let gh = bin.path().join("gh");
        let rest = match inner {
            Some(inner) => format!("exec '{}' \"$@\"\n", inner.display()),
            None => "echo \"fake gh: unexpected call: $*\" >&2\nexit 1\n".to_string(),
        };
        let script = format!(
            "#!/bin/sh\n\
             # #9548 write-scope permission probe for the registered fixture.\n\
             if [ \"$1\" = api ] && [ \"$2\" = 'repos/{repo}' ]; then\n\
             echo '{{\"{permission}\":true}}'\n\
             exit 0\n\
             fi\n\
             if [ \"$1\" = api ] && [ \"$2\" = installation/repositories ]; then\n\
             echo 'fake gh: not an App installation token' >&2\n\
             exit 1\n\
             fi\n\
             {rest}"
        );
        std::fs::write(&gh, script).expect("write fake gh");
        std::fs::set_permissions(&gh, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod fake gh");
        Self {
            root: root.to_path_buf(),
            repo,
            gh,
            _bin: bin,
        }
    }
}

impl Drop for WritableRoot {
    /// Remove the permission answer the probe cached for this fixture's
    /// repository, so test runs leave nothing in the host's cache directory.
    fn drop(&mut self) {
        let dir = crate::write_scope::probe::cache_dir();
        let key_dir = crate::credential_preflight::gh_config_dir_for_root(&self.root);
        let scope = crate::write_scope::probe::CacheScope::GitHub;
        if let Some(key) =
            crate::write_scope::probe::cache_key(key_dir.as_deref(), &scope, &self.repo)
        {
            let _ = std::fs::remove_file(dir.join(format!("{key}.json")));
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok, "git {args:?} in {}", dir.display());
}

/// A machine-level Claude token pool for a registered checkout.
///
/// A registered root is a git checkout, and Loom refuses a token pool inside
/// one (#9135), so a test that needs a pool puts it where a real workspace
/// has it: in the shared pool (`LOOM_SHARED_TOKENS_DIR`), outside the
/// checkout. The variable is restored on drop; callers must be `#[serial]`
/// with the other tests that set it.
const SHARED_TOKENS_DIR_ENV: &str = "LOOM_SHARED_TOKENS_DIR";

pub(crate) struct SharedTokenPool {
    dir: tempfile::TempDir,
    prior: Option<std::ffi::OsString>,
}

impl SharedTokenPool {
    /// A shared pool holding one fake `*.token` account.
    pub(crate) fn one_account() -> Self {
        let dir = tempfile::tempdir().expect("shared pool dir");
        std::fs::write(dir.path().join("fake.token"), "sk-ant-oat01-fake").expect("token");
        let prior = std::env::var_os(SHARED_TOKENS_DIR_ENV);
        std::env::set_var(SHARED_TOKENS_DIR_ENV, dir.path());
        Self { dir, prior }
    }

    /// The pool directory (where `.bad_tokens` and friends live).
    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for SharedTokenPool {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var(SHARED_TOKENS_DIR_ENV, v),
            None => std::env::remove_var(SHARED_TOKENS_DIR_ENV),
        }
    }
}
