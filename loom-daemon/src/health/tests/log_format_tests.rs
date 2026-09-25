//! Issue #8504: `daemon.log`'s line-prefix timestamp must always be UTC with
//! a trailing `Z` designator, never host-local time.
//!
//! Split out of `health/tests.rs` rather than appended to it: that file is
//! over the `.loom/docs/file-size-policy.md` threshold and therefore frozen
//! at its current size, so new cases go in a sibling module instead (mirrors
//! `model_class_tests`'s own doc comment for the same reason).
//!
//! Declared as a child module of `health` (not of `tests`, despite living
//! under `health/tests/` on disk — see the `#[path]` declaration in
//! `health.rs`), so it inherits `use super::*;` directly from `health`'s own
//! public items (`log_now()` stays local to `tests.rs` itself, so this
//! module defines its own equivalent).

use super::*;

fn log_now() -> chrono::NaiveDateTime {
    chrono::NaiveDateTime::parse_from_str("2026-07-31T14:30:00.000Z", DAEMON_LOG_STAMP_FORMAT)
        .unwrap()
}

/// A stamp lacking the trailing `Z` designator (e.g. a regression back to
/// host-local time) must NOT parse — this is the guardrail that keeps the
/// designator from silently disappearing again.
#[test]
fn log_probe_rejects_a_stamp_without_the_z_designator() {
    let log =
        "[2026-07-31T14:29:30.500] [INFO] work_finder: tick — cap 16; 12 seen, 2 dispatched\n";
    assert_eq!(work_finder_log_tick_age_secs(log, log_now()), None);
}

/// `DAEMON_LOG_STAMP_FORMAT` is the ONE constant both the binary's
/// `daemon_service::setup_logging` writer and this module's reader
/// format/parse with — regex-pin its shape here (millisecond precision,
/// trailing `Z`, no local-time offset) so a future edit cannot drop the
/// designator without failing a test that has nothing to do with a live
/// daemon or a real log file.
#[test]
fn daemon_log_stamp_format_always_renders_a_trailing_z() {
    let re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").unwrap();
    let rendered = chrono::Utc::now()
        .format(DAEMON_LOG_STAMP_FORMAT)
        .to_string();
    assert!(re.is_match(&rendered), "stamp missing UTC Z designator: {rendered:?}");
    // And the format string itself must end in a literal `Z` (not `%z`/`%:z`,
    // which would render an offset like `+00:00` instead of the designator).
    assert!(
        DAEMON_LOG_STAMP_FORMAT.ends_with('Z'),
        "format string regressed away from a literal trailing Z: {DAEMON_LOG_STAMP_FORMAT:?}"
    );
}
