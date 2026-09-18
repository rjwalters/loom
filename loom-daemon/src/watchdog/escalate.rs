//! Escalation of last resort (#5391): file a forge issue when bounded recovery
//! is exhausted.
//!
//! The 2026-07-26 incident was discovered hours late because the only signal was
//! a logfile nobody tails. A confirmed outage that has spent its whole attempt
//! budget is not a log line; it is an operator's problem, and this puts it where
//! operators look.
//!
//! Deduplicated by a sentinel file, which is cleared by
//! [`super::recovery::clear`] the moment any tick observes a healthy daemon. The
//! pairing is what keeps this from filing one issue per tick forever, and also
//! what lets the NEXT outage escalate — a sentinel left behind after recovery
//! would silence every future outage on the host.

use std::path::{Path, PathBuf};

use super::env;

/// Whether escalation may be attempted this tick.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// File it.
    Escalate,
    /// Switched off on this host.
    Disabled,
    /// Already escalated; the sentinel is still in place.
    AlreadyEscalated,
}

/// Decide, from the knob and the sentinel.
#[must_use]
pub fn decide(sentinel: &Path) -> Decision {
    if env::var("LOOM_WATCHDOG_ESCALATE").is_some_and(|v| env::is_false(&v)) {
        return Decision::Disabled;
    }
    if sentinel.exists() {
        return Decision::AlreadyEscalated;
    }
    Decision::Escalate
}

/// Locate `create-issue.sh`, in the shell's order.
///
/// The repo's own copy wins, because filing must go through
/// `create-issue.sh` rather than a bare `gh issue create`: that path is
/// GraphQL-backed and dies on GraphQL exhaustion while the independent REST
/// pool sits idle (#5047). An outage escalation that fails because the forge's
/// GraphQL quota is spent is the worst possible time to discover that.
#[must_use]
pub fn resolve_issue_script(
    repo_root: Option<&Path>,
    cli_dir: &Path,
    fallback_override: Option<&Path>,
) -> Option<PathBuf> {
    let executable = |p: &Path| -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            p.is_file()
        }
    };

    if let Some(root) = repo_root {
        for rel in [
            ".loom/scripts/create-issue.sh",
            "defaults/scripts/create-issue.sh",
        ] {
            let c = root.join(rel);
            if executable(&c) {
                return Some(c);
            }
        }
    }
    let fallback = fallback_override.map_or_else(|| cli_dir.join(".."), Path::to_path_buf);
    let c = fallback.join("create-issue.sh");
    executable(&c).then_some(c)
}

/// Everything the issue body reports.
pub struct Context<'a> {
    pub hostname: &'a str,
    pub socket_path: &'a Path,
    pub marker: &'a Path,
    pub liveness_detail: &'a str,
    pub reason: &'a str,
    pub recovery_argv_detail: Option<&'a str>,
    pub watchdog_log: &'a Path,
    pub recovery_state: &'a Path,
    pub sentinel: &'a Path,
}

/// The issue title.
#[must_use]
pub fn title(hostname: &str) -> String {
    format!("loom-daemon is DOWN on {hostname} and watchdog recovery is exhausted")
}

/// The issue body.
///
/// In the shell this was a heredoc with every backtick escaped, because an
/// unescaped one would have been command-substituted into the body of an issue
/// filed automatically at 3am. Here it is a string literal and that entire bug
/// class is gone — which is a small, concrete example of what the port buys
/// beyond line count.
#[must_use]
pub fn body(ctx: &Context) -> String {
    format!(
        "`loom-daemon-watchdog.sh` has been unable to restore the loom-daemon on host\n\
         `{host}`. Autonomous dispatch is DOWN and the watchdog bounded-recovery loop\n\
         has stopped trying — this issue is the escalation of last resort (#5391), filed so the\n\
         outage does not sit unnoticed in a logfile.\n\
         - **Host**: `{host}`\n\
         - **Socket**: `{socket}`\n\
         - **Intent marker**: `{marker}` (present — a daemon IS expected here)\n\
         - **Observed**: {detail}\n\
         - **Why recovery stopped**: {reason}\n\
         - **Recovery command**: `{recovery}`\n\
         - **Watchdog log**: `{log}`\n\
         - **Episode state**: `{state}`\n\
         **To recover by hand**: run `./.loom/scripts/cli/loom-daemon-start.sh [flags]` on that\n\
         host and inspect `loom-daemon status`. The watchdog resumes automatic recovery (with a\n\
         fresh attempt budget) as soon as any tick observes a healthy daemon; deleting\n\
         `{state}` resets the circuit breaker immediately.\n\
         Filed automatically by the loom-daemon-watchdog.sh outage escalation (#5391). Deduped by a\n\
         sentinel at `{sentinel}`, which is cleared automatically once the daemon is\n\
         healthy again.\n",
        host = ctx.hostname,
        socket = ctx.socket_path.display(),
        marker = ctx.marker.display(),
        detail = ctx.liveness_detail,
        reason = ctx.reason,
        recovery = ctx.recovery_argv_detail.unwrap_or("<none resolvable>"),
        log = ctx.watchdog_log.display(),
        state = ctx.recovery_state.display(),
        sentinel = ctx.sentinel.display(),
    )
}

/// Record that this outage has been escalated, so later ticks do not refile.
pub fn write_sentinel(sentinel: &Path) {
    if let Some(dir) = sentinel.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ =
        std::fs::write(sentinel, format!("{}\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")));
}

