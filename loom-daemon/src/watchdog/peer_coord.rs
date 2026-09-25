//! The 50-series: peer-claim coordination health (#6222/#7258/#7664).
//!
//! A fleet coordinates claims by advertising them to peers and receiving
//! theirs. When the receive path degrades, each host still thinks it is
//! coordinating while actually working alone — so two hosts can claim the same
//! issue and neither notices. That failure is silent by construction, which is
//! why the watchdog asks about it explicitly rather than waiting for a symptom.
//!
//! Only checked on a tick whose IPC probe came back healthy. Asking a daemon
//! that is not answering produces no information, and a "could not determine"
//! reported alongside a confirmed hang is noise on top of a real signal.

use std::path::Path;

use super::report;

/// The daemon's own verdict about its coordination path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Green,
    Degraded,
}

/// What the daemon reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub verdict: Verdict,
    pub summary: String,
    /// How long the receive path has been degraded, when the daemon said.
    pub degraded_for_secs: Option<u64>,
    /// Sustained receives accumulated toward recovery.
    pub consecutive: Option<u64>,
    /// How many are needed.
    pub recovery_threshold: Option<u64>,
}

/// Parse `loom-daemon peer-claims --json`.
///
/// `None` means **could not determine**, and it is distinct from `Green` in a
/// way that matters: an absent or unexpected shape must not be read as healthy.
/// The shell is explicit about this — anything other than a literal `true` or
/// `false` for `.coordination.degraded` returns failure, and the caller then
/// says nothing rather than reporting health it did not observe.
#[must_use]
pub fn parse(json: &str) -> Option<Health> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let coordination = v.get("coordination")?;

    // Strictly a bool. A string "true", a number, or null is the shape being
    // wrong, and a wrong shape is unknown rather than healthy.
    let degraded = coordination.get("degraded")?.as_bool()?;

    let num = |k: &str| -> Option<u64> { coordination.get(k).and_then(serde_json::Value::as_u64) };
    let top = |k: &str| -> Option<u64> { v.get(k).and_then(serde_json::Value::as_u64) };

    let received = top("received");
    let advertised = top("advertised");
    let show = |o: Option<u64>| o.map_or_else(|| "0".to_string(), |n| n.to_string());
    let show_q = |o: Option<u64>| o.map_or_else(|| "?".to_string(), |n| n.to_string());

    let summary = if degraded {
        format!(
            "peer-claim receive path DEGRADED ({} received / {} advertised), degraded for {}s — \
             {}/{} sustained receive(s) toward recovery",
            show(received),
            show(advertised),
            show_q(num("degraded_for_secs")),
            show(num("consecutive_receives_toward_recovery")),
            show_q(num("recovery_threshold")),
        )
    } else {
        format!(
            "peer-claim receive path healthy ({} received / {} advertised)",
            show(received),
            show(advertised)
        )
    };

    Some(Health {
        verdict: if degraded {
            Verdict::Degraded
        } else {
            Verdict::Green
        },
        summary,
        degraded_for_secs: num("degraded_for_secs"),
        consecutive: num("consecutive_receives_toward_recovery"),
        recovery_threshold: num("recovery_threshold"),
    })
}

/// Whether the check is enabled on this host.
#[must_use]
pub fn enabled() -> bool {
    !super::env::var("LOOM_WATCHDOG_PEER_COORD_CHECK").is_some_and(|v| super::env::is_false(&v))
}

/// The cooldown after a recovery, during which a repeat degradation does not
/// file a fresh issue (#7258).
///
/// Coordination flaps: a path that recovers and re-degrades within minutes is
/// one episode to an operator, not two, and filing per flap buries the signal
/// under duplicates.
#[must_use]
pub fn cooldown_secs() -> u64 {
    super::env::num("LOOM_WATCHDOG_PEER_COORD_COOLDOWN_SECS", 21600)
}

