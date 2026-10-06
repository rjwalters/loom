//! Test-harness guard against the real `gh` (#10138).
//!
//! loom-daemon's lib and bin test binaries used to spawn the operator's real
//! `gh` against live GitHub — about 1,840 calls a day out of the same hourly
//! REST pool the fleet runs dry on. #10088 routed every `gh_bin()` caller
//! through a loud-failing stub, but any call site that spells `"gh"` itself
//! (or a shell script a test runs) still found the real binary on `PATH`.
//!
//! This module closes that hole at the harness level, for every spawn path at
//! once. Before `main` runs, a test binary:
//!
//! 1. writes a `gh` stand-in into a per-process temp dir. It prints
//!    `live gh in test: <argv>` to stderr, appends the argv to
//!    `$LOOM_LIVE_GH_LOG`, and exits non-zero;
//! 2. prepends that dir to `PATH`, so a bare `gh` resolves to the stand-in,
//!    and points `LOOM_LIVE_GH_LOG` at a fresh log file;
//! 3. registers an `atexit` hook that reads the log back and, when any call
//!    was recorded, prints each argv as `live gh in test: <argv>` and exits
//!    101. The run fails even when the leaking test swallowed the error.
//!
//! Under nextest (process per test) the failure names the leaking test. Under
//! plain `cargo test` it fails the binary and names the argv.
//!
//! A test that wants a fake `gh` keeps working as long as it puts its fake
//! *ahead of* the inherited `PATH` (the usual `format!("{fake}:{PATH}")`), or
//! sets `LOOM_GH_BIN`, or replaces `PATH` outright. Only a spawn that falls
//! through to the inherited `PATH` reaches the guard.
//!
//! The module is compiled only under `cfg(test)`, into both the lib test binary
//! (`lib.rs`) and the bin test binary (`cli/mod.rs`, via `#[path]`). The
//! shipped binary never contains it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Env var naming the file the stand-in appends each argv line to.
pub(crate) const LOG_ENV: &str = "LOOM_LIVE_GH_LOG";

/// Prefix of every guard message. It is the contract the issue names.
pub(crate) const MESSAGE_PREFIX: &str = "live gh in test: ";

/// Exit code of the stand-in `gh`. It is non-zero and distinct from the
/// `LOOM_GH_BIN` stub's 127, so a log line tells you which guard fired.
pub(crate) const STUB_EXIT: i32 = 97;

/// The stand-in `gh` script.
pub(crate) const SCRIPT: &str = "#!/bin/sh\n\
printf 'live gh in test: %s\\n' \"$*\" >&2\n\
if [ -n \"$LOOM_LIVE_GH_LOG\" ]; then printf '%s\\n' \"$*\" >> \"$LOOM_LIVE_GH_LOG\"; fi\n\
exit 97\n";

/// The per-process guard dir. Set once by [`install`].
static GUARD_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Run [`install`] before `main` (the same mechanism the `ctor` crate uses),
/// so that no test thread exists yet while `PATH` is rewritten.
#[used]
#[cfg_attr(
    any(target_os = "linux", target_os = "android"),
    link_section = ".init_array"
)]
#[cfg_attr(target_os = "macos", link_section = "__DATA,__mod_init_func")]
static INSTALL: extern "C" fn() = install;

extern "C" fn install() {
    // A guard that cannot be set up must not bring down every test with a
    // confusing pre-main panic. Report it and let the run go on unguarded.
    if let Err(e) = try_install() {
        eprintln!("loom-daemon live-gh guard: not installed: {e}");
    }
}

fn try_install() -> std::io::Result<()> {
    let dir = std::env::temp_dir().join(format!("loom-live-gh-guard-{}", std::process::id()));
    write_stub(&dir)?;
    let log = dir.join("calls.log");
    std::fs::write(&log, b"")?;

    let path = prepend_path(&dir, std::env::var_os("PATH"))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    std::env::set_var("PATH", path);
    std::env::set_var(LOG_ENV, &log);
    let _ = GUARD_DIR.set(dir);
    // SAFETY: registering a plain `extern "C" fn()` with libc's atexit table.
    unsafe {
        libc::atexit(check_at_exit);
    }
    Ok(())
}

/// Write the stand-in `gh` into `dir` (created if missing), mode 0755.
pub(crate) fn write_stub(dir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    let gh = dir.join("gh");
    std::fs::write(&gh, SCRIPT)?;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))?;
    Ok(gh)
}

/// `dir` followed by the existing `PATH` entries.
pub(crate) fn prepend_path(
    dir: &Path,
    current: Option<OsString>,
) -> Result<OsString, std::env::JoinPathsError> {
    let mut entries = vec![dir.to_path_buf()];
    if let Some(current) = current {
        entries.extend(std::env::split_paths(&current));
    }
    std::env::join_paths(entries)
}

