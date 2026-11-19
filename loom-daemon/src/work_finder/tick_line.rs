//! The multi-workspace loop's `work_finder: tick` INFO line, split out of the
//! size-frozen `work_finder.rs`.

use super::TickReport;
use crate::step_dispatch::gather_budget::millis;
use chrono::{DateTime, SecondsFormat, Utc};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Local log breadcrumbs that remain available when an export batch is lost.
pub(super) struct TickTiming<'a> {
    pub(super) number: u64,
    pub(super) host_id: &'a str,
    pub(super) process_id: Uuid,
    pub(super) began: Instant,
}

impl TickTiming<'_> {
    pub(super) fn start_line(
        &self,
        deadline: Duration,
        wait_started: Instant,
        previous_published: Option<Instant>,
    ) -> String {
        let previous_post_publish_ms = previous_published.map_or_else(
            || "none".to_owned(),
            |at| millis(wait_started.saturating_duration_since(at)).to_string(),
        );
        format!(
            "work_finder: tick {} start (deadline {}s, #12074) host_id={:?} process_instance_id={} \
             timer_wait_ms={} previous_post_publish_ms={previous_post_publish_ms}",
            self.number,
            deadline.as_secs(),
            self.host_id,
            self.process_id,
            millis(self.began.saturating_duration_since(wait_started)),
        )
    }

    pub(super) fn completion_line(
        &self,
        started_at: DateTime<Utc>,
        completed_at: DateTime<Utc>,
        span_started: Instant,
        span_completed: Instant,
        published: Instant,
    ) -> String {
        format!(
            "work_finder: tick {} complete host_id={:?} process_instance_id={} span_started_at={} \
             completed_at={} pre_span_ms={} span_ms={} publish_ms={}",
            self.number,
            self.host_id,
            self.process_id,
            started_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
            completed_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
            millis(span_started.saturating_duration_since(self.began)),
            millis(span_completed.saturating_duration_since(span_started)),
            millis(published.saturating_duration_since(span_completed)),
        )
    }

    pub(super) fn rate_limit_skip_line(&self, skipped_at: DateTime<Utc>, ended: Instant) -> String {
        let reason = crate::observability::pick_decision::RATE_LIMIT_HELD;
        format!(
            "work_finder: tick {} skipped host_id={:?} process_instance_id={} outcome={reason} \
             skipped_at={} skip_ms={}",
            self.number,
            self.host_id,
            self.process_id,
            skipped_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
            millis(ended.saturating_duration_since(self.began)),
        )
    }
}

/// The tick-level figures the line reports beside `report`'s counters.
#[derive(Debug, Clone)]
pub struct TickLine {
    /// The cap: the operator's `maxConcurrent` (#12406).
    pub max_concurrent: usize,
    pub pool_size: usize,
    pub token_limit: usize,
    pub min_workspace_healthy: usize,
    pub workspaces: usize,
}

/// Whether the tick did anything worth an INFO line.
#[must_use]
pub fn worth_logging(report: &TickReport) -> bool {
    report.dispatched > 0
        || report.errors > 0
        || report.skipped_quarantined > 0
        || report.skipped_workspace_commands_missing > 0
        || report.skipped_backoff > 0
        || report.skipped_pr_open_backoff > 0
        || report.skipped_noop_cooldown > 0
        || report.skipped_declined > 0
        || report.skipped_prless_retry > 0
        || report.skipped_recheck_interval > 0
        || report.skipped_host_constraint > 0
        || report.skipped_pr_open > 0
}

