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

use super::{env, report};

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

/// Decide, from the resolved `LOOM_WATCHDOG_ESCALATE` knob and the sentinel.
///
/// Thin wrapper reading the knob from the process env; the pure decision lives
/// in [`decide_with_knob`] so tests can pass the knob explicitly instead of
/// inheriting whatever a live daemon on this host exported (#8328).
#[must_use]
pub fn decide(sentinel: &Path) -> Decision {
    decide_with_knob(env::tri("LOOM_WATCHDOG_ESCALATE"), sentinel)
}

/// The pure decision, from the already-resolved knob and the sentinel.
///
/// `escalate` is [`env::tri`]'s tri-state: `Some(false)` — `0`/`false`/`no`,
/// the shell's exact negative spelling — switches escalation off; `Some(true)`
/// and `None` (unset, empty, or an unrecognised value, matching the shell's
/// fall-through) leave it on.
#[must_use]
pub fn decide_with_knob(escalate: Option<bool>, sentinel: &Path) -> Decision {
    if escalate == Some(false) {
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
         outage does not sit unnoticed in a logfile.\n\n\
         - **Host**: `{host}`\n\
         - **Socket**: `{socket}`\n\
         - **Intent marker**: `{marker}` (present — a daemon IS expected here)\n\
         - **Observed**: {detail}\n\
         - **Why recovery stopped**: {reason}\n\
         - **Recovery command**: `{recovery}`\n\
         - **Watchdog log**: `{log}`\n\
         - **Episode state**: `{state}`\n\n\
         **To recover by hand**: run `./.loom/scripts/cli/loom-daemon-start.sh [flags]` on that\n\
         host and inspect `loom-daemon status`. The watchdog resumes automatic recovery (with a\n\
         fresh attempt budget) as soon as any tick observes a healthy daemon; deleting\n\
         `{state}` resets the circuit breaker immediately.\n\n\
         Filed automatically by the loom-daemon-watchdog.sh outage escalation (#5391). Deduped by a\n\
         sentinel at `{sentinel}`, which is cleared automatically once the daemon is\n\
         healthy again.",
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
///
/// #8649: shares the same failure-reporting fix as
/// [`super::peer_coord::write_sentinel`] — a failed write here used to vanish
/// via `let _ =`, so a still-open outage would refile every tick for as long
/// as the write kept failing (e.g. a full disk). This now reports the failure
/// loudly instead of discarding it.
///
/// FAILS OPEN, same as before and for the same reason as its peer-coordination
/// sibling: the outage issue itself was already filed by the caller (a network
/// call, independent of this write), so blocking or retrying here would only
/// trade "occasionally duplicated" for "occasionally silent".
pub fn write_sentinel(sentinel: &Path, reporter: &report::Reporter) {
    if let Some(dir) = sentinel.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            reporter.report(
                report::Level::Warn,
                &format!(
                    "failed to create the parent directory for the outage-escalation sentinel \
                     {} ({e}) — the sentinel write below will fail too, so a later tick may \
                     re-escalate this same outage.",
                    sentinel.display()
                ),
            );
        }
    }
    if let Err(e) =
        std::fs::write(sentinel, format!("{}\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")))
    {
        reporter.report(
            report::Level::Warn,
            &format!(
                "failed to write the outage-escalation sentinel {} ({e}) — the forge issue was \
                 filed, but a later tick cannot see this sentinel and may re-escalate the same \
                 outage (#8649).",
                sentinel.display()
            ),
        );
    }
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
pub fn file_issue(
    script: &Path,
    ctx: &Context,
    sentinel: &Path,
    reporter: &report::Reporter,
) -> bool {
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
        write_sentinel(sentinel, reporter);
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
    fn a_failed_sentinel_write_is_reported_not_silently_eaten() {
        // #8649: the write used to vanish via `let _ =`. Point the sentinel
        // at a path whose PARENT is a plain file — `create_dir_all` and the
        // write both fail deterministically, no read-only filesystem needed
        // — and assert the failure lands in the watchdog log rather than
        // disappearing.
        let d = tempfile::tempdir().expect("tempdir");
        let blocker = d.path().join("not-a-directory");
        std::fs::write(&blocker, "x").expect("write blocker file");
        let sentinel = blocker.join("sentinel");

        let log = d.path().join("watchdog.log");
        let reporter = report::Reporter::new(log.clone(), false);
        write_sentinel(&sentinel, &reporter);

        assert!(
            !sentinel.exists(),
            "the write must actually have failed for this test to prove anything"
        );
        let logged = std::fs::read_to_string(&log).expect("reporter must still have logged");
        assert!(logged.contains("[WARN]"), "failure must be reported, not swallowed: {logged}");
        assert!(
            logged.contains("outage-escalation sentinel"),
            "the report should name what failed: {logged}"
        );
    }

    #[test]
    fn an_existing_sentinel_suppresses_a_duplicate() {
        let d = tempfile::tempdir().expect("tempdir");
        let s = d.path().join("sentinel");
        // The knob is passed explicitly: this host may export
        // `LOOM_WATCHDOG_ESCALATE=0` (a live daemon's environment leaks into
        // every spawned test, #8328), and the sentinel-suppression claim must
        // not depend on it.
        assert_eq!(decide_with_knob(None, &s), Decision::Escalate);
        std::fs::write(&s, "x").expect("write");
        assert_eq!(decide_with_knob(None, &s), Decision::AlreadyEscalated);
    }

    #[test]
    fn a_disabled_knob_short_circuits_before_the_sentinel_is_consulted() {
        // The regression half of #8328: the Disabled verdict must survive, so
        // it is asserted here explicitly rather than only ever inherited from
        // a hostile host environment.
        let d = tempfile::tempdir().expect("tempdir");
        let s = d.path().join("absent-sentinel");
        assert_eq!(decide_with_knob(Some(false), &s), Decision::Disabled);
        std::fs::write(&s, "x").expect("write");
        assert_eq!(decide_with_knob(Some(false), &s), Decision::Disabled);
    }

    #[test]
    fn an_explicitly_enabled_or_unrecognised_knob_leaves_escalation_on() {
        // `Some(true)` and `None` (unset/empty/unrecognised — env::tri's
        // fall-through) both behave identically to the wrapper's unset case.
        let d = tempfile::tempdir().expect("tempdir");
        let s = d.path().join("sentinel");
        for knob in [Some(true), None] {
            assert_eq!(decide_with_knob(knob, &s), Decision::Escalate);
        }
        std::fs::write(&s, "x").expect("write");
        for knob in [Some(true), None] {
            assert_eq!(decide_with_knob(knob, &s), Decision::AlreadyEscalated);
        }
    }

    /// Save/restore guard for the knob a live daemon on this host may have
    /// exported into this very process (#8328) — the `ClearedGhConfigDirEnv`
    /// pattern from `role_runner::tests`: pin the exact hostile value, put
    /// back whatever was there afterward.
    struct PinnedEscalateKnob(Option<String>);

    impl PinnedEscalateKnob {
        fn new(value: &str) -> Self {
            let prior = std::env::var("LOOM_WATCHDOG_ESCALATE").ok();
            std::env::set_var("LOOM_WATCHDOG_ESCALATE", value);
            Self(prior)
        }
    }

    impl Drop for PinnedEscalateKnob {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("LOOM_WATCHDOG_ESCALATE", v),
                None => std::env::remove_var("LOOM_WATCHDOG_ESCALATE"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn a_poisoned_escalate_knob_cannot_leak_into_the_pure_decision() {
        // The #8328 regression test proper. A live-daemon host exports
        // `LOOM_WATCHDOG_ESCALATE=0` into every spawned test, which is exactly
        // how `an_existing_sentinel_suppresses_a_duplicate` used to go red.
        // This test pins that hostile value in the REAL process env and then
        // asserts both halves of the fix:
        //   * the pure decision ignores the env entirely (hermeticity — this
        //     is the assertion that fails if the suite ever regresses to
        //     calling the env-reading `decide` from a test), and
        //   * the env-reading wrapper still honours the knob (no behaviour
        //     was weakened to buy the isolation).
        let _env = PinnedEscalateKnob::new("0");
        let d = tempfile::tempdir().expect("tempdir");
        let s = d.path().join("sentinel");
        assert_eq!(decide_with_knob(None, &s), Decision::Escalate);
        std::fs::write(&s, "x").expect("write");
        assert_eq!(decide_with_knob(None, &s), Decision::AlreadyEscalated);
        // The wrapper is the one place the knob is SUPPOSED to be read: with
        // the hostile value pinned, `decide` must still short-circuit to
        // `Disabled` before the (now existing) sentinel is ever consulted.
        assert_eq!(decide(&s), Decision::Disabled);
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

#[cfg(test)]
mod shell_differential {
    use super::*;

    /// The #5391 outage-escalation body, rendered by the RETIRED shell at
    /// `143fe332^:defaults/scripts/cli/loom-daemon-watchdog.sh` for the
    /// fixture below — captured by running its own heredoc under bash, not
    /// retyped.
    ///
    /// Successor proof for the `#7508` static scans
    /// (`defaults/docs/verification-recipes.md` §6): those checked the body
    /// was BUILT safely; this checks it IS the same body, which subsumes them.
    ///
    /// It earned that immediately. The port had silently dropped all three
    /// blank lines between paragraphs, so in Markdown the bullet list would
    /// have rendered glued to the paragraph above it — in an issue filed
    /// unattended, during an outage, that nobody reviews before it is posted.
    /// Every behavioural assertion in the retained suite passed throughout.
    const SHELL_RENDERED_BODY: &str = "`loom-daemon-watchdog.sh` has been unable to restore the loom-daemon on host\n\
         `build-01`. Autonomous dispatch is DOWN and the watchdog bounded-recovery loop\n\
         has stopped trying — this issue is the escalation of last resort (#5391), filed so the\n\
         outage does not sit unnoticed in a logfile.\n\
         \n\
         - **Host**: `build-01`\n\
         - **Socket**: `/home/u/.loom/loom-daemon.sock`\n\
         - **Intent marker**: `/home/u/.loom/autonomy-desired` (present — a daemon IS expected here)\n\
         - **Observed**: launchd job gui/501/com.x is not loaded/alive\n\
         - **Why recovery stopped**: the circuit breaker is OPEN\n\
         - **Recovery command**: `/home/u/.loom/scripts/cli/loom-daemon-start.sh`\n\
         - **Watchdog log**: `/home/u/.loom/logs/daemon-watchdog.log`\n\
         - **Episode state**: `/home/u/.loom/.watchdog-recovery-state`\n\
         \n\
         **To recover by hand**: run `./.loom/scripts/cli/loom-daemon-start.sh [flags]` on that\n\
         host and inspect `loom-daemon status`. The watchdog resumes automatic recovery (with a\n\
         fresh attempt budget) as soon as any tick observes a healthy daemon; deleting\n\
         `/home/u/.loom/.watchdog-recovery-state` resets the circuit breaker immediately.\n\
         \n\
         Filed automatically by the loom-daemon-watchdog.sh outage escalation (#5391). Deduped by a\n\
         sentinel at `/home/u/.loom/.watchdog-outage-escalated`, which is cleared automatically once the daemon is\n\
         healthy again.";

    #[test]
    fn the_outage_issue_body_is_byte_identical_to_the_shell_it_replaced() {
        let ctx = Context {
            hostname: "build-01",
            socket_path: Path::new("/home/u/.loom/loom-daemon.sock"),
            marker: Path::new("/home/u/.loom/autonomy-desired"),
            liveness_detail: "launchd job gui/501/com.x is not loaded/alive",
            reason: "the circuit breaker is OPEN",
            recovery_argv_detail: Some("/home/u/.loom/scripts/cli/loom-daemon-start.sh"),
            watchdog_log: Path::new("/home/u/.loom/logs/daemon-watchdog.log"),
            recovery_state: Path::new("/home/u/.loom/.watchdog-recovery-state"),
            sentinel: Path::new("/home/u/.loom/.watchdog-outage-escalated"),
        };
        let ours = body(&ctx);
        assert_eq!(
            ours, SHELL_RENDERED_BODY,
            "the ported outage body diverged from the shell's. If deliberate, say so and \
             re-capture the constant; if not, it is a port defect in text filed unattended \
             during an outage."
        );
        assert_eq!(ours.len(), 1314, "byte count is part of the claim");
        // 22, not the 21 `wc -l` reports: this body deliberately ends WITHOUT
        // a trailing newline (the shell's `$(...)` strips it), and `wc -l`
        // counts newlines rather than lines.
        assert_eq!(ours.lines().count(), 22, "line count is part of the claim");
    }
}