/// The failure report for a log's contents, or `None` when it is empty.
pub(crate) fn report(log: &str) -> Option<String> {
    let calls: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
    if calls.is_empty() {
        return None;
    }
    let mut out = format!(
        "loom-daemon test binary spawned the real `gh` {} time(s) (#10138). \
         Route the test through a fake gh (LOOM_GH_BIN, a gh_bin argument, or a \
         fake ahead of PATH):\n",
        calls.len()
    );
    for call in calls {
        out.push_str(MESSAGE_PREFIX);
        out.push_str(call);
        out.push('\n');
    }
    Some(out)
}

extern "C" fn check_at_exit() {
    let Some(dir) = GUARD_DIR.get() else {
        return;
    };
    let log = std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(dir);
    if let Some(report) = report(&log) {
        eprintln!("{report}");
        // SAFETY: terminating the process from an atexit hook. `_exit` skips
        // the remaining hooks, which is fine because nothing else is pending.
        unsafe { libc::_exit(101) }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::process::Command;

    // `#[serial]` below: a few `release_fetch` tests briefly replace `PATH`
    // with `<dir>:/usr/bin:/bin` under `#[serial]`. A bare `gh` looked up in
    // that window would resolve to the real `gh`, so every test here that
    // reads `PATH` for the guard serialises against them.

    /// Where a bare `gh` resolves on `path`: the first `<dir>/gh` that is an
    /// executable file, as `execvp` looks it up. The self-tests spawn that
    /// resolved stand-in by path, after asserting what it is, instead of a raw
    /// `Command::new("gh")`. #9985's choke-point scan admits no new raw site
    /// (#10249), and a spawn that checks its target first can never reach the
    /// real `gh`. Do not "simplify" this back to a bare `gh` spawn.
    fn resolve_bare(path: &std::ffi::OsStr) -> Option<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        std::env::split_paths(path).map(|d| d.join("gh")).find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    }

    /// The guard's stand-in, asserted to be what a bare `gh` resolves to now.
    fn guard_stand_in() -> PathBuf {
        let resolved = resolve_bare(&std::env::var_os("PATH").unwrap_or_default());
        let expected = GUARD_DIR
            .get()
            .expect("guard installed before main")
            .join("gh");
        assert_eq!(
            resolved.as_deref(),
            Some(expected.as_path()),
            "bare gh must resolve to the guard"
        );
        expected
    }

    #[test]
    #[serial]
    fn guard_is_installed_first_on_path() {
        let dir = GUARD_DIR.get().expect("guard installed before main");
        let first = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .next()
            .unwrap();
        assert_eq!(&first, dir);
        assert!(dir.join("gh").is_file());
    }

    #[test]
    #[serial]
    fn bare_gh_hits_the_guard_and_names_argv() {
        // Point the stand-in at a private log, so this deliberate spawn does
        // not trip the process-wide exit check.
        let scratch = tempfile::tempdir().unwrap();
        let log = scratch.path().join("calls.log");
        let stand_in = guard_stand_in();
        let out = Command::new(&stand_in)
            .args(["api", "--paginate", "repos/o/r/pulls"])
            .env(LOG_ENV, &log)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(STUB_EXIT));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("live gh in test: api --paginate repos/o/r/pulls"), "{stderr}");
        let logged = std::fs::read_to_string(&log).unwrap();
        assert_eq!(logged, "api --paginate repos/o/r/pulls\n");
        let report = report(&logged).unwrap();
        assert!(report.contains("live gh in test: api --paginate repos/o/r/pulls"));
    }

    #[test]
    fn a_fake_ahead_of_path_still_wins() {
        let scratch = tempfile::tempdir().unwrap();
        let fake = scratch.path().join("gh");
        std::fs::write(&fake, "#!/bin/sh\necho fake\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = prepend_path(scratch.path(), std::env::var_os("PATH")).unwrap();
        assert_eq!(resolve_bare(&path), Some(fake.clone()));
        let out = Command::new(&fake).env("PATH", path).output().unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "fake\n");
    }

    /// A test that leaks a real-`gh` spawn and swallows the error. Ignored, so
    /// it runs only inside the child process of the self-test below.
    #[test]
    #[ignore = "fixture: run only by a_leaking_test_fails_its_binary_naming_argv"]
    fn leaking_fixture() {
        let _ = Command::new(guard_stand_in())
            .args(["api", "repos/o/r/pulls", "--paginate"])
            .output();
    }

    #[test]
    #[serial]
    fn a_leaking_test_fails_its_binary_naming_argv() {
        let module = module_path!().split_once("::").unwrap().1;
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                &format!("{module}::leaking_fixture"),
                "--exact",
                "--ignored",
            ])
            .env_remove(LOG_ENV)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // The fixture itself passed; the exit hook is what failed the binary.
        assert!(stdout.contains("1 passed"), "stdout: {stdout}\nstderr: {stderr}");
        assert_eq!(out.status.code(), Some(101), "stderr: {stderr}");
        assert!(
            stderr.contains("live gh in test: api repos/o/r/pulls --paginate"),
            "stderr: {stderr}"
        );
    }

    #[test]
    fn empty_log_reports_nothing() {
        assert_eq!(report(""), None);
        assert_eq!(report("\n  \n"), None);
    }
}
