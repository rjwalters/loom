//! The startup supervision drop-in (#11111): deliver #11058's systemd
//! supervision settings to a host whose unit predates them, without a re-render.
//!
//! A host only gets a re-rendered unit from `loom-daemon-update.sh --relaunch`
//! or a fresh start against a stopped daemon. A floor roll is a supervised
//! exit-0 relaunch and re-renders nothing, so a floor raise would move every
//! host to a binary with the #11058 fix while its unit kept `Restart=on-success`
//! and the default `OOMPolicy=stop`.
//!
//! So at startup, when systemd supervises it, the daemon writes
//! `~/.config/systemd/user/<unit>.service.d/zz-loom-supervision.conf` and runs
//! `systemctl --user daemon-reload`. systemd reads `Restart=` and the start
//! limit when the daemon exits, so the settings apply from the next exit with
//! no restart needed. The drop-in covers both the canonical unit and the
//! `fleet add-worker` unit without touching their other lines.
//!
//! The content comes from [`render::systemd_supervision_block`] and
//! [`render::SYSTEMD_START_LIMIT`], the same source the unit renderer uses, so
//! the two cannot drift. `RestartPreventExitStatus=` and `SuccessExitStatus=`
//! ADD to their lists in systemd, so each gets an empty reset line first;
//! without it the old unit's `RestartPreventExitStatus=143 130` would be kept
//! alongside the new codes.
//!
//! systemd applies drop-ins in file-name order, so a later file in the same
//! directory wins. The pre-#11111 retrofit hint had operators write
//! `supervisor.conf` with `Restart=on-success`, so the file is named
//! [`DROPIN_NAME`] (`zz-…`) to sort after it and after the other usual operator
//! names (`override.conf`, `NN-*.conf`). A file that still sorts later and sets
//! the same keys is rare; [`shadowing_dropins`] finds one, and startup names it
//! at WARN. A copy under the pre-release name [`LEGACY_DROPIN_NAME`] is removed
//! when the drop-in is written, so two copies never coexist.
//!
//! Only the unit's own main process writes (#11111 review). `LOOM_DAEMON_SUPERVISOR`
//! is inherited by every sweep child and every test daemon a sweep's
//! `cargo test` starts, so the env var alone would let a branch-under-test
//! binary write its supervision block into the live unit's drop-in and
//! `daemon-reload` the production user manager (the #8077 hazard). So before
//! writing, the daemon asks `systemctl --user show -p MainPID --value <unit>`
//! and acts only when the answer is its own pid. Any error, an empty value or
//! `0` means "not us": it skips, logging at DEBUG. This also covers a duplicate
//! daemon that runs before the singleton guard refuses it.
//!
//! Every failure is logged at WARN and swallowed: a daemon that cannot write
//! its own drop-in is still a working daemon.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::render;

/// The drop-in's file name inside `<unit>.service.d/`. It starts with `zz-` so
/// it sorts after any operator drop-in, and the last drop-in wins.
pub const DROPIN_NAME: &str = "zz-loom-supervision.conf";

/// The name an unreleased build of #11111 used. It sorted before an operator's
/// `supervisor.conf`; [`ensure_with`] removes it when present.
pub const LEGACY_DROPIN_NAME: &str = "50-supervision.conf";

/// The directives systemd treats as lists, where a later assignment ADDS to the
/// earlier ones. Each needs an empty `Key=` reset before its value.
pub const LIST_RESET_KEYS: &[&str] = &["RestartPreventExitStatus", "SuccessExitStatus"];

/// The drop-in text: `[Unit]` start limit, then the `[Service]` directives of
/// the shared supervision block, list keys reset first.
#[must_use]
pub fn render_dropin() -> String {
    let mut s = String::from(
        "# Written by loom-daemon at startup (#11111). Delivers the #11058 supervision\n\
         # settings to a unit rendered before them. The source of truth is\n\
         # daemon_start::render::systemd_supervision_block; edits here are overwritten.\n\
         [Unit]\n",
    );
    s.push_str(render::SYSTEMD_START_LIMIT);
    s.push_str("\n[Service]\n");
    for line in render::systemd_supervision_block().lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            if LIST_RESET_KEYS.contains(&key) {
                s.push_str(key);
                s.push_str("=\n");
            }
        }
        s.push_str(line);
        s.push('\n');
    }
    s
}

