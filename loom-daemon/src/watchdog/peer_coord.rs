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
//!
//! #12818: this diagnostic reports to the watchdog log (and the loom-ui alert)
//! only. It no longer creates, comments on, reopens or closes a GitHub issue,
//! and it never reads the legacy escalation sentinel or cooldown files that the
//! old lifecycle left on hosts — they are inert. The GitHub notifications were
//! removed ahead of verified SigNoz visibility (#12167 tracks that export).

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

    // Issue #9294: the daemon's own verdict sentence, when it supplies one.
    // Appended rather than interpolated so every pre-#9294 assertion on this
    // summary's prefix still holds, and absent on a pre-#9294 daemon.
    //
    // It matters because "receive path DEGRADED" above is a guess about WHICH
    // half broke, and the measured 2026-09-29 outage was the other half: the
    // homeserver was refusing every *send* into the claims room, so no host
    // could publish and the receive path was fine. The watchdog files issues
    // from this string; without the reason it files a misdiagnosis.
    let reason_note = coordination
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .map_or_else(String::new, |r| format!(" — {r}"));

    let summary = if degraded {
        format!(
            "peer-claim receive path DEGRADED ({} received / {} advertised), degraded for {}s — \
             {}/{} sustained receive(s) toward recovery{reason_note}",
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

/// Report the daemon's verdict to the watchdog log, and nothing else (#12818).
///
/// Degraded is a `DIVERGENCE` line, green an `[OK]` line. No branch here
/// touches the forge or any state file, so a legacy sentinel or cooldown left
/// by the retired issue lifecycle cannot change what is reported or block the
/// tick. An undeterminable probe never reaches this function: [`parse`]
/// returns `None` and the caller reports nothing rather than claiming health.
pub fn report_health(health: &Health, reporter: &report::Reporter) {
    match health.verdict {
        Verdict::Degraded => reporter.report(
            report::Level::Divergence,
            &format!("peer-claim coordination is DEGRADED: {}.", health.summary),
        ),
        Verdict::Green => reporter.report(
            report::Level::Ok,
            &format!("peer-claim coordination is healthy: {}.", health.summary),
        ),
    }
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

    /// Issue #9294: the daemon's own verdict reason rides along, so a
    /// publish-side fault is not filed as a receive-path one. A pre-#9294
    /// daemon supplies no `reason` and the summary is unchanged (the test
    /// above covers that shape).
    #[test]
    fn a_degraded_report_carries_the_daemons_own_reason_when_it_supplies_one() {
        let j = r#"{"advertised":3765,"received":0,"coordination":{"degraded":true,
                    "degraded_for_secs":518400,"consecutive_receives_toward_recovery":0,
                    "recovery_threshold":3,
                    "reason":"every peer claim ad is being REJECTED by safehoused"}}"#;
        let h = parse(j).expect("parse");
        assert_eq!(h.verdict, Verdict::Degraded);
        assert!(
            h.summary.contains("REJECTED by safehoused"),
            "a publish-side fault must not be filed as a receive-path one: {}",
            h.summary
        );
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

    fn health(verdict: Verdict) -> Health {
        parse(&format!(
            r#"{{"advertised":5,"received":1,"coordination":{{"degraded":{}}}}}"#,
            verdict == Verdict::Degraded
        ))
        .expect("parse")
    }

    fn logged(f: impl FnOnce(&report::Reporter)) -> String {
        let d = tempfile::tempdir().expect("tempdir");
        let log = d.path().join("watchdog.log");
        f(&report::Reporter::new(log.clone(), false));
        std::fs::read_to_string(&log).unwrap_or_default()
    }

    #[test]
    fn degraded_reports_divergence_and_green_reports_ok() {
        let out = logged(|r| {
            report_health(&health(Verdict::Degraded), r);
            report_health(&health(Verdict::Green), r);
        });
        assert!(out.contains("[DIVERGENCE] peer-claim coordination is DEGRADED"), "{out}");
        assert!(out.contains("[OK] peer-claim coordination is healthy"), "{out}");
        assert!(!out.contains("issue") && !out.contains("sentinel"), "{out}");
    }

    /// The no-forge-write boundary (#12818): a degraded → degraded → green →
    /// degraded flap with legacy sentinel and cooldown files pre-seeded (one of
    /// them corrupt) reports every tick, never errors, and leaves those files
    /// byte-for-byte untouched. `report_health` takes only a `Reporter`, so it
    /// has no handle with which to reach the forge or any state file.
    #[test]
    fn a_flap_with_legacy_state_files_reports_each_tick_and_leaves_them_alone() {
        let d = tempfile::tempdir().expect("tempdir");
        let sentinel = d.path().join(".watchdog-peer-coord-escalated");
        let cooldown = d.path().join(".watchdog-peer-coord-cooldown");
        std::fs::write(&sentinel, "owner/repo#8709\n").expect("seed");
        std::fs::write(&cooldown, "\u{0}garbage not numbers").expect("seed");

        let out = logged(|r| {
            for v in [
                Verdict::Degraded,
                Verdict::Degraded,
                Verdict::Green,
                Verdict::Degraded,
            ] {
                report_health(&health(v), r);
            }
        });

        assert_eq!(out.lines().count(), 4, "{out}");
        assert_eq!(std::fs::read_to_string(&sentinel).expect("kept"), "owner/repo#8709\n");
        assert_eq!(std::fs::read_to_string(&cooldown).expect("kept"), "\u{0}garbage not numbers");
    }
}
