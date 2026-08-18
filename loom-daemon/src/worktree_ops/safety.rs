//! Worktree removal safety checks.
//!
//! Rust port of `loom_tools.common.worktree_safety` (the process-detection
//! piece `clean.py` needs — `find_processes_using_directory`) plus the small
//! git-status helpers `clean.py` inlines (`check_uncommitted_changes`). This
//! is the load-bearing safety layer the epic body calls out: "same tier as
//! the `.loom-managed` sentinel" — a worktree with an active process must
//! never be torn down even when every other gate says "stale".
//!
//! `common/worktree_safety.py` + its test file are deleted by this issue
//! (`clean.py` was its sole importer); this module is the Rust replacement
//! for the one function `clean.py` actually used.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A live process whose **executable image** lives inside some directory — the
/// evidence [`find_processes_executing_within`] returns (issue #6127).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveExecutable {
    /// The process id.
    pub pid: u32,
    /// The executable path exactly as the OS reports it. On Linux a program
    /// whose backing file has already been unlinked reports with a trailing
    /// ` (deleted)`; that suffix is kept verbatim, because it is precisely the
    /// evidence an operator needs to see.
    pub exe: PathBuf,
}

impl std::fmt::Display for LiveExecutable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pid {} → {}", self.pid, self.exe.display())
    }
}

/// Find live processes whose executable lives inside `directory` (at any
/// depth) — the gate that stops a build-artifact sweep from unlinking the
/// backing file of a **running** program (issue #6127).
///
/// # Why a process-table scan rather than a supervisor query
///
/// The alternatives considered were (a) asking the service manager
/// (`launchctl print` / `systemctl show -p ExecStart`), and (b) an opt-in
/// exclusion list a repo declares in `.loom/config.json`. This picks the
/// process table because it is the only option that is *evidence-based*: it
/// reports what is executing right now, needs no per-repo configuration to be
/// correct on a host nobody has configured, and covers programs started
/// outside any supervisor (a `nohup`'d binary, a tmux-hosted daemon) that a
/// supervisor query would miss entirely. Supervisor enumeration is also
/// per-platform, per-manager, and answers a subtly different question (what is
/// *registered*, not what is *running*).
///
/// # Its one blind spot, stated plainly
///
/// A service that is **stopped** at the moment the sweep runs is not detected —
/// its `program` path is deleted and the next start fails, which is the same
/// failure this guard exists to prevent, just reached from a different
/// direction. Nothing in a process table can see that; only a supervisor query
/// or a declared exclusion could. The operator-side rule therefore still
/// stands, and both the CLI help and the docs say so: **do not point a
/// launchd/systemd unit at a path under a build-output directory.**
///
/// # Platform behavior
///
/// - **Linux**: reads every `/proc/<pid>/exe` symlink. No subprocess, and the
///   kernel's answer is authoritative (it is the same link whose `(deleted)`
///   marker confirmed the incident in #6127).
/// - **macOS/BSD/other**: shells out to `ps -A -o pid=,comm=`, where `comm` is
///   the executable path; entries that are not absolute paths (kernel threads,
///   platforms whose `comm` is a bare name) are ignored.
///
/// Detection failures (no `/proc`, missing `ps`, a process owned by another
/// user whose `exe` link is unreadable) degrade to "not found" rather than an
/// error — consistent with [`find_processes_using_directory`] above. That is a
/// fail-*open* direction for a safety check, and deliberately so: this is one
/// layer of defense, and a probe that cannot run is not evidence of danger. It
/// does mean detection is effectively scoped to processes the caller can see,
/// which on a Loom host is the same user that owns the repo.
#[must_use]
pub fn find_processes_executing_within(directory: &Path) -> Vec<LiveExecutable> {
    // Match against both the literal and the resolved path: `/proc/<pid>/exe`
    // is fully resolved, while a `ps` `comm` (and the caller's `repo_root`) may
    // still contain symlinks — `/var` → `/private/var` under a macOS tempdir,
    // for instance.
    let mut prefixes = vec![directory.to_path_buf()];
    if let Ok(canonical) = directory.canonicalize() {
        if canonical != prefixes[0] {
            prefixes.push(canonical);
        }
    }

    let mut running = running_executables_proc();
    if running.is_empty() {
        running = running_executables_ps();
    }
    running.retain(|live| prefixes.iter().any(|prefix| live.exe.starts_with(prefix)));
    running.sort_by_key(|live| live.pid);
    running.dedup_by_key(|live| live.pid);
    running
}