/// The line itself.
#[must_use]
pub fn render(report: &TickReport, t: &TickLine) -> String {
    format!(
        "work_finder: tick — cap {} (pool={}, healthy={} [fallback_root probe only], \
         min_workspace_healthy={} [minimum across every registered workspace's own resolved \
         pool, #7527]); {} workspace(s), \
         {} seen, {} dispatched, {} labeled-skip, {} in-flight-skip, \
         {} quarantine-skip, {} workspace-commands-missing-skip, \
         {} backoff-skip, {} pr-open-backoff, {} noop-cooldown-skip, \
         {} declined-skip, {} prless-retry-skip, \
         {} recheck-interval-skip, \
         {} host-constraint-skip, \
         {} pr-open-skip, \
         {} deferred (capacity), {} deferred (build back-off, #9410), \
         {} error(s), {} cross-host-collision(s)",
        t.max_concurrent,
        t.pool_size,
        t.token_limit,
        t.min_workspace_healthy,
        t.workspaces,
        report.seen,
        report.dispatched,
        report.skipped_labeled,
        report.skipped_in_flight,
        report.skipped_quarantined,
        report.skipped_workspace_commands_missing,
        report.skipped_backoff,
        report.skipped_pr_open_backoff,
        report.skipped_noop_cooldown,
        report.skipped_declined,
        report.skipped_prless_retry,
        report.skipped_recheck_interval,
        report.skipped_host_constraint,
        report.skipped_pr_open,
        report.deferred_capacity,
        report.deferred_build_backoff,
        report.errors,
        report.collisions
    )
}

/// Log the line at INFO when the tick did anything.
pub fn log(report: &TickReport, t: &TickLine) {
    if worth_logging(report) {
        log::info!("{}", render(report, t));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tick_line_names_the_cap() {
        let report = TickReport {
            dispatched: 1,
            ..TickReport::default()
        };
        let t = TickLine {
            max_concurrent: 12,
            pool_size: 4,
            token_limit: 4,
            min_workspace_healthy: 4,
            workspaces: 1,
        };
        assert!(worth_logging(&report));
        let line = render(&report, &t);
        assert!(line.starts_with("work_finder: tick — cap 12 "), "{line}");
        assert!(line.contains("1 dispatched"), "{line}");
        assert!(!worth_logging(&TickReport::default()));
    }

    #[test]
    fn completion_gaps_separate_tail_wait_pre_span_and_publication() {
        let previous_publish = Instant::now();
        let wait_started = previous_publish + Duration::from_secs(37);
        let began = wait_started + Duration::from_secs(61);
        let span_started = began + Duration::from_secs(541);
        let span_completed = span_started + Duration::from_secs(99);
        let published = span_completed + Duration::from_millis(7);
        let timing = TickTiming {
            number: 42,
            host_id: "host-fixture",
            process_id: Uuid::nil(),
            began,
        };
        let start =
            timing.start_line(Duration::from_secs(90), wait_started, Some(previous_publish));
        assert_eq!(
            start,
            "work_finder: tick 42 start (deadline 90s, #12074) host_id=\"host-fixture\" \
             process_instance_id=00000000-0000-0000-0000-000000000000 timer_wait_ms=61000 \
             previous_post_publish_ms=37000"
        );

        let start_wall = DateTime::parse_from_rfc3339("2026-11-19T05:00:12.000000123Z")
            .unwrap()
            .to_utc();
        let completion = timing.completion_line(
            start_wall,
            start_wall + chrono::Duration::seconds(99),
            span_started,
            span_completed,
            published,
        );
        assert_eq!(
            completion,
            "work_finder: tick 42 complete host_id=\"host-fixture\" \
             process_instance_id=00000000-0000-0000-0000-000000000000 \
             span_started_at=2026-11-19T05:00:12.000000123Z \
             completed_at=2026-11-19T05:01:51.000000123Z pre_span_ms=541000 span_ms=99000 publish_ms=7"
        );
    }

    #[test]
    fn a_rate_limit_skip_has_a_terminal_line_without_a_completion() {
        let began = Instant::now();
        let timing = TickTiming {
            number: 43,
            host_id: "host-fixture",
            process_id: Uuid::nil(),
            began,
        };
        let start = timing.start_line(Duration::from_secs(90), began, None);
        assert!(start.ends_with("timer_wait_ms=0 previous_post_publish_ms=none"));

        let skipped_at = DateTime::parse_from_rfc3339("2026-11-19T05:02:30Z")
            .unwrap()
            .to_utc();
        let skipped = timing.rate_limit_skip_line(skipped_at, began + Duration::from_millis(13));
        assert_eq!(
            skipped,
            "work_finder: tick 43 skipped host_id=\"host-fixture\" \
             process_instance_id=00000000-0000-0000-0000-000000000000 outcome=rate_limit_held \
             skipped_at=2026-11-19T05:02:30.000000000Z skip_ms=13"
        );
        assert!(!start.contains("complete"));
        assert!(!skipped.contains("completed_at"));
    }
}