/// `unit` with the `.service` suffix systemd implies for a bare name (see
/// [`super::platform::systemd_unit`]).
#[must_use]
pub fn service_name(unit: &str) -> String {
    if unit.ends_with(".service") {
        unit.to_string()
    } else {
        format!("{unit}.service")
    }
}

/// The drop-in directory for `unit`.
#[must_use]
pub fn dropin_dir(unit_dir: &Path, unit: &str) -> PathBuf {
    unit_dir.join(format!("{}.d", service_name(unit)))
}

/// The filesystem and `systemctl` this module touches, behind a seam so tests
/// never reach real systemd.
pub trait DropinHost {
    /// The current file content, `Ok(None)` when it does not exist.
    ///
    /// # Errors
    /// Any read failure other than not-found.
    fn read(&self, path: &Path) -> io::Result<Option<String>>;
    /// Write `content` atomically, creating the parent directory.
    ///
    /// # Errors
    /// Any I/O failure.
    fn write_atomic(&self, path: &Path, content: &str) -> io::Result<()>;
    /// Remove `path`. A missing file is not an error.
    ///
    /// # Errors
    /// Any removal failure other than not-found.
    fn remove(&self, path: &Path) -> io::Result<()>;
    /// The file names in `dir`, empty when it does not exist.
    ///
    /// # Errors
    /// Any read failure other than not-found.
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;
    /// `systemctl --user show -p MainPID --value <unit>`, trimmed. A read-only
    /// query.
    ///
    /// # Errors
    /// When `systemctl` cannot run or exits non-zero.
    fn main_pid(&self, unit: &str) -> io::Result<String>;
    /// `systemctl --user daemon-reload`.
    ///
    /// # Errors
    /// When `systemctl` cannot run or exits non-zero.
    fn daemon_reload(&self) -> io::Result<()>;
}

/// The real host: `std::fs` and `systemctl --user`.
pub struct RealHost;

