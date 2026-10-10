//! The `Forge events:` line on `loom-daemon status` (ADR-0021, #8765).
//!
//! A sibling module rather than more lines in `status_render.rs` (an
//! over-threshold ledger entry), matching how `holds` and `model_class`
//! already live here.
//!
//! The line always renders. Its whole reason for existing is that the
//! question an operator asks about a feed — "why is my cursor not
//! advancing?" — has several materially different answers that all look
//! identical if the surface stays silent: off by choice, never provisioned,
//! wrong key, wrong host, or simply a quiet feed.

use chrono::{DateTime, Utc};
use loom_daemon::health::format_window;
use loom_daemon::types::{ForgeEventsState as State, ForgeEventsStatus, ForgeEventsWakeStatus};

/// The feed line plus, when there is something to say about them, the Phase 2
/// early-tick consumers (#8995 item 3).
///
/// One function so the caller stays one `println!` — `status_render.rs` is an
/// over-threshold ledger entry that may not grow.
pub fn render_block(status: Option<&ForgeEventsStatus>, now: DateTime<Utc>) -> String {
    let mut lines = vec![render(status, now)];
    if let Some(s) = status {
        lines.extend(render_wakes(s));
        lines.extend(render_poll_gating(s));
    }
    lines.join("\n")
}

/// Zero or more `Forge event wakes:` lines.
///
/// One line per **armed** consumer. Nothing at all when nothing is armed *and*
/// the feed is off — the feed line already said `disabled`, and a host that
/// opted into neither half does not need a second line to say so. When the feed
/// is on but no consumer is armed, one line says exactly that: "the feed is
/// running and no loop is listening" is the confusing state worth naming, and
/// it is the state every Phase-1 host is in.
fn render_wakes(s: &ForgeEventsStatus) -> Vec<String> {
    if s.wakes.is_empty() {
        if s.state == State::Disabled {
            return Vec::new();
        }
        return vec![
            "Forge event wakes: none armed — every forgeEvents.events.* consumer is off, so \
             each loop ticks on its own cadence only"
                .to_string(),
        ];
    }
    s.wakes.iter().map(wake_line).collect()
}

/// The `Forge poll gating:` block (#9255): nothing when gating is off (the
/// default), otherwise a summary line (state, effective cap, skip/re-poll
/// counters, lossy-feed rate) and one line per workspace.
fn render_poll_gating(s: &ForgeEventsStatus) -> Vec<String> {
    let Some(g) = s.poll_gating.as_ref() else {
        return Vec::new();
    };
    let state = if g.gating_active {
        "ACTIVE (feed healthy)"
    } else {
        "inactive (feed not healthy: every workspace on base cadence)"
    };
    let lossy = g.lossy_rate().map_or_else(
        || "n/a (no hard-cap re-poll yet)".to_string(),
        |rate| format!("{:.1}% ({}/{})", rate * 100.0, g.lossy_repolls, g.hard_cap_repolls),
    );
    let mut lines = vec![format!(
        "Forge poll gating: {state} — hard cap {}s, {} poll(s) skipped, {} event re-poll(s), {} hard-cap re-poll(s), lossy-feed rate {lossy}",
        g.hard_cap_secs, g.polls_skipped, g.event_repolls, g.hard_cap_repolls
    )];
    for w in &g.workspaces {
        let age = w
            .last_poll_age_secs
            .map_or_else(|| "never".to_string(), |a| format!("{} ago", format_window(a)));
        lines.push(format!(
            "  {} ({}): {}, last poll {age}",
            w.workspace,
            w.repo,
            if w.gated { "gated" } else { "ungated" }
        ));
    }
    lines
}

/// One armed consumer's line: what it is, what it saw, and what that did.
///
/// A zero-prompt entry is reported as such rather than omitted — "armed and
/// nothing arrived" and "not armed" are different answers, and distinguishing
/// them is the whole point of the surface.
fn wake_line(w: &ForgeEventsWakeStatus) -> String {
    // The CI telemetry run consumer (#9201) is not an early ticker: it has no
    // spacing floor and no multiplier, so its counters mean something else.
    if w.config_key == loom_daemon::forge_events::wake::CI_TELEMETRY_RUNS.config_key {
        return format!(
            "Forge event wakes: {} (forgeEvents.events.{}) — {} prompt(s) → {} targeted \
             batch(es), {} run key(s) dropped; correction-floor sweep cadence {}s",
            w.consumer, w.config_key, w.prompts, w.early_ticks, w.throttled, w.cadence_secs
        );
    }
    format!(
        "Forge event wakes: {} (forgeEvents.events.{}) — {} prompt(s) → {} early tick(s), {} \
         throttled by the {}s floor; cadence {}s, ceiling {}x",
        w.consumer,
        w.config_key,
        w.prompts,
        w.early_ticks,
        w.throttled,
        w.min_spacing_secs,
        w.cadence_secs,
        w.max_multiplier()
    )
}