/// Seconds remaining in the cooldown, or `None` when it has expired or no
/// recovery is on record.
#[must_use]
pub fn cooldown_remaining(state: &std::path::Path, now: u64, window: u64) -> Option<u64> {
    let text = std::fs::read_to_string(state).ok()?;
    let recovered_at: u64 = text.split_whitespace().next()?.parse().ok()?;
    // The shell's guard is `elapsed >= 0 && elapsed < COOLDOWN`, and the `>= 0`
    // half is load-bearing: a record stamped in the FUTURE (clock skew, an NTP
    // step, a restored snapshot) makes elapsed negative, the condition false,
    // and the shell FILES FRESH. Its own comment says so — "a record whose
    // stored timestamp is in the future, e.g. clock skew, FAILS OPEN".
    //
    // `saturating_sub` silently inverted that: it clamps to 0, which reads as
    // "just recovered" and suppresses for the whole window. On a host whose
    // clock jumped forward, a real coordination outage would go unreported for
    // six hours. Fail-open is the deliberate choice here — a duplicate issue
    // is noise, a missing one is an outage nobody hears about.
    if recovered_at > now {
        return None;
    }
    let elapsed = now - recovered_at;
    (elapsed < window).then(|| window - elapsed)
}

/// The cooldown state file's three fields: when the last episode recovered,
/// which issue tracked it, and how many times it has flapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cooldown {
    pub recovered_at: u64,
    pub issue_ref: String,
    pub flap_count: u64,
}

/// Read the cooldown record, or `None` when there is no usable one.
#[must_use]
pub fn read_cooldown(state: &std::path::Path) -> Option<Cooldown> {
    let text = std::fs::read_to_string(state).ok()?;
    let mut it = text.split_whitespace();
    let recovered_at = it.next()?.parse().ok()?;
    let issue_ref = it.next()?.to_string();
    // A missing or malformed count reads as 1, not 0: this record only exists
    // because an episode happened, so "no flaps yet" is one.
    let flap_count = it.next().and_then(|v| v.parse().ok()).unwrap_or(1);
    Some(Cooldown {
        recovered_at,
        issue_ref,
        flap_count,
    })
}

/// The #7664 dedup window: how long after a recovery a repeat degradation is
/// treated as the SAME episode flapping rather than a new one.
///
/// A day by default, and much longer than the #7258 cooldown on purpose. The
/// cooldown asks "is this too soon to be worth reporting at all"; this asks "is
/// this the same problem coming back". A path that degrades, recovers and
/// re-degrades three times in an afternoon is one operator problem, and three
/// issues bury it under itself.
#[must_use]
pub fn dedup_window_secs() -> u64 {
    super::env::num("LOOM_WATCHDOG_PEER_COORD_DEDUP_WINDOW_SECS", 86_400)
}

/// What to do about a repeat degradation.
#[derive(Debug, PartialEq, Eq)]
pub enum Repeat {
    /// Comment on the existing issue and reopen it, as flap number `flap`.
    CommentOn { issue_ref: String, flap: u64 },
    /// Outside the window, or no issue on record — file fresh.
    FileFresh,
}

/// Decide, given the cooldown record and the current time.
#[must_use]
pub fn repeat_action(cooldown: Option<&Cooldown>, now: u64, window: u64) -> Repeat {
    let Some(c) = cooldown else {
        return Repeat::FileFresh;
    };
    if c.issue_ref.is_empty() {
        return Repeat::FileFresh;
    }
    // Same clock-skew rule as `cooldown_remaining`: a record stamped in the
    // FUTURE is not evidence of a recent episode, so it cannot hold the dedup
    // window open either. `saturating_sub` would clamp to 0 and keep commenting
    // on a stale issue instead of filing the fresh one the operator needs.
    if c.recovered_at > now || now - c.recovered_at >= window {
        return Repeat::FileFresh;
    }
    Repeat::CommentOn {
        issue_ref: c.issue_ref.clone(),
        flap: c.flap_count + 1,
    }
}

/// The comment left on an existing tracking issue for a repeat flap.
#[must_use]
pub fn flap_comment(hostname: &str, summary: &str, flap: u64, window: u64) -> String {
    format!(
        "peer-claim coordination has gone DEGRADED again on `{hostname}` ({summary}).\n\n\
         This is flap #{flap} since this tracking issue was first filed, landing within the \
         {window}s dedup window (#7664) since the last recovery — commenting here instead of \
         filing a fresh issue.\n\n\
         **Suspected cause** (unverified, per anvil#1270): `advertised` only moves at dispatch \
         time, so a RAM/disk-throttled host with cap 0 never advertises and cannot reach the \
         sustained-receive recovery threshold.\n\n\
         Filed automatically by the loom-daemon-watchdog.sh peer-coordination escalation \
         (#6222, dedup by #7664).\n"
    )
}