/// Every process whose `/proc/<pid>/exe` link is readable. Empty on non-Linux.
#[cfg(target_os = "linux")]
fn running_executables_proc() -> Vec<LiveExecutable> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut running = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // Unreadable (another user's process, or one that exited mid-scan) is
        // skipped, never treated as an error.
        if let Ok(exe) = std::fs::read_link(entry.path().join("exe")) {
            running.push(LiveExecutable { pid, exe });
        }
    }
    running
}

#[cfg(not(target_os = "linux"))]
fn running_executables_proc() -> Vec<LiveExecutable> {
    Vec::new()
}

/// Every process `ps` reports with an absolute executable path — the
/// macOS/BSD path, where `comm` is the program's full path.
fn running_executables_ps() -> Vec<LiveExecutable> {
    // `-w -w` disables column truncation, so a long path is never clipped into
    // a prefix that silently stops matching.
    let Ok(output) = Command::new("ps")
        .args(["-A", "-w", "-w", "-o", "pid=,comm="])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_ps_exe_line)
        .collect()
}

/// Parse one `ps -o pid=,comm=` line into a [`LiveExecutable`], keeping only
/// absolute executable paths. Split out from the `ps` call so the parsing is
/// unit-testable on every platform, including the Linux CI that never takes
/// the `ps` branch.
fn parse_ps_exe_line(line: &str) -> Option<LiveExecutable> {
    let (pid, exe) = line.trim_start().split_once(char::is_whitespace)?;
    let pid = pid.parse::<u32>().ok()?;
    let exe = exe.trim();
    // Linux `comm` is a bare 15-char process name, and macOS reports kernel
    // threads without a path; neither can be matched against a directory.
    if !exe.starts_with('/') {
        return None;
    }
    Some(LiveExecutable {
        pid,
        exe: PathBuf::from(exe),
    })
}

/// Find PIDs with their current working directory inside `directory`
/// (recursively — matches the Python `_find_processes_lsof` / `_find_processes_proc`
/// behavior of matching the directory itself or any descendant).
///
/// macOS/BSD: shells out to `lsof +D <dir> -F pt` and keeps only `cwd`-typed
/// entries. Linux: scans `/proc/*/cwd` symlinks. Any other platform falls
/// back to the `lsof` path. Detection failures (missing tool, permission
/// errors) degrade to an empty list rather than propagating an error — the
/// caller treats "unknown" the same as "no active processes", matching the
/// Python original's fail-open-to-empty behavior (the marker file + issue
/// state checks are the primary gates; this is defense in depth).
#[must_use]
pub fn find_processes_using_directory(directory: &Path) -> Vec<u32> {
    let directory = directory
        .canonicalize()
        .unwrap_or_else(|_| directory.to_path_buf());
    let mut pids = if cfg!(target_os = "linux") {
        find_processes_proc(&directory)
    } else {
        find_processes_lsof(&directory)
    };
    let current_pid = std::process::id();
    pids.retain(|p| *p != current_pid);
    pids
}

fn find_processes_lsof(directory: &Path) -> Vec<u32> {
    let output = match Command::new("lsof")
        .arg("+d")
        .arg(directory)
        .arg("-F")
        .arg("pt")
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut pids: Vec<u32> = Vec::new();
    let mut current_pid: Option<u32> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            current_pid = rest.parse().ok();
        } else if let Some(rest) = line.strip_prefix('t') {
            if rest == "cwd" {
                if let Some(pid) = current_pid {
                    if !pids.contains(&pid) {
                        pids.push(pid);
                    }
                }
            }
        }
    }
    pids
}

#[cfg(target_os = "linux")]
fn find_processes_proc(directory: &Path) -> Vec<u32> {
    let proc = Path::new("/proc");
    if !proc.is_dir() {
        return Vec::new();
    }
    let dir_str = directory.to_string_lossy().to_string();
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir(proc) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        let cwd_link = entry.path().join("cwd");
        if let Ok(cwd) = std::fs::read_link(&cwd_link) {
            let cwd_str = cwd.to_string_lossy();
            if cwd_str == dir_str || cwd_str.starts_with(&format!("{dir_str}/")) {
                pids.push(pid);
            }
        }
    }
    pids
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
fn find_processes_proc(_directory: &Path) -> Vec<u32> {
    Vec::new()
}