/// This host's name for the issue title and body.
///
/// Reads `/proc/sys/kernel/hostname` first and falls back to the `hostname`
/// command, matching [`crate::script_helpers`]'s existing resolution rather
/// than adding a third way to answer the same question. The shell's final
/// fallback was `unknown-host`; that is kept, because an issue titled for
/// `localhost` in a fleet is worse than one that admits it does not know.
#[must_use]
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown-host".to_string())
}

/// File the issue. `true` only when `create-issue.sh` reported success.
///
/// The sentinel is written **only** on a confirmed file, never optimistically.
/// A sentinel written for an issue that was not filed suppresses every future
/// escalation on this host until someone deletes it by hand — the outage would
/// be invisible in both places at once.
///
/// `--force` is passed because the duplicate-detection in `create-issue.sh`
/// compares against open issues by similarity, and a recurring outage on the
/// same host is *supposed* to file again once the sentinel has been cleared by
/// a recovery. The sentinel is this path's dedup, not the forge's.
#[must_use]
pub fn file_issue(script: &Path, ctx: &Context, sentinel: &Path) -> bool {
    let mut cmd = std::process::Command::new(script);
    cmd.arg("--title")
        .arg(title(ctx.hostname))
        .arg("--body")
        .arg(body(ctx))
        .arg("--label")
        .arg("loom:triage")
        .arg("--force");

    let filed = crate::sweep_registry::output_with_timeout(cmd, std::time::Duration::from_secs(60))
        .ok()
        .flatten()
        .is_some_and(|o| o.status.success());

    if filed {
        write_sentinel(sentinel);
    }
    filed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(recovery: Option<&'a str>) -> Context<'a> {
        Context {
            hostname: "build-01",
            socket_path: Path::new("/home/u/.loom/loom-daemon.sock"),
            marker: Path::new("/home/u/.loom/autonomy-desired"),
            liveness_detail: "launchd job gui/501/com.x is not loaded/alive",
            reason: "the circuit breaker is OPEN",
            recovery_argv_detail: recovery,
            watchdog_log: Path::new("/home/u/.loom/logs/daemon-watchdog.log"),
            recovery_state: Path::new("/home/u/.loom/.watchdog-recovery-state"),
            sentinel: Path::new("/home/u/.loom/.watchdog-outage-escalated"),
        }
    }

    #[test]
    fn an_existing_sentinel_suppresses_a_duplicate() {
        let d = tempfile::tempdir().expect("tempdir");
        let s = d.path().join("sentinel");
        assert_eq!(decide(&s), Decision::Escalate);
        std::fs::write(&s, "x").expect("write");
        assert_eq!(decide(&s), Decision::AlreadyEscalated);
    }

    #[test]
    fn the_body_names_every_path_an_operator_needs() {
        let b = body(&ctx(Some("bash /x/loom-daemon-start.sh --from-config")));
        for needle in [
            "build-01",
            "loom-daemon.sock",
            "autonomy-desired",
            "the circuit breaker is OPEN",
            "bash /x/loom-daemon-start.sh --from-config",
            "daemon-watchdog.log",
            ".watchdog-recovery-state",
            ".watchdog-outage-escalated",
        ] {
            assert!(b.contains(needle), "body is missing {needle:?}:\n{b}");
        }
    }

    #[test]
    fn the_body_says_so_when_no_recovery_command_was_resolvable() {
        // "<none resolvable>" is more useful than an empty backtick pair: it
        // tells the operator the watchdog could not even try, which is a
        // different problem from trying and failing.
        let b = body(&ctx(None));
        assert!(b.contains("<none resolvable>"), "{b}");
    }

    #[test]
    fn the_body_tells_the_operator_how_to_reset_the_breaker() {
        // Without this the issue reports an outage nobody can clear except by
        // guessing, and the breaker stays open after they fix the daemon.
        let b = body(&ctx(None));
        assert!(b.contains("resets the circuit breaker immediately"), "{b}");
        assert!(b.contains("as soon as any tick observes a healthy daemon"), "{b}");
    }

    #[test]
    fn the_title_names_the_host_because_a_fleet_files_many() {
        assert!(title("build-01").contains("build-01"));
    }

    #[test]
    fn the_repo_copy_of_create_issue_wins_over_the_fallback() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("repo");
        let loom = root.join(".loom/scripts");
        std::fs::create_dir_all(&loom).expect("mkdir");
        let script = loom.join("create-issue.sh");
        std::fs::write(&script, "#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let got = resolve_issue_script(Some(&root), Path::new("/nope"), None);
        assert_eq!(got.as_deref(), Some(script.as_path()));
    }

    #[test]
    fn a_non_executable_create_issue_is_not_accepted() {
        // The shell guarded with `-x`. A readable-but-not-executable file must
        // fall through rather than resolve to something that cannot run.
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("repo");
        let loom = root.join(".loom/scripts");
        std::fs::create_dir_all(&loom).expect("mkdir");
        std::fs::write(loom.join("create-issue.sh"), "#!/bin/sh\n").expect("write");
        assert_eq!(resolve_issue_script(Some(&root), Path::new("/nope"), None), None);
    }

    #[test]
    fn no_resolvable_script_yields_none_rather_than_a_guess() {
        assert_eq!(resolve_issue_script(None, Path::new("/nope"), None), None);
    }
}