/// Seconds since the epoch. Mirrors `mod.rs`'s helper rather than making that
/// one public, so the cooldown stamp and the cooldown read agree on a clock.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The escalation issue's title (#6222).
#[must_use]
pub fn title(hostname: &str) -> String {
    format!("peer-claim coordination is DEGRADED on {hostname} (#6157 Layer 3)")
}

/// The escalation issue's body.
///
/// In the shell this was built with `read -d ''` rather than `$(cat <<EOF)`,
/// because wrapping a heredoc in command substitution trips a real bash 3.2
/// lexer bug: a bare apostrophe in the prose (here, "host's") was misread as
/// opening a quote region it could never close, and the escalation body was
/// silently dropped — 1099 times from 2026-08-16 (#7508). Here it is a string
/// literal, so the entire bug class is gone rather than worked around, and the
/// prose no longer has to avoid apostrophes to stay correct.
#[must_use]
pub fn body(hostname: &str, health: &Health, watchdog_log: &Path, sentinel: &Path) -> String {
    let q = |o: Option<u64>| o.map_or_else(|| "?".to_string(), |n| n.to_string());
    format!(
        "The `peer_coordination` section of `loom-daemon health` has gone DEGRADED on host\n\
         `{hostname}`. This host's one-way peer-claim RECEIVE path (Safehouse, #6157)\n\
         can no longer be trusted to prove another host has already claimed an issue.\n\
         This is diagnostic only since Epic #6165 Phase 4 (#6317): it no longer freezes\n\
         stale-claim reclamation, which now gates solely on the lease record (#6286)\n\
         (see `.loom/docs/safehouse.md` -> \"Peer-claim coordination: cross-host soft\n\
         claim (#4028)\").\n\n\
         - **Host**: `{hostname}`\n\
         - **Verdict**: {summary}\n\
         - **Degraded for**: {degraded_for}s\n\
         - **Recovery progress**: {consecutive}/{threshold} consecutive sustained receive(s) \
         toward recovery\n\
         - **Watchdog log**: `{log}`\n\n\
         **To recover by hand**: run `loom-daemon peer-claims` (or `loom-daemon health`)\n\
         on `{hostname}` to confirm the live `peer_coordination` state, and check\n\
         Safehouse connectivity to peer hosts (`.loom/docs/safehouse.md`).\n\n\
         **This alert clears itself** — no manual close needed. Filed automatically by\n\
         the loom-daemon-watchdog.sh peer-coordination escalation (#6222, Layer 3 of\n\
         #6157). Deduped by a sentinel at `{sentinel}`, which is cleared\n\
         automatically (and this issue commented on + closed) once a later watchdog\n\
         tick observes `peer_coordination` back to healthy.\n",
        summary = health.summary,
        degraded_for = health
            .degraded_for_secs
            .map_or_else(|| "unknown".to_string(), |n| n.to_string()),
        consecutive = health.consecutive.unwrap_or(0),
        threshold = q(health.recovery_threshold),
        log = watchdog_log.display(),
        sentinel = sentinel.display(),
    )
}

/// File the tracking issue. Returns the issue reference `create-issue.sh`
/// printed, which the sentinel records so recovery can close that exact issue.
///
/// `--force` because the sentinel already dedupes this path.
pub fn file(
    script: &Path,
    hostname: &str,
    health: &Health,
    watchdog_log: &Path,
    sentinel: &Path,
) -> Option<String> {
    let mut cmd = std::process::Command::new(script);
    cmd.arg("--title")
        .arg(title(hostname))
        .arg("--body")
        .arg(body(hostname, health, watchdog_log, sentinel))
        .arg("--label")
        .arg("loom:triage")
        .arg("--force");

    let out = crate::sweep_registry::output_with_timeout(cmd, std::time::Duration::from_secs(60))
        .ok()
        .flatten()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if url.is_empty() {
        // The shell requires BOTH rc 0 and a non-empty url. A sentinel with no
        // issue reference is worse than none: recovery would have nothing to
        // close and would keep retrying against a filing that may not exist.
        return None;
    }
    Some(url)
}