/// True if `worktree_path` has any uncommitted changes (staged or unstaged).
/// Mirrors `clean.py::check_uncommitted_changes`: a non-directory path is
/// treated as "no changes" (nothing to lose), and any git invocation failure
/// is treated the same way (fail toward "safe to proceed", matching the
/// Python original — the caller layers other gates on top of this one).
#[must_use]
pub fn check_uncommitted_changes(worktree_path: &Path) -> bool {
    if !worktree_path.is_dir() {
        return false;
    }
    let unstaged = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["diff", "--quiet"])
        .status();
    let staged = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["diff", "--cached", "--quiet"])
        .status();
    let unstaged_dirty = unstaged.map(|s| !s.success()).unwrap_or(false);
    let staged_dirty = staged.map(|s| !s.success()).unwrap_or(false);
    unstaged_dirty || staged_dirty
}

/// Untracked files Loom writes into a managed worktree itself. These are
/// bookkeeping, not user work, so they must never make a worktree look dirty
/// to [`has_untracked_files`] — most repos gitignore them (the `loom-managed`
/// block in `.gitignore`), but a repo that predates or has edited that block
/// would otherwise see every managed worktree as permanently unreclaimable.
const LOOM_OWN_UNTRACKED_FILES: [&str; 2] = [".loom-managed", ".loom-in-use"];

/// True if `worktree_path` contains untracked, non-gitignored files that are
/// not Loom's own sentinels (issue #5939).
///
/// [`check_uncommitted_changes`] deliberately only asks `git diff` /
/// `git diff --cached`, which are both blind to untracked files — and
/// `git worktree remove --force` deletes those. For an `issue-<N>` worktree
/// that gap is bounded by the closed-issue gate; for a `pr-<N>` worktree,
/// whose branch and contents come from outside Loom, it is not, so the PR path
/// layers this on top (see [`super::clean::classify_pr_worktree`]).
///
/// A `git` failure (including "not a git worktree at all", the #5177 orphaned
/// directory case the removal path exists to clean up) reports `false` — the
/// same fail-toward-proceed convention as [`check_uncommitted_changes`], and
/// the reason the sentinel/containment gates still bound that path.
#[must_use]
pub fn has_untracked_files(worktree_path: &Path) -> bool {
    if !worktree_path.is_dir() {
        return false;
    }
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .any(|l| !LOOM_OWN_UNTRACKED_FILES.contains(&l))
}

/// [`check_uncommitted_changes`] widened to also catch untracked files
/// ([`has_untracked_files`]) — the `uncommitted` probe the `pr-<N>` path wires
/// in (issue #5939).
///
/// Kept as a separate function rather than folded into
/// [`check_uncommitted_changes`] on purpose: the `issue-<N>` gate chain was
/// reviewed and shipped with the narrower definition, and widening it there is
/// a behavior change to a path this work does not touch.
#[must_use]
pub fn check_uncommitted_or_untracked_changes(worktree_path: &Path) -> bool {
    check_uncommitted_changes(worktree_path) || has_untracked_files(worktree_path)
}

/// Parsed `.loom-in-use` marker contents (best-effort; unknown/missing
/// fields render as `"unknown"`, matching `clean.py::clean_worktrees`'s
/// `marker_data.get(..., "unknown")` reads).
#[derive(Debug, Default, Clone)]
pub struct InUseMarker {
    pub task_id: String,
    pub pid: String,
}