/// Render the one-line feed summary. `None` means the answering daemon
/// predates ADR-0021 — reported as such, never as `disabled`.
pub fn render(status: Option<&ForgeEventsStatus>, now: DateTime<Utc>) -> String {
    let Some(s) = status else {
        return "Forge events: unknown (older daemon binary — restart to pick up ADR-0021)"
            .to_string();
    };
    let host = s.host_id.as_deref().unwrap_or("unknown-host");
    let endpoint = s.endpoint.as_deref().unwrap_or("(no endpoint)");
    let detail = s
        .last_error_detail
        .as_deref()
        .map_or_else(String::new, |d| format!(" ({d})"));
    let last_poll = s
        .last_poll_age_secs(now)
        .map_or_else(|| "never".to_string(), |age| format!("{} ago", format_window(age)));
    match s.state {
        State::Disabled => "Forge events: disabled (no event feed — set forgeEvents.enabled=true \
             to opt in; polling is unaffected either way)"
            .to_string(),
        State::Misconfigured => {
            format!("Forge events: MISCONFIGURED — enabled but not polling → {endpoint}{detail}")
        }
        State::Connecting => format!(
            "Forge events: connecting — polling {endpoint} as host_id={host} from cursor {}, \
             no poll completed yet",
            s.cursor
        ),
        State::Healthy => format!(
            "Forge events: OK — last poll {last_poll}, cursor {} ({} event(s) in {} page(s)) \
             as host_id={host} → {endpoint}",
            s.cursor, s.events_observed, s.pages_observed
        ),
        State::AuthFailed => format!(
            "Forge events: AUTH FAILED — this host's event key is not accepted as \
             host_id={host}; cursor stuck at {}{detail}",
            s.cursor
        ),
        State::HostMismatch => format!(
            "Forge events: HOST MISMATCH — the feed at {endpoint} is not this host's; cursor \
             stuck at {}{detail}",
            s.cursor
        ),
        State::Failing => format!(
            "Forge events: FAILING — {} consecutive failed poll(s) as host_id={host}, cursor \
             stuck at {}{detail}",
            s.consecutive_failures, s.cursor
        ),
        // Cadence, not cause: `last_error` still names the class, so the line
        // reports both rather than letting the backoff promotion erase why.
        State::Backoff => format!(
            "Forge events: BACKOFF — {} consecutive failed poll(s) ({}), retrying every {}s as \
             host_id={host}, cursor stuck at {}{detail}",
            s.consecutive_failures,
            s.last_error.as_deref().unwrap_or("unknown"),
            s.poll_interval_secs,
            s.cursor
        ),
        State::Unrecognized => {
            format!("Forge events: unrecognized state from a newer daemon binary → {endpoint}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn status(state: State) -> ForgeEventsStatus {
        ForgeEventsStatus {
            state,
            endpoint: Some("https://events.internal/".to_string()),
            host_id: Some("mac-studio".to_string()),
            cursor: 42,
            pages_observed: 3,
            events_observed: 9,
            consecutive_failures: 4,
            poll_interval_secs: 300,
            last_error: Some("auth_failed".to_string()),
            last_error_detail: Some("feed rejected this host's event key with HTTP 401".into()),
            last_poll_at: Some(now() - chrono::Duration::seconds(30)),
            ..ForgeEventsStatus::default()
        }
    }

    #[test]
    fn a_pre_adr_daemon_is_not_reported_as_disabled() {
        let line = render(None, now());
        assert!(line.contains("older daemon binary"), "{line}");
        assert!(!line.contains("disabled"), "{line}");
    }

    #[test]
    fn disabled_names_the_opt_in_and_reassures_about_polling() {
        let line = render(Some(&ForgeEventsStatus::disabled()), now());
        assert!(line.contains("forgeEvents.enabled=true"), "{line}");
        assert!(line.contains("polling is unaffected"), "{line}");
    }

    #[test]
    fn misconfigured_names_the_missing_piece() {
        let line = render(
            Some(&ForgeEventsStatus::misconfigured(
                None,
                "forgeEvents.hostId not configured".to_string(),
            )),
            now(),
        );
        assert!(line.contains("MISCONFIGURED"), "{line}");
        assert!(line.contains("forgeEvents.hostId not configured"), "{line}");
    }

    #[test]
    fn backoff_keeps_the_underlying_class_and_the_stretched_cadence() {
        let line = render(Some(&status(State::Backoff)), now());
        assert!(line.contains("BACKOFF"), "{line}");
        assert!(line.contains("auth_failed"), "{line}");
        assert!(line.contains("every 300s"), "{line}");
    }

    #[test]
    fn healthy_reports_cursor_and_counts() {
        let line = render(Some(&status(State::Healthy)), now());
        assert!(line.contains("OK"), "{line}");
        assert!(line.contains("cursor 42"), "{line}");
        assert!(line.contains("9 event(s) in 3 page(s)"), "{line}");
        assert!(line.contains("30s ago"), "{line}");
    }

    #[test]
    fn every_error_state_names_the_stuck_cursor() {
        for state in [State::AuthFailed, State::HostMismatch, State::Failing] {
            let line = render(Some(&status(state)), now());
            assert!(line.contains("cursor stuck at 42"), "{state:?}: {line}");
        }
    }

    fn wake(config_key: &str, prompts: u64, early: u64, throttled: u64) -> ForgeEventsWakeStatus {
        ForgeEventsWakeStatus {
            consumer: "claim-reconcile wake".to_string(),
            config_key: config_key.to_string(),
            cadence_secs: 600,
            min_spacing_secs: 30,
            prompts,
            early_ticks: early,
            throttled,
        }
    }

    #[test]
    fn an_armed_consumer_reports_what_its_prompts_did() {
        let mut s = status(State::Healthy);
        s.wakes = vec![wake("claimReconcileWake", 9, 2, 7)];
        let block = render_block(Some(&s), now());
        let line = block
            .lines()
            .nth(1)
            .expect("a wake line follows the feed line");
        assert!(line.contains("forgeEvents.events.claimReconcileWake"), "{line}");
        assert!(line.contains("9 prompt(s)"), "{line}");
        assert!(line.contains("2 early tick(s)"), "{line}");
        assert!(line.contains("7 throttled"), "{line}");
        // The per-loop ceiling item 2 is about: 600s / 30s.
        assert!(line.contains("ceiling 20x"), "{line}");
    }

    // "armed and nothing arrived" is a different answer from "not armed", and
    // the surface exists to tell them apart.
    #[test]
    fn an_armed_consumer_with_no_prompts_still_reports() {
        let mut s = status(State::Healthy);
        s.wakes = vec![wake("claimReconcileWake", 0, 0, 0)];
        let block = render_block(Some(&s), now());
        assert_eq!(block.lines().count(), 2, "{block}");
        assert!(block.contains("0 prompt(s)"), "{block}");
    }

    #[test]
    fn a_running_feed_with_nothing_armed_says_so() {
        let block = render_block(Some(&status(State::Healthy)), now());
        assert!(block.contains("Forge event wakes: none armed"), "{block}");
        assert!(block.contains("forgeEvents.events.*"), "{block}");
    }

    // The all-default host: the feed line already says `disabled`, and a second
    // line to say the consumers are off too would be noise on every status.
    #[test]
    fn a_disabled_feed_with_nothing_armed_adds_no_wake_line() {
        let block = render_block(Some(&ForgeEventsStatus::disabled()), now());
        assert_eq!(block.lines().count(), 1, "{block}");
        assert!(!block.contains("wakes"), "{block}");
    }

    // A consumer armed without the feed provisioned is exactly the mistake the
    // surface should catch, so it renders even under `disabled`.
    #[test]
    fn a_consumer_armed_without_a_feed_is_still_reported() {
        let mut s = ForgeEventsStatus::disabled();
        s.wakes = vec![wake("claimReconcileWake", 0, 0, 0)];
        let block = render_block(Some(&s), now());
        assert_eq!(block.lines().count(), 2, "{block}");
    }

    #[test]
    fn a_pre_adr_daemon_gets_no_wake_lines() {
        assert_eq!(render_block(None, now()).lines().count(), 1);
    }

    #[test]
    fn poll_gating_renders_nothing_when_off_and_state_plus_lossy_rate_when_on() {
        let mut s = status(State::Healthy);
        assert!(!render_block(Some(&s), now()).contains("poll gating"));
        s.poll_gating = Some(loom_daemon::types::PollGatingStatus {
            enabled: true,
            gating_active: true,
            hard_cap_secs: 600,
            polls_skipped: 7,
            event_repolls: 2,
            hard_cap_repolls: 4,
            lossy_repolls: 1,
            workspaces: vec![loom_daemon::types::PollGatingWorkspace {
                workspace: "/ws/a".into(),
                repo: "o/a".into(),
                gated: true,
                last_poll_age_secs: Some(90),
            }],
        });
        let out = render_block(Some(&s), now());
        let line = out
            .lines()
            .find(|l| l.starts_with("Forge poll gating:"))
            .expect("gating line");
        assert_eq!(
            line,
            "Forge poll gating: ACTIVE (feed healthy) — hard cap 600s, 7 poll(s) skipped, 2 event re-poll(s), 4 hard-cap re-poll(s), lossy-feed rate 25.0% (1/4)"
        );
        assert!(out.contains("/ws/a (o/a): gated"), "{out}");
    }

    // The event key must never reach an operator-visible surface, even
    // indirectly: the status type carries only paths and classes, and this
    // asserts the renderer adds nothing.
    #[test]
    fn no_state_renders_anything_but_the_recorded_detail() {
        let mut s = status(State::Failing);
        s.last_error_detail =
            Some("could not read event key file /home/x/.loom/forge-events/key".into());
        let line = render(Some(&s), now());
        assert!(line.contains("/home/x/.loom/forge-events/key"), "{line}");
    }
}