/// Record the filing so a later tick dedupes against it and recovery can close
/// the exact issue: `<iso8601> <issue-ref>`, the shell's format.
///
/// #8649: a failed write here used to vanish via `let _ =` — the forge issue
/// still got filed (that's a network call, not a disk write), but the
/// sentinel that suppresses the NEXT tick's escalation never landed, so a
/// still-ongoing degradation looked brand new every tick and refiled
/// (#8646/#8647/#8648, one continuous episode filed three times under disk
/// pressure). This now REPORTS the failure loudly via the same `Reporter`
/// every other divergence in this watchdog uses, so an operator sees it in
/// the log instead of only inferring it from duplicate issues.
///
/// Deliberately FAILS OPEN, same as before: the caller still treats the
/// escalation as filed and moves on, rather than retrying or blocking on a
/// write that may keep failing for as long as the underlying disk pressure
/// does. A blocked escalation would trade "occasionally duplicated" for
/// "occasionally silent", which is worse — the forge issue is the operator
/// signal that matters, and it already landed. Fixing the disk pressure
/// itself is out of scope (see the issue's "Suspected Cause").
pub fn write_sentinel(sentinel: &Path, issue_ref: &str, reporter: &report::Reporter) {
    if let Some(dir) = sentinel.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            reporter.report(
                report::Level::Warn,
                &format!(
                    "failed to create the parent directory for the peer-coordination sentinel \
                     {} ({e}) — the sentinel write below will fail too, so the NEXT tick will \
                     not see it and may re-escalate a still-ongoing degradation.",
                    sentinel.display()
                ),
            );
        }
    }
    if let Err(e) = std::fs::write(
        sentinel,
        format!("{} {issue_ref}\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")),
    ) {
        reporter.report(
            report::Level::Warn,
            &format!(
                "failed to write the peer-coordination sentinel {} for {issue_ref} ({e}) — the \
                 forge issue was filed, but the NEXT tick cannot see this one and may treat the \
                 still-ongoing degradation as new (#8649).",
                sentinel.display()
            ),
        );
    }
}

/// The issue reference a previous escalation recorded, if any.
#[must_use]
pub fn sentinel_issue_ref(sentinel: &Path) -> Option<String> {
    let text = std::fs::read_to_string(sentinel).ok()?;
    let mut it = text.split_whitespace();
    let _ts = it.next()?;
    it.next().map(str::to_string)
}

/// #7664: a REPEAT degradation inside the dedup window comments on the
/// existing tracking issue and reopens it, instead of filing a fresh one.
///
/// Returns the operator-facing note on success. `None` means "fall through and
/// file fresh" — the shell's `return 1`, which every failure path takes
/// (no `gh`, no prior issue reference, the comment call failing). Reopen
/// failure is deliberately NOT one of them: the comment is the record, and a
/// closed-but-commented issue is better than a duplicate.
///
/// The cooldown row is rewritten with the ORIGINAL `recovered_at`, not now —
/// the dedup window measures from the last recovery, so refreshing it here
/// would let an indefinitely flapping host never escape the window.
pub fn dedup_comment(
    cooldown: &Cooldown,
    sentinel: &Path,
    cooldown_state: &Path,
    hostname: &str,
    summary: &str,
    flap: u64,
    reporter: &report::Reporter,
) -> Option<String> {
    let window = dedup_window_secs();
    let gh = std::time::Duration::from_secs(60);

    let mut comment = std::process::Command::new("gh");
    comment
        .args(["issue", "comment", &cooldown.issue_ref, "--body"])
        .arg(flap_comment(hostname, summary, flap, window));
    let commented = crate::sweep_registry::output_with_timeout(comment, gh)
        .ok()
        .flatten()
        .is_some_and(|o| o.status.success());
    if !commented {
        return None;
    }

    let mut reopen = std::process::Command::new("gh");
    reopen.args(["issue", "reopen", &cooldown.issue_ref]);
    let _ = crate::sweep_registry::output_with_timeout(reopen, gh);

    write_sentinel(sentinel, &cooldown.issue_ref, reporter);
    // #8649: same fail-open-but-loud treatment as `write_sentinel` above — a
    // failed cooldown-state write means the NEXT flap's `flap_count` resets to
    // 1 instead of incrementing, which is a worse comment, not a worse
    // decision (the sentinel above is what suppresses re-filing).
    if let Some(dir) = cooldown_state.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            reporter.report(
                report::Level::Warn,
                &format!(
                    "failed to create the parent directory for the peer-coordination cooldown \
                     state {} ({e}) — the write below will fail too.",
                    cooldown_state.display()
                ),
            );
        }
    }
    if let Err(e) = std::fs::write(
        cooldown_state,
        format!("{} {} {flap}\n", cooldown.recovered_at, cooldown.issue_ref),
    ) {
        reporter.report(
            report::Level::Warn,
            &format!(
                "failed to write the peer-coordination cooldown state {} ({e}) — the flap count \
                 for this episode may under-report on the next repeat (#8649).",
                cooldown_state.display()
            ),
        );
    }

    Some(format!(
        "Repeat flap #{flap} within the {window}s dedup window (#7664) — commented on the \
         existing tracking issue {} instead of filing a new one.",
        cooldown.issue_ref
    ))
}