/// Read a worktree's `.loom-in-use` marker file, if present.
#[must_use]
pub fn read_in_use_marker(worktree_path: &Path) -> Option<InUseMarker> {
    let marker_path = worktree_path.join(".loom-in-use");
    if !marker_path.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&marker_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let obj = value.as_object();
    Some(InUseMarker {
        task_id: obj
            .and_then(|o| o.get("shepherd_task_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        pid: obj
            .and_then(|o| o.get("pid"))
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "unknown".to_string()),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_directory_has_no_uncommitted_changes() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(!check_uncommitted_changes(&missing));
    }

    #[test]
    fn find_processes_excludes_current_pid() {
        // Whatever `find_processes_using_directory` reports for a scratch
        // dir, it must never include our own pid (we're "using" our own
        // cwd trivially, and the Python original explicitly filters it).
        let dir = tempdir().unwrap();
        let pids = find_processes_using_directory(dir.path());
        assert!(!pids.contains(&std::process::id()));
    }

    // ------------------------------------------------------------------
    // find_processes_executing_within (#6127) — the guard that keeps a
    // build-artifact sweep from unlinking a running program's binary.
    // ------------------------------------------------------------------

    /// Copy the host's `sleep` into `dir/<name>` and run it, so a live process
    /// is executing an image inside `dir`.
    ///
    /// The spawn is retried on `ETXTBSY`: a *concurrent* test thread forking
    /// while our write fd is briefly open leaves the child holding that fd, and
    /// Linux then refuses to exec the file. Nothing to do with the code under
    /// test — just the standard fork/exec race in a multi-threaded harness.
    fn spawn_from(dir: &Path, name: &str) -> std::process::Child {
        let source = ["/bin/sleep", "/usr/bin/sleep"]
            .iter()
            .map(Path::new)
            .find(|p| p.is_file())
            .expect("a `sleep` binary is needed to stand in for a service");
        let program = dir.join(name);
        std::fs::copy(source, &program).unwrap();
        // Re-sign the relocated copy on macOS: a plain `fs::copy` of a system binary
        // carries over the original embedded code signature (bound to the source
        // path's identity), so Gatekeeper SIGKILLs the exec'd copy asynchronously —
        // `Command::spawn()` still returns `Ok`, so the "live" process can already
        // be dead by the time this test asserts on it. Same mitigation this repo
        // already applies to its own compiled test binaries via
        // `.cargo/macos-test-runner.sh` (#2298). Test-only; not a production fix.
        // See #6430 / #6343.
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("codesign")
                .args(["-f", "-s", "-", program.to_str().unwrap()])
                .status()
                .expect("failed to ad-hoc codesign test binary");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut last_err = None;
        for _ in 0..100 {
            match Command::new(&program)
                .arg("300")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    // Let the kernel publish the exec'd image (/proc/<pid>/exe).
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    return child;
                }
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => panic!("stand-in service must spawn: {e}"),
            }
        }
        panic!("stand-in service never became executable: {last_err:?}");
    }

    #[test]
    fn a_binary_running_from_the_directory_is_reported() {
        let dir = tempdir().unwrap();
        let artifacts = dir.path().join("target/release");
        std::fs::create_dir_all(&artifacts).unwrap();
        let mut child = spawn_from(&artifacts, "loom-unit-service");

        // The whole artifact root must report the holder, not just the exact
        // directory the binary sits in — that is the granularity the sweep
        // deletes at.
        let found = find_processes_executing_within(&dir.path().join("target"));
        let deeper = find_processes_executing_within(&artifacts);

        let _ = child.kill();
        let _ = child.wait();

        assert!(
            found.iter().any(|live| live.pid == child.id()),
            "the spawned service must be detected under target/: {found:?}"
        );
        assert!(
            deeper.iter().any(|live| live.pid == child.id()),
            "and under the exact directory holding it: {deeper:?}"
        );
        assert!(found
            .iter()
            .all(|live| live.exe.to_string_lossy().contains("loom-unit-service")));
    }

    #[test]
    fn a_directory_with_nothing_executing_inside_it_is_unprotected() {
        let dir = tempdir().unwrap();
        let artifacts = dir.path().join("target");
        std::fs::create_dir_all(&artifacts).unwrap();
        // A binary that merely *exists* is not a running process.
        std::fs::write(artifacts.join("not-running"), "#!/bin/sh\n").unwrap();
        assert!(
            find_processes_executing_within(&artifacts).is_empty(),
            "the normal reclaim path must not be blocked by inert files"
        );
    }

    #[test]
    fn a_sibling_directory_sharing_a_name_prefix_is_not_a_match() {
        // Component-wise matching, not string-prefix: `/x/target-old` must not
        // count as being inside `/x/target`.
        let dir = tempdir().unwrap();
        let decoy = dir.path().join("target-old");
        std::fs::create_dir_all(&decoy).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        let mut child = spawn_from(&decoy, "loom-unit-decoy");

        let found = find_processes_executing_within(&dir.path().join("target"));

        let _ = child.kill();
        let _ = child.wait();

        assert!(
            found.is_empty(),
            "a name-prefix sibling must not protect the real artifact dir: {found:?}"
        );
    }

    #[test]
    fn ps_lines_parse_into_absolute_executables_only() {
        let parsed = parse_ps_exe_line("  1234 /Users/x/repo/target/release/safehoused").unwrap();
        assert_eq!(parsed.pid, 1234);
        assert_eq!(parsed.exe, PathBuf::from("/Users/x/repo/target/release/safehoused"));
        // Linux `comm` (a bare name) and kernel threads carry no path to match.
        assert!(parse_ps_exe_line("  1234 safehoused").is_none());
        assert!(parse_ps_exe_line("").is_none());
        assert!(parse_ps_exe_line("not-a-pid /bin/sh").is_none());
    }

    #[test]
    fn a_deleted_backing_file_still_reports_its_directory() {
        // Linux appends " (deleted)" to /proc/<pid>/exe once the file is
        // unlinked — exactly the state #6127's repro host was found in. The
        // suffix must not defeat containment matching.
        let live = LiveExecutable {
            pid: 7,
            exe: PathBuf::from("/home/u/GitHub/safehouse/target/release/safehoused (deleted)"),
        };
        assert!(live
            .exe
            .starts_with(Path::new("/home/u/GitHub/safehouse/target")));
        assert!(live.to_string().contains("pid 7"));
    }

    #[test]
    fn reads_in_use_marker_with_defaults() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".loom-in-use"), r#"{"pid": 123}"#).unwrap();
        let marker = read_in_use_marker(dir.path()).unwrap();
        assert_eq!(marker.pid, "123");
        assert_eq!(marker.task_id, "unknown");
    }

    #[test]
    fn missing_marker_is_none() {
        let dir = tempdir().unwrap();
        assert!(read_in_use_marker(dir.path()).is_none());
    }

    #[test]
    fn corrupt_marker_falls_back_to_unknown_defaults() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".loom-in-use"), "not json").unwrap();
        let marker = read_in_use_marker(dir.path()).unwrap();
        assert_eq!(marker.pid, "unknown");
        assert_eq!(marker.task_id, "unknown");
    }

    // ------------------------------------------------------------------
    // Untracked-file gate (#5939 review): `git diff` is blind to untracked
    // files, and `git worktree remove --force` deletes them.
    // ------------------------------------------------------------------

    fn init_repo_with_commit(dir: &Path) {
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            assert!(Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(dir.join("tracked.txt"), "x").unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["commit", "-q", "-m", "init"])
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn a_clean_worktree_has_no_untracked_files() {
        let dir = tempdir().unwrap();
        init_repo_with_commit(dir.path());
        assert!(!has_untracked_files(dir.path()));
        assert!(!check_uncommitted_or_untracked_changes(dir.path()));
    }

    #[test]
    fn an_untracked_file_is_work_that_would_be_lost() {
        let dir = tempdir().unwrap();
        init_repo_with_commit(dir.path());
        std::fs::write(dir.path().join("scratch-notes.md"), "unsaved").unwrap();
        assert!(has_untracked_files(dir.path()));
        // The narrower legacy probe cannot see it — the exact gap this closes.
        assert!(!check_uncommitted_changes(dir.path()));
        assert!(check_uncommitted_or_untracked_changes(dir.path()));
    }

    #[test]
    fn looms_own_sentinels_are_not_untracked_user_work() {
        let dir = tempdir().unwrap();
        init_repo_with_commit(dir.path());
        std::fs::write(dir.path().join(".loom-managed"), "").unwrap();
        std::fs::write(dir.path().join(".loom-in-use"), "{}").unwrap();
        assert!(
            !has_untracked_files(dir.path()),
            "every managed worktree carries these; counting them would make the pr-<N> \
             reaper a permanent no-op in a repo without the loom .gitignore block"
        );
    }

    #[test]
    fn gitignored_build_artifacts_are_not_untracked_user_work() {
        let dir = tempdir().unwrap();
        init_repo_with_commit(dir.path());
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/big.bin"), "0").unwrap();
        // `.gitignore` itself is untracked here, so commit it before asserting.
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", ".gitignore"])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-q", "-m", "ignore"])
            .status()
            .unwrap()
            .success());
        assert!(!has_untracked_files(dir.path()));
    }

    #[test]
    fn a_non_git_directory_reports_no_untracked_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("loose.txt"), "x").unwrap();
        assert!(
            !has_untracked_files(dir.path()),
            "the #5177 orphaned-directory path must stay reclaimable"
        );
    }
}
