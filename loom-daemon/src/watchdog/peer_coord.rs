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

#[cfg(test)]
mod tests {
    use super::*;

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