/// The recovery counterpart: comment on and close the exact issue that was
/// filed, clear the sentinel, and stamp the cooldown.
///
/// Best-effort throughout. A missing `gh`, no forge auth, or a failed close
/// leaves the sentinel in place so a LATER healthy tick simply retries, rather
/// than silently losing track of an open tracking issue.
///
/// Returns `true` only when the issue was actually closed.
pub fn recover(sentinel: &Path, cooldown_state: &Path, hostname: &str, summary: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(sentinel) else {
        return false;
    };
    let issue_ref = {
        let mut it = text.split_whitespace();
        let _ts = it.next();
        it.next().map(str::to_string)
    };
    let Some(issue_ref) = issue_ref.filter(|r| !r.is_empty()) else {
        // A malformed or legacy sentinel with no recorded reference: there is
        // nothing to close, so clear it rather than retrying forever.
        let _ = std::fs::remove_file(sentinel);
        return true;
    };

    let gh = std::time::Duration::from_secs(60);
    let mut comment = std::process::Command::new("gh");
    comment
        .args(["issue", "comment", &issue_ref, "--body"])
        .arg(format!(
            // Verbatim from the shell (loom-daemon-watchdog.sh:1820). This is
            // posted to the forge, so a stray run of spaces and a dropped `.sh`
            // are both visible to an operator reading the issue.
            "peer-claim coordination has RECOVERED on `{hostname}` ({summary}). Closing \
         automatically — filed by the loom-daemon-watchdog.sh peer-coordination escalation \
         (#6222)."
        ));
    // The comment is advisory: a failure must not stop the close.
    let _ = crate::sweep_registry::output_with_timeout(comment, gh);

    let mut close = std::process::Command::new("gh");
    close.args(["issue", "close", &issue_ref, "--reason", "completed"]);
    let closed = crate::sweep_registry::output_with_timeout(close, gh)
        .ok()
        .flatten()
        .is_some_and(|o| o.status.success());
    if !closed {
        return false;
    }

    let _ = std::fs::remove_file(sentinel);

    // #7664: carry this issue's running flap count forward, so a dedup-comment
    // escalation's bumped count survives recovery instead of resetting to 1 on
    // every episode.
    let flap = read_cooldown(cooldown_state)
        .filter(|c| c.issue_ref == issue_ref)
        .map_or(1, |c| c.flap_count);
    if let Some(dir) = cooldown_state.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(cooldown_state, format!("{} {issue_ref} {flap}\n", now_secs()));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cd(recovered_at: u64, issue: &str, flap: u64) -> Cooldown {
        Cooldown {
            recovered_at,
            issue_ref: issue.to_string(),
            flap_count: flap,
        }
    }

    #[test]
    fn a_failed_sentinel_write_is_reported_not_silently_eaten() {
        // #8649: mirrors the `escalate.rs` test of the same name. The
        // production trigger was a full disk, but a parent path that is a
        // plain file reproduces the same `write` failure deterministically —
        // both `create_dir_all` and the write itself fail, and the point is
        // that the failure is REPORTED rather than dropped by `let _ =`.
        let d = tempfile::tempdir().expect("tempdir");
        let blocker = d.path().join("not-a-directory");
        std::fs::write(&blocker, "x").expect("write blocker file");
        let sentinel = blocker.join("sentinel");

        let log = d.path().join("watchdog.log");
        let reporter = report::Reporter::new(log.clone(), false);
        write_sentinel(&sentinel, "1234", &reporter);

        assert!(
            !sentinel.exists(),
            "the write must actually have failed for this test to prove anything"
        );
        let logged = std::fs::read_to_string(&log).expect("reporter must still have logged");
        assert!(logged.contains("[WARN]"), "failure must be reported, not swallowed: {logged}");
        assert!(
            logged.contains("peer-coordination sentinel"),
            "the report should name what failed: {logged}"
        );
    }

    #[test]
    fn a_repeat_inside_the_window_comments_rather_than_files() {
        let c = cd(1000, "1234", 1);
        assert_eq!(
            repeat_action(Some(&c), 1500, 86_400),
            Repeat::CommentOn {
                issue_ref: "1234".into(),
                flap: 2
            }
        );
    }

    #[test]
    fn the_flap_count_increments_across_episodes() {
        let c = cd(1000, "1234", 4);
        assert_eq!(
            repeat_action(Some(&c), 1001, 86_400),
            Repeat::CommentOn {
                issue_ref: "1234".into(),
                flap: 5
            }
        );
    }

    #[test]
    fn outside_the_window_it_files_fresh() {
        assert_eq!(
            repeat_action(Some(&cd(1000, "1234", 1)), 1000 + 86_400, 86_400),
            Repeat::FileFresh
        );
    }

    #[test]
    fn no_issue_on_record_means_there_is_nothing_to_comment_on() {
        // Commenting needs somewhere to comment: an empty ref must never
        // become `gh issue comment ""`.
        assert_eq!(repeat_action(Some(&cd(1000, "", 1)), 1001, 86_400), Repeat::FileFresh);
        assert_eq!(repeat_action(None, 1001, 86_400), Repeat::FileFresh);
    }

    #[test]
    fn a_record_stamped_in_the_future_files_fresh_rather_than_commenting() {
        // Renamed and inverted, for the same reason as its twin in
        // `cooldown_remaining`. The old version argued that clamping avoided
        // "filing a duplicate" — but a record stamped in the FUTURE is not
        // evidence of a recent episode at all, and treating it as one keeps
        // commenting on a stale issue instead of filing the fresh one an
        // operator needs. The shell files fresh here.
        assert_eq!(repeat_action(Some(&cd(5000, "1234", 1)), 1000, 86_400), Repeat::FileFresh);
        // The ordinary in-window case is unchanged.
        assert_eq!(
            repeat_action(Some(&cd(1000, "1234", 1)), 5000, 86_400),
            Repeat::CommentOn {
                issue_ref: "1234".into(),
                flap: 2
            }
        );
    }

    #[test]
    fn a_cooldown_record_round_trips_and_defaults_its_count_to_one() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("cd");
        std::fs::write(&p, "1700000000 4242 3\n").expect("write");
        assert_eq!(read_cooldown(&p), Some(cd(1_700_000_000, "4242", 3)));
        std::fs::write(&p, "1700000000 4242\n").expect("write");
        assert_eq!(read_cooldown(&p).map(|c| c.flap_count), Some(1));
    }

    #[test]
    fn a_corrupt_cooldown_record_is_no_record() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("cd");
        std::fs::write(&p, "notanumber 4242 1\n").expect("write");
        assert_eq!(read_cooldown(&p), None);
        std::fs::write(&p, "\n").expect("write");
        assert_eq!(read_cooldown(&p), None);
    }

    #[test]
    fn the_flap_comment_names_the_flap_number_and_the_window() {
        let c = flap_comment("build-01", "3 received / 9 advertised", 4, 86_400);
        for needle in [
            "flap #4",
            "86400s dedup window",
            "build-01",
            "3 received / 9 advertised",
        ] {
            assert!(c.contains(needle), "missing {needle:?}: {c}");
        }
        // The suspected cause is marked unverified, because it is.
        assert!(c.contains("(unverified, per anvil#1270)"), "{c}");
    }

    #[test]
    fn a_degraded_report_summarises_the_recovery_progress() {
        let j = r#"{"advertised":5,"received":1,"coordination":{"degraded":true,
                    "degraded_for_secs":420,"consecutive_receives_toward_recovery":2,
                    "recovery_threshold":5}}"#;
        let h = parse(j).expect("parse");
        assert_eq!(h.verdict, Verdict::Degraded);
        assert!(h.summary.contains("1 received / 5 advertised"), "{}", h.summary);
        assert!(h.summary.contains("degraded for 420s"), "{}", h.summary);
        assert!(h.summary.contains("2/5 sustained receive(s)"), "{}", h.summary);
    }

    #[test]
    fn a_green_report_is_short() {
        let j = r#"{"advertised":5,"received":5,"coordination":{"degraded":false}}"#;
        let h = parse(j).expect("parse");
        assert_eq!(h.verdict, Verdict::Green);
        assert!(h.summary.contains("healthy (5 received / 5 advertised)"), "{}", h.summary);
    }

    #[test]
    fn an_unexpected_shape_is_unknown_not_healthy() {
        // The distinction is the whole point: "could not determine" must never
        // be reported as a clean coordination path.
        for j in [
            r#"{"coordination":{}}"#,
            r#"{"coordination":{"degraded":null}}"#,
            r#"{"coordination":{"degraded":"true"}}"#,
            r#"{"coordination":{"degraded":1}}"#,
            r#"{}"#,
            "not json at all",
            "",
        ] {
            assert_eq!(parse(j), None, "{j:?} must be unknown");
        }
    }

    #[test]
    fn missing_counters_render_as_zero_or_question_mark() {
        // Absent counts read as 0; absent thresholds read as `?`. Rendering a
        // missing threshold as 0 would say "2/0 toward recovery", which is
        // both wrong and alarming.
        let j = r#"{"coordination":{"degraded":true}}"#;
        let h = parse(j).expect("parse");
        assert!(h.summary.contains("0 received / 0 advertised"), "{}", h.summary);
        assert!(h.summary.contains("degraded for ?s"), "{}", h.summary);
        assert!(h.summary.contains("0/? sustained"), "{}", h.summary);
    }

    #[test]
    fn the_cooldown_counts_down_and_then_expires() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("cooldown");
        std::fs::write(&p, "1000 issue-42 1\n").expect("write");
        assert_eq!(cooldown_remaining(&p, 1000, 3600), Some(3600));
        assert_eq!(cooldown_remaining(&p, 2800, 3600), Some(1800));
        assert_eq!(cooldown_remaining(&p, 4600, 3600), None, "expired");
        assert_eq!(cooldown_remaining(&p, 9999, 3600), None);
    }

    #[test]
    fn a_missing_or_corrupt_cooldown_file_means_no_cooldown() {
        let d = tempfile::tempdir().expect("tempdir");
        assert_eq!(cooldown_remaining(&d.path().join("nope"), 1, 3600), None);
        let p = d.path().join("bad");
        std::fs::write(&p, "notanumber x\n").expect("write");
        assert_eq!(cooldown_remaining(&p, 1, 3600), None);
    }

    #[test]
    fn a_record_stamped_in_the_future_fails_open_like_the_shell() {
        // Renamed and inverted. The old version asserted that a future-stamped
        // record suppresses for the FULL window, and called that "not
        // underflowing" — it does not underflow, but it is the wrong
        // direction, and the test enshrined it.
        //
        // The shell's guard is `elapsed >= 0 && elapsed < COOLDOWN`, whose own
        // comment says a timestamp in the future (clock skew) FAILS OPEN. A
        // host whose clock jumped forward would otherwise go six hours without
        // reporting a real coordination outage. A duplicate issue is noise; a
        // missing one is an outage nobody hears about.
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("cooldown");
        std::fs::write(&p, "5000 issue-42 1\n").expect("write");
        assert_eq!(
            cooldown_remaining(&p, 1000, 3600),
            None,
            "a future-stamped record must NOT suppress"
        );
        // And the ordinary case still behaves.
        assert_eq!(cooldown_remaining(&p, 6000, 3600), Some(2600));
    }
}