impl DropinHost for RealHost {
    fn read(&self, path: &Path) -> io::Result<Option<String>> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write_atomic(&self, path: &Path, content: &str) -> io::Result<()> {
        crate::roll_pause::write_atomic(path, content.as_bytes())
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        match std::fs::read_dir(dir) {
            Ok(rd) => Ok(rd
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn main_pid(&self, unit: &str) -> io::Result<String> {
        let out = Command::new("systemctl")
            .args(["--user", "show", "-p", "MainPID", "--value", unit])
            .stdin(Stdio::null())
            .output()?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(io::Error::other(format!(
                "systemctl --user show -p MainPID exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }

    fn daemon_reload(&self) -> io::Result<()> {
        let out = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .stdin(Stdio::null())
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "systemctl --user daemon-reload exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }
}

/// What [`ensure_with`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not supervised by systemd on Linux (launchd, macOS, unsupervised): nothing written.
    NotSystemd,
    /// systemd's `MainPID` for the unit is not this process (a sweep child, a
    /// test daemon, a duplicate daemon), or the query failed: nothing written.
    NotMainProcess(String),
    /// The drop-in already has this content and no legacy copy exists: no
    /// write, no reload.
    Unchanged(PathBuf),
    /// Written (or a legacy copy removed) and reloaded.
    Written(PathBuf),
    /// The read or write failed. Non-fatal.
    WriteFailed(PathBuf, String),
    /// Written, but `daemon-reload` failed. Non-fatal; systemd picks the
    /// drop-in up at its next reload.
    ReloadFailed(PathBuf, String),
}

/// Write the drop-in when `supervisor` is systemd on Linux and its content
/// differs, remove a [`LEGACY_DROPIN_NAME`] copy, then reload. Acts only when
/// the unit's `MainPID` is `own_pid`. Never fails; see [`Outcome`].
pub fn ensure_with(
    supervisor: Option<&str>,
    is_linux: bool,
    own_pid: u32,
    unit_dir: &Path,
    unit: &str,
    host: &dyn DropinHost,
) -> Outcome {
    if !is_linux || supervisor != Some("systemd") {
        return Outcome::NotSystemd;
    }
    match host.main_pid(&service_name(unit)) {
        Ok(v) if own_pid != 0 && v.parse::<u32>().ok() == Some(own_pid) => {}
        Ok(v) => {
            return Outcome::NotMainProcess(format!(
                "the unit's MainPID is {:?}, this process is {own_pid}",
                v
            ))
        }
        Err(e) => return Outcome::NotMainProcess(e.to_string()),
    }
    let dir = dropin_dir(unit_dir, unit);
    let path = dir.join(DROPIN_NAME);
    let legacy = dir.join(LEGACY_DROPIN_NAME);
    let legacy_present = matches!(host.read(&legacy), Ok(Some(_)));
    let want = render_dropin();
    let current = match host.read(&path) {
        Ok(have) => have.as_deref() == Some(want.as_str()),
        Err(e) => return Outcome::WriteFailed(path, format!("read: {e}")),
    };
    if current && !legacy_present {
        return Outcome::Unchanged(path);
    }
    if !current {
        if let Err(e) = host.write_atomic(&path, &want) {
            return Outcome::WriteFailed(path, e.to_string());
        }
    }
    if legacy_present {
        // Ours sorts after the legacy copy and so wins either way; a failed
        // removal only leaves a redundant file behind.
        if let Err(e) = host.remove(&legacy) {
            log::warn!(
                "supervision drop-in: could not remove the legacy {} ({e}); {DROPIN_NAME} \
                 sorts after it and takes precedence",
                legacy.display()
            );
        }
    }
    match host.daemon_reload() {
        Ok(()) => Outcome::Written(path),
        Err(e) => Outcome::ReloadFailed(path, e.to_string()),
    }
}

/// The drop-ins in `dropin_dir` that sort after [`DROPIN_NAME`] and set one
/// of its keys, so systemd applies their value instead of ours.
#[must_use]
pub fn shadowing_dropins(dropin_dir: &Path, host: &dyn DropinHost) -> Vec<String> {
    let ours = render_dropin();
    let keys: Vec<&str> = ours
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    let mut names = host.list(dropin_dir).unwrap_or_default();
    names.retain(|n| n.ends_with(".conf") && n.as_str() > DROPIN_NAME);
    names.sort();
    names.retain(|n| {
        host.read(&dropin_dir.join(n))
            .ok()
            .flatten()
            .is_some_and(|text| {
                text.lines().any(|l| {
                    l.trim_start()
                        .split_once('=')
                        .is_some_and(|(k, _)| keys.contains(&k.trim()))
                })
            })
    });
    names
}

/// Log an [`Outcome`]: failures at WARN, a write at INFO, the rest at DEBUG.
pub fn log_outcome(outcome: &Outcome) {
    match outcome {
        Outcome::NotSystemd => {
            log::debug!("supervision drop-in: not supervised by systemd on Linux; skipped");
        }
        Outcome::NotMainProcess(why) => {
            log::debug!("supervision drop-in: not the unit's main process ({why}); skipped");
        }
        Outcome::Unchanged(p) => {
            log::debug!("supervision drop-in: {} is current", p.display());
        }
        Outcome::Written(p) => log::info!(
            "supervision drop-in: wrote {} and ran daemon-reload; the #11058 supervision \
             settings apply from this daemon's next exit",
            p.display()
        ),
        Outcome::WriteFailed(p, e) => log::warn!(
            "supervision drop-in: could not write {} ({e}); the unit keeps its old \
             supervision settings until `loom-daemon-update.sh --relaunch` re-renders it",
            p.display()
        ),
        Outcome::ReloadFailed(p, e) => log::warn!(
            "supervision drop-in: wrote {} but daemon-reload failed ({e}); systemd applies \
             it at its next reload",
            p.display()
        ),
    }
}

/// Production entry point, called once at daemon startup: the real
/// supervisor, platform, unit name and host.
pub fn ensure_on_startup() {
    let supervisor = crate::ipc::detect_supervisor();
    let outcome = ensure_with(
        supervisor.as_deref(),
        cfg!(target_os = "linux"),
        std::process::id(),
        &super::platform::systemd_unit_dir(),
        &super::platform::systemd_unit(),
        &RealHost,
    );
    log_outcome(&outcome);
    if let Outcome::Written(p) | Outcome::Unchanged(p) | Outcome::ReloadFailed(p, _) = &outcome {
        let dir = p.parent().unwrap_or_else(|| Path::new("."));
        let shadows = shadowing_dropins(dir, &RealHost);
        if !shadows.is_empty() {
            log::warn!(
                "supervision drop-in: {} in {} sort(s) after {DROPIN_NAME} and set(s) the same \
                 keys, so systemd applies those values instead of ours (for \
                 RestartPreventExitStatus= and SuccessExitStatus=, appends to our lists); \
                 remove those lines so the #11058 supervision applies",
                shadows.join(", "),
                dir.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// An in-memory host recording every write and reload. By default this
    /// process is the unit's main process.
    struct FakeHost {
        files: RefCell<HashMap<PathBuf, String>>,
        writes: RefCell<usize>,
        reloads: RefCell<usize>,
        fail_write: bool,
        fail_reload: bool,
        fail_remove: bool,
        /// What `main_pid` answers; `None` is a systemctl failure.
        main_pid: Option<String>,
    }

    /// The pid the tests' `ensure` calls itself.
    const OWN_PID: u32 = 4242;

    impl Default for FakeHost {
        fn default() -> Self {
            Self {
                files: RefCell::default(),
                writes: RefCell::default(),
                reloads: RefCell::default(),
                fail_write: false,
                fail_reload: false,
                fail_remove: false,
                main_pid: Some(OWN_PID.to_string()),
            }
        }
    }

    impl DropinHost for FakeHost {
        fn read(&self, path: &Path) -> io::Result<Option<String>> {
            Ok(self.files.borrow().get(path).cloned())
        }
        fn write_atomic(&self, path: &Path, content: &str) -> io::Result<()> {
            if self.fail_write {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only"));
            }
            *self.writes.borrow_mut() += 1;
            self.files
                .borrow_mut()
                .insert(path.to_path_buf(), content.to_string());
            Ok(())
        }
        fn remove(&self, path: &Path) -> io::Result<()> {
            if self.fail_remove {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only"));
            }
            self.files.borrow_mut().remove(path);
            Ok(())
        }
        fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
            Ok(self
                .files
                .borrow()
                .keys()
                .filter(|p| p.parent() == Some(dir))
                .filter_map(|p| p.file_name()?.to_str().map(ToString::to_string))
                .collect())
        }
        fn main_pid(&self, unit: &str) -> io::Result<String> {
            assert_eq!(unit, "loom-daemon.service", "queries the suffixed unit");
            self.main_pid
                .clone()
                .ok_or_else(|| io::Error::other("Failed to connect to bus"))
        }
        fn daemon_reload(&self) -> io::Result<()> {
            *self.reloads.borrow_mut() += 1;
            if self.fail_reload {
                return Err(io::Error::other("no user manager"));
            }
            Ok(())
        }
    }

    const UNIT_DIR: &str = "/h/.config/systemd/user";

    fn dropin_path() -> PathBuf {
        PathBuf::from("/h/.config/systemd/user/loom-daemon.service.d/zz-loom-supervision.conf")
    }

    fn legacy_path() -> PathBuf {
        PathBuf::from("/h/.config/systemd/user/loom-daemon.service.d/50-supervision.conf")
    }

    fn ensure(host: &FakeHost, sup: Option<&str>, linux: bool) -> Outcome {
        ensure_with(sup, linux, OWN_PID, Path::new(UNIT_DIR), "loom-daemon", host)
    }

    /// `key=` values in order, across the whole text.
    fn values<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
        let prefix = format!("{key}=");
        text.lines()
            .filter_map(|l| l.strip_prefix(prefix.as_str()))
            .collect()
    }

    fn directives(text: &str) -> Vec<String> {
        text.lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('['))
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn the_dropin_is_the_shared_block_with_list_resets() {
        let d = render_dropin();
        let (unit, service) = d.split_once("[Service]\n").expect("has [Service]");
        assert!(unit.contains("[Unit]\n"));
        // [Unit]: exactly the shared start limit.
        assert_eq!(directives(unit), directives(render::SYSTEMD_START_LIMIT));
        // [Service]: exactly the shared block's directives, plus one empty reset
        // directly before each list key.
        let mut want = Vec::new();
        for line in directives(&render::systemd_supervision_block()) {
            let key = line.split_once('=').expect("directive").0;
            if LIST_RESET_KEYS.contains(&key) {
                want.push(format!("{key}="));
            }
            want.push(line);
        }
        assert_eq!(directives(service), want);
    }

    #[test]
    fn the_dropin_carries_the_11058_settings() {
        let d = render_dropin();
        let unit = d.split("[Service]").next().expect("has [Unit]");
        assert_eq!(values(unit, "StartLimitIntervalSec"), ["600"]);
        assert_eq!(values(unit, "StartLimitBurst"), ["5"]);
        assert_eq!(values(&d, "Restart"), ["always"]);
        assert_eq!(values(&d, "RestartSec"), ["5"]);
        assert_eq!(values(&d, "OOMPolicy"), ["continue"]);
        assert_eq!(values(&d, "RestartPreventExitStatus"), ["", "1 79 130 143 SIGTERM SIGINT"]);
        assert_eq!(values(&d, "SuccessExitStatus"), ["", "143 130"]);
    }

    #[test]
    fn a_bare_unit_name_gets_the_service_suffix() {
        let u = Path::new(UNIT_DIR);
        assert_eq!(dropin_dir(u, "loom-daemon"), dropin_dir(u, "loom-daemon.service"));
        assert_eq!(
            dropin_dir(u, "scratch.service"),
            PathBuf::from("/h/.config/systemd/user/scratch.service.d")
        );
    }

    #[test]
    fn writes_and_reloads_when_absent() {
        let host = FakeHost::default();
        assert_eq!(ensure(&host, Some("systemd"), true), Outcome::Written(dropin_path()));
        assert_eq!(host.files.borrow().get(&dropin_path()), Some(&render_dropin()));
        assert_eq!((*host.writes.borrow(), *host.reloads.borrow()), (1, 1));
    }

    #[test]
    fn rewrites_only_when_the_content_differs() {
        let host = FakeHost::default();
        host.files
            .borrow_mut()
            .insert(dropin_path(), "[Service]\nRestart=on-success\n".to_string());
        assert_eq!(ensure(&host, Some("systemd"), true), Outcome::Written(dropin_path()));
        assert_eq!((*host.writes.borrow(), *host.reloads.borrow()), (1, 1));
        // A second startup finds it current: no write and no reload.
        assert_eq!(ensure(&host, Some("systemd"), true), Outcome::Unchanged(dropin_path()));
        assert_eq!((*host.writes.borrow(), *host.reloads.borrow()), (1, 1));
    }

    #[test]
    fn never_writes_off_systemd() {
        for (sup, linux) in [
            (Some("launchd"), false),
            (Some("launchd"), true),
            (None, true),
            (None, false),
            // A stray LOOM_DAEMON_SUPERVISOR=systemd on macOS.
            (Some("systemd"), false),
        ] {
            let host = FakeHost::default();
            assert_eq!(ensure(&host, sup, linux), Outcome::NotSystemd, "{sup:?} linux={linux}");
            assert_eq!((*host.writes.borrow(), *host.reloads.borrow()), (0, 0));
        }
    }

    #[test]
    fn writes_when_this_process_is_the_units_main_pid() {
        let host = FakeHost::default();
        assert_eq!(ensure(&host, Some("systemd"), true), Outcome::Written(dropin_path()));
        assert_eq!((*host.writes.borrow(), *host.reloads.borrow()), (1, 1));
    }

    #[test]
    fn a_main_pid_mismatch_writes_nothing_and_does_not_reload() {
        // A sweep child or a test daemon that inherited
        // LOOM_DAEMON_SUPERVISOR=systemd (#8077): the unit's MainPID is the
        // production daemon, not this process. Also an inactive unit (`0`) and
        // an empty answer.
        for answer in ["999", "0", "", "not-a-pid"] {
            let host = FakeHost {
                main_pid: Some(answer.to_string()),
                ..FakeHost::default()
            };
            host.files
                .borrow_mut()
                .insert(legacy_path(), "[Service]\nRestart=always\n".to_string());
            let out = ensure(&host, Some("systemd"), true);
            assert!(matches!(out, Outcome::NotMainProcess(_)), "{answer:?}: {out:?}");
            assert_eq!((*host.writes.borrow(), *host.reloads.borrow()), (0, 0), "{answer:?}");
            assert!(!host.files.borrow().contains_key(&dropin_path()));
            // Not even the legacy removal.
            assert!(host.files.borrow().contains_key(&legacy_path()));
            log_outcome(&out);
        }
        // A pid of 0 for this process never matches an inactive unit's `0`.
        let host = FakeHost {
            main_pid: Some("0".to_string()),
            ..FakeHost::default()
        };
        let out = ensure_with(Some("systemd"), true, 0, Path::new(UNIT_DIR), "loom-daemon", &host);
        assert!(matches!(out, Outcome::NotMainProcess(_)), "{out:?}");
    }

    #[test]
    fn a_systemctl_failure_writes_nothing() {
        let host = FakeHost {
            main_pid: None,
            ..FakeHost::default()
        };
        let out = ensure(&host, Some("systemd"), true);
        assert!(matches!(out, Outcome::NotMainProcess(_)), "{out:?}");
        assert_eq!((*host.writes.borrow(), *host.reloads.borrow()), (0, 0));
        assert!(host.files.borrow().is_empty());
    }

    #[test]
    fn a_write_failure_is_non_fatal_and_skips_the_reload() {
        let host = FakeHost {
            fail_write: true,
            ..FakeHost::default()
        };
        let out = ensure(&host, Some("systemd"), true);
        assert!(matches!(&out, Outcome::WriteFailed(p, _) if *p == dropin_path()), "{out:?}");
        assert_eq!(*host.reloads.borrow(), 0);
        log_outcome(&out);
    }

    #[test]
    fn a_reload_failure_is_non_fatal() {
        let host = FakeHost {
            fail_reload: true,
            ..FakeHost::default()
        };
        let out = ensure(&host, Some("systemd"), true);
        assert!(matches!(&out, Outcome::ReloadFailed(p, _) if *p == dropin_path()), "{out:?}");
        log_outcome(&out);
    }

    #[test]
    fn a_later_dropin_that_sets_our_keys_is_reported() {
        let host = FakeHost::default();
        let dir = dropin_dir(Path::new(UNIT_DIR), "loom-daemon.service");
        let put = |name: &str, text: &str| {
            host.files
                .borrow_mut()
                .insert(dir.join(name), text.to_string());
        };
        // Sorts after zz-loom-supervision.conf and sets Restart=: it wins.
        put("zzz-local.conf", "[Service]\nRestart=on-failure\n");
        // Sorts after, but sets none of our keys.
        put("zzz-env.conf", "[Service]\nEnvironment=FOO=1\n");
        // The pre-#11111 retrofit and `systemctl edit`'s file set Restart=, but
        // sort before ours, so ours wins.
        put("supervisor.conf", "[Service]\nRestart=on-success\n");
        put("override.conf", "[Service]\nRestart=no\n");
        // Not a drop-in: systemd only reads `*.conf`.
        put("zzz.conf.bak", "[Service]\nRestart=no\n");
        assert_eq!(shadowing_dropins(&dir, &host), ["zzz-local.conf"]);
        assert!(shadowing_dropins(Path::new("/nowhere"), &host).is_empty());
    }

    /// The value systemd ends up with for a single-valued `key` across the
    /// drop-ins in `dir`: files applied in file-name order, the last one wins.
    fn effective(host: &FakeHost, dir: &Path, key: &str) -> Option<String> {
        let mut names = host.list(dir).expect("list");
        names.retain(|n| n.ends_with(".conf"));
        names.sort();
        let mut value = None;
        for n in names {
            let text = host.read(&dir.join(n)).expect("read").expect("present");
            if let Some(v) = values(&text, key).last() {
                value = Some((*v).to_string());
            }
        }
        value
    }

    #[test]
    fn an_operator_supervisor_conf_does_not_override_the_dropin() {
        // The pre-#11111 retrofit hint wrote this file. Our name must sort after
        // it, and after the other usual operator names, so our settings win.
        for operator in [
            "supervisor.conf",
            "override.conf",
            "99-local.conf",
            "50-supervision.conf",
        ] {
            assert!(DROPIN_NAME > operator, "{DROPIN_NAME} must sort after {operator}");
        }
        let host = FakeHost::default();
        let dir = dropin_dir(Path::new(UNIT_DIR), "loom-daemon.service");
        host.files.borrow_mut().insert(
            dir.join("supervisor.conf"),
            "[Service]\nEnvironment=LOOM_DAEMON_SUPERVISOR=systemd\nRestart=on-success\n"
                .to_string(),
        );
        assert_eq!(ensure(&host, Some("systemd"), true), Outcome::Written(dropin_path()));
        assert_eq!(effective(&host, &dir, "Restart").as_deref(), Some("always"));
        assert_eq!(effective(&host, &dir, "OOMPolicy").as_deref(), Some("continue"));
        assert!(shadowing_dropins(&dir, &host).is_empty());
    }

    #[test]
    fn the_legacy_dropin_is_removed_when_writing() {
        let host = FakeHost::default();
        host.files
            .borrow_mut()
            .insert(legacy_path(), "[Service]\nRestart=always\n".to_string());
        assert_eq!(ensure(&host, Some("systemd"), true), Outcome::Written(dropin_path()));
        assert!(!host.files.borrow().contains_key(&legacy_path()));
        assert_eq!(host.files.borrow().get(&dropin_path()), Some(&render_dropin()));
        assert_eq!((*host.writes.borrow(), *host.reloads.borrow()), (1, 1));
        assert_eq!(ensure(&host, Some("systemd"), true), Outcome::Unchanged(dropin_path()));
    }

    #[test]
    fn a_legacy_copy_beside_a_current_dropin_is_removed_and_reloaded() {
        let host = FakeHost::default();
        host.files
            .borrow_mut()
            .insert(dropin_path(), render_dropin());
        host.files
            .borrow_mut()
            .insert(legacy_path(), "[Service]\nRestart=always\n".to_string());
        assert_eq!(ensure(&host, Some("systemd"), true), Outcome::Written(dropin_path()));
        assert!(!host.files.borrow().contains_key(&legacy_path()));
        // Nothing to rewrite, but the directory changed, so systemd reloads.
        assert_eq!((*host.writes.borrow(), *host.reloads.borrow()), (0, 1));
    }

    #[test]
    fn a_failed_legacy_removal_is_non_fatal() {
        let host = FakeHost {
            fail_remove: true,
            ..FakeHost::default()
        };
        host.files
            .borrow_mut()
            .insert(legacy_path(), "[Service]\nRestart=always\n".to_string());
        assert_eq!(ensure(&host, Some("systemd"), true), Outcome::Written(dropin_path()));
        assert_eq!(*host.reloads.borrow(), 1);
    }

    #[test]
    fn the_real_host_writes_atomically_and_reads_back() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = dropin_dir(tmp.path(), "loom-daemon").join(DROPIN_NAME);
        assert_eq!(RealHost.read(&path).expect("read"), None);
        RealHost.write_atomic(&path, "x\n").expect("write");
        assert_eq!(RealHost.read(&path).expect("read").as_deref(), Some("x\n"));
        let dir = path.parent().expect("dir");
        assert_eq!(RealHost.list(dir).expect("list"), [DROPIN_NAME]);
        RealHost.remove(&path).expect("remove");
        assert_eq!(RealHost.read(&path).expect("read"), None);
        RealHost
            .remove(&path)
            .expect("removing an absent file is not an error");
        assert!(RealHost
            .list(&tmp.path().join("absent"))
            .expect("list")
            .is_empty());
    }
}
