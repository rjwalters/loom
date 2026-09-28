//! The final "installed" line (AC4) and the idle-shutdown cron-guard notice
//! (#4697) that rides along with it.
//!
//! THE INCIDENT THE NOTICE EXISTS FOR: a remote worker was updated through
//! this script — rebuild and supervised restart both succeeded — and ~15
//! minutes later the host powered itself off. Nothing in the update flow
//! warned that the "successful" update was landing on a host about to
//! evaporate: the STAGE-2 cron guard `fleet add-worker
//! --idle-shutdown-minutes` installs fired once the freshly-relaunched, idle
//! daemon crossed its window and powered the WHOLE HOST off — SSH, tailnet,
//! everything.
//!
//! It is purely advisory: it never disables or touches the guard, never
//! changes the exit code, and is silent when no guard is installed. The
//! guard's own design (#3998/#4477) is correct and out of scope — the gap
//! this closes is operator awareness at the moment a "successful" update is
//! reported.
//!
//! Every successful / "already up to date" exit path funnels through
//! [`print_final_installed_line`], which is why the notice fires from there
//! once rather than being duplicated at each of the script's several exits.

use std::process::{Command, Stdio};

use super::out;
use super::util;

/// What the final line needs to know about this run.
pub struct FinalLine<'a> {
    pub artifact_mode: bool,
    pub artifact_tag: &'a str,
    pub artifact_version: &'a str,
    pub artifact_target: &'a str,
    pub default_branch: &'a str,
    pub origin_commit: &'a str,
}

/// `print_final_installed_line <commit>`.
pub fn print_final_installed_line(ctx: &FinalLine<'_>, commit: &str) {
    // Artifact-fetch mode (#5020): the installed binary is a RELEASE build, so
    // comparing its commit against origin/<default-branch>'s tip is the wrong
    // currency claim — a released commit is normally BEHIND the branch tip and
    // saying "does NOT match" about it would be actively misleading.
    if ctx.artifact_mode {
        let commit_clause = if commit.is_empty() {
            String::new()
        } else {
            format!(", commit {commit}")
        };
        out::say(&format!(
            "Installed: release {} ({}{commit_clause}) for target {} — fetched artifact, checksum verified",
            ctx.artifact_tag, ctx.artifact_version, ctx.artifact_target
        ));
        idle_shutdown_notice();
        return;
    }
    if ctx.default_branch.is_empty() || ctx.origin_commit == "unknown" {
        out::say(&format!(
            "Installed: {commit} (currency vs origin/<default-branch> unknown — unresolvable or unreachable)"
        ));
    } else if commit == ctx.origin_commit {
        out::say(&format!("Installed: {commit} (matches origin/{})", ctx.default_branch));
    } else {
        out::say(&format!(
            "Installed: {commit} (origin/{} is at {} — does NOT match; built from a checkout that was behind or diverged, e.g. --allow-stale)",
            ctx.default_branch, ctx.origin_commit
        ));
    }
    idle_shutdown_notice();
}

/// `idle_shutdown_notice` — silent unless this host carries the cron guard.
pub fn idle_shutdown_notice() {
    if util::env_truthy("LOOM_SKIP_IDLE_SHUTDOWN_NOTICE") {
        return;
    }
    if !util::have("crontab") {
        return;
    }
    let crontab = Command::new("crontab")
        .arg("-l")
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    if !crontab.contains("loom-idle-shutdown") {
        return;
    }

    let guard = util::home().join(".local/bin/loom-idle-shutdown.sh");
    // `grep -oE 'LIMIT=[0-9]+' | head -n1 | cut -d= -f2` — the first match's
    // digits, or empty when the file is unreadable or holds no such line.
    let minutes = std::fs::read_to_string(&guard)
        .ok()
        .and_then(|text| first_limit(&text))
        .unwrap_or_default();

    if minutes.is_empty() {
        out::warn(&format!(
            "Heads up: this host has an idle-shutdown cron guard installed (crontab holds a loom-idle-shutdown entry, but the configured window could not be read from {}) — it WILL power the whole host off after some idle window. This is expected/by-design (#3998/#4477), not a fault in this update. See daemon-reference.md, 'fleet add-worker' step 9 (idle-shutdown), for the wake path.",
            guard.display()
        ));
    } else {
        out::warn(&format!(
            "Heads up: this host has an idle-shutdown cron guard installed (fleet add-worker --idle-shutdown-minutes {minutes}) — after ~{minutes} idle minute(s) it POWERS THE WHOLE HOST OFF (SSH/tailnet included), not just this daemon. This is expected/by-design (#3998/#4477), not a fault in this update. Wake path (provider console/CLI restart; Loom never calls a cloud CLI itself) and tailnet-identity/re-registration notes: daemon-reference.md, 'fleet add-worker' step 9 (idle-shutdown)."
        ));
    }
}

/// The digits of the first `LIMIT=<digits>` occurrence anywhere in `text`.
fn first_limit(text: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = text[from..].find("LIMIT=") {
        let start = from + rel + "LIMIT=".len();
        let digits: String = text[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return Some(digits);
        }
        from = start;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_scan_takes_the_first_match_with_digits() {
        assert_eq!(first_limit("LIMIT=45\nLIMIT=90\n").as_deref(), Some("45"));
        assert_eq!(first_limit("# LIMIT=\nLIMIT=12").as_deref(), Some("12"));
        assert_eq!(first_limit("no limit here"), None);
        // `-oE 'LIMIT=[0-9]+'` needs at least one digit, so a bare `LIMIT=`
        // is not a match and the scan continues past it.
        assert_eq!(first_limit("LIMIT="), None);
    }
}