#[cfg(test)]
mod shell_differential {
    use super::*;

    /// The #6222 issue body, rendered by the RETIRED shell implementation at
    /// `143fe332^:defaults/scripts/cli/loom-daemon-watchdog.sh`, for the
    /// fixture below. Captured by running its own heredoc under bash — not
    /// retyped, which would only prove the two agree with my typing.
    ///
    /// This is the successor proof that lets the three `#7508` static scans be
    /// retired (`defaults/docs/verification-recipes.md` §6). Those scans
    /// checked the body was BUILT safely — `read -d \'\'` rather than
    /// `$(cat <<EOF)`, because wrapping a heredoc in command substitution
    /// trips a bash 3.2 lexer bug that silently dropped the body 1099 times
    /// from 2026-08-16. This checks the body IS the same body, byte for byte,
    /// which subsumes it: a construction defect cannot survive an exact match.
    const SHELL_RENDERED_BODY: &str = "The `peer_coordination` section of `loom-daemon health` has gone DEGRADED on host\n\
         `build-01`. This host's one-way peer-claim RECEIVE path (Safehouse, #6157)\n\
         can no longer be trusted to prove another host has already claimed an issue.\n\
         This is diagnostic only since Epic #6165 Phase 4 (#6317): it no longer freezes\n\
         stale-claim reclamation, which now gates solely on the lease record (#6286)\n\
         (see `.loom/docs/safehouse.md` -> \"Peer-claim coordination: cross-host soft\n\
         claim (#4028)\").\n\
         \n\
         - **Host**: `build-01`\n\
         - **Verdict**: peer-claim receive path DEGRADED (7 received / 10 advertised), degraded for 300s - 1/3 sustained receive(s) toward recovery\n\
         - **Degraded for**: 300s\n\
         - **Recovery progress**: 1/3 consecutive sustained receive(s) toward recovery\n\
         - **Watchdog log**: `/home/u/.loom/logs/daemon-watchdog.log`\n\
         \n\
         **To recover by hand**: run `loom-daemon peer-claims` (or `loom-daemon health`)\n\
         on `build-01` to confirm the live `peer_coordination` state, and check\n\
         Safehouse connectivity to peer hosts (`.loom/docs/safehouse.md`).\n\
         \n\
         **This alert clears itself** — no manual close needed. Filed automatically by\n\
         the loom-daemon-watchdog.sh peer-coordination escalation (#6222, Layer 3 of\n\
         #6157). Deduped by a sentinel at `/home/u/.loom/.watchdog-peer-coordination-escalated`, which is cleared\n\
         automatically (and this issue commented on + closed) once a later watchdog\n\
         tick observes `peer_coordination` back to healthy.\n\
         ";

