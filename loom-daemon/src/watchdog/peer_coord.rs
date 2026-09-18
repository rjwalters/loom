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
    super::env::num("LOOM_WATCHDOG_PEER_COORD_COOLDOWN_SECS", 3600)
}

/// Seconds remaining in the cooldown, or `None` when it has expired or no
/// recovery is on record.
#[must_use]
pub fn cooldown_remaining(state: &std::path::Path, now: u64, window: u64) -> Option<u64> {
    let text = std::fs::read_to_string(state).ok()?;
    let recovered_at: u64 = text.split_whitespace().next()?.parse().ok()?;
    let elapsed = now.saturating_sub(recovered_at);
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
    if now.saturating_sub(c.recovered_at) >= window {
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
        "peer-claim coordination has gone DEGRADED again on `{hostname}` ({summary}).\n\
         This is flap #{flap} since this tracking issue was first filed, landing within the \
         {window}s dedup window (#7664) since the last recovery — commenting here instead of \
         filing a fresh issue.\n\
         **Suspected cause** (unverified, per anvil#1270): `advertised` only moves at dispatch \
         time, so a RAM/disk-throttled host with cap 0 never advertises and cannot reach the \
         sustained-receive recovery threshold.\n\
         Filed automatically by the loom-daemon-watchdog.sh peer-coordination escalation \
         (#6222, dedup by #7664).\n"
    )
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
    fn a_clock_that_went_backwards_stays_inside_the_window() {
        // saturating_sub keeps elapsed at 0 rather than wrapping to a huge
        // number, which would look like "long outside the window" and file a
        // duplicate.
        assert_eq!(
            repeat_action(Some(&cd(5000, "1234", 1)), 1000, 86_400),
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
    fn a_clock_that_went_backwards_does_not_underflow() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("cooldown");
        std::fs::write(&p, "5000 issue-42 1\n").expect("write");
        // `now` before the recorded recovery: saturating_sub keeps elapsed at 0,
        // so the full window remains rather than wrapping to a huge number.
        assert_eq!(cooldown_remaining(&p, 1000, 3600), Some(3600));
    }
}