    fn fixture() -> Health {
        Health {
            verdict: Verdict::Degraded,
            summary: "peer-claim receive path DEGRADED (7 received / 10 advertised), degraded \
                      for 300s - 1/3 sustained receive(s) toward recovery"
                .to_string(),
            degraded_for_secs: Some(300),
            consecutive: Some(1),
            recovery_threshold: Some(3),
        }
    }

    #[test]
    fn the_issue_body_is_byte_identical_to_the_shell_it_replaced() {
        let ours = body(
            "build-01",
            &fixture(),
            Path::new("/home/u/.loom/logs/daemon-watchdog.log"),
            Path::new("/home/u/.loom/.watchdog-peer-coordination-escalated"),
        );
        assert_eq!(
            ours, SHELL_RENDERED_BODY,
            "the ported issue body diverged from the shell's. If this is deliberate, say so and \
             re-capture the constant; if it is not, it is a port defect in text that gets filed \
             unattended during an outage."
        );
        assert_eq!(ours.len(), 1415, "byte count is part of the claim");
        assert_eq!(ours.lines().count(), 23, "line count is part of the claim");
    }

    #[test]
    fn the_body_carries_no_construct_that_could_substitute_a_command() {
        // What the #7508 scans protected, restated as a property of the port:
        // there is no shell, no heredoc and no command substitution here, so a
        // backtick in the prose is inert. Asserting the backticks are PRESENT
        // is the point — the shell had to escape every one of them, and this
        // proves the port carries them literally rather than having dodged the
        // hazard by rewording.
        let ours = body("build-01", &fixture(), Path::new("/l"), Path::new("/s"));
        assert!(ours.contains("`peer_coordination`"), "backticks are carried literally");
        assert!(ours.contains("host's"), "and so is the apostrophe that broke bash 3.2");
    }
}
