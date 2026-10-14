//! Naming *why* `session-exec` refused a dispatch, without any shell.
//!
//! A refusal exits 78, which the adapter script's shared classifier reports
//! as a generic `RECOVERABLE` terminal record. When the daemon binary knows
//! the specific cause, it says so itself: [`announce`] writes one line to
//! stderr,
//!
//! ```text
//! # LOOM_SESSION_REFUSAL v=1 category=SESSION_DOWN
//! ```
//!
//! which reaches the tick's log through the adapter's ordinary stderr
//! pass-through. The daemon's terminal-record parser
//! (`sweep_registry::parse_terminal_result_after`) then calls [`category_of`], which
//! replaces the record's category with the announced one. The adapter script
//! needs no line per category, so the shell budget does not grow with them
//! (#10455; `SESSION_DOWN` was first, #10364's mount-drift refusal is next).
//!
//! **Adding a category** is daemon-only: add the `TerminalClassification`
//! variant and its `FromStr` arm, add one arm to [`wire`], and call
//! [`announce`] where `session-exec` refuses.
//!
//! The override is narrow on purpose. It applies only to a record that is
//! exactly what a refusal looks like from the script's side: the generic
//! `RECOVERABLE` category with the refusal exit code ([`REFUSAL_EXIT_CODE`]).
//! And it can only produce a category [`wire`] lists.
//!
//! **What a forged line can do.** The tick log does not separate
//! `session-exec`'s stderr from the agent's output, so a process inside the
//! session can print the marker. If that run then genuinely ends as
//! `RECOVERABLE`/78, it is recorded as the announced category instead: for
//! `SESSION_DOWN` that means no transient back-off on the account and a
//! `session-down` label on the span. That is the worst case. It cannot touch
//! a success, a record with any other exit code, or any other category, so it
//! can never erase an account hold (`TOKEN_EXHAUSTED`, `TOKEN_EXPIRED`,
//! `SESSION_LIMIT`, …).

use crate::tokens_pool::health::TerminalClassification;

/// Line prefix of a refusal announcement.
pub const REFUSAL_MARKER: &str = "# LOOM_SESSION_REFUSAL ";

/// The exit code every `session-exec` refusal uses.
pub const REFUSAL_EXIT_CODE: i32 = 78;

/// The categories a refusal may announce, with their wire spelling (the same
/// spelling `TerminalClassification::from_str` reads).
#[must_use]
pub fn wire(category: TerminalClassification) -> Option<&'static str> {
    match category {
        TerminalClassification::SessionDown => Some("SESSION_DOWN"),
        TerminalClassification::SessionMountStale => Some("SESSION_MOUNT_STALE"),
        _ => None,
    }
}

/// The announcement line for `category`, or `None` when it is not a refusal
/// category.
#[must_use]
pub fn marker_line(category: TerminalClassification) -> Option<String> {
    wire(category).map(|name| format!("{REFUSAL_MARKER}v=1 category={name}"))
}

/// Announce on stderr that this `session-exec` invocation is refusing for
/// `category`. Call it just before returning the refusal.
pub fn announce(category: TerminalClassification) {
    if let Some(line) = marker_line(category) {
        eprintln!("{line}");
    }
}

/// The category announced by the last valid refusal line in `region`.
#[must_use]
pub fn announced(region: &str) -> Option<TerminalClassification> {
    region
        .lines()
        .filter_map(|line| line.strip_prefix(REFUSAL_MARKER))
        .filter_map(|fields| {
            let mut fields = fields.split_ascii_whitespace();
            (fields.next() == Some("v=1")).then_some(())?;
            let category: TerminalClassification =
                fields.next()?.strip_prefix("category=")?.parse().ok()?;
            (fields.next().is_none() && wire(category).is_some()).then_some(category)
        })
        .next_back()
}

/// The category to use for a terminal record found in `region` (one tick's
/// log) that reported `category` with `exit_code`.
#[must_use]
pub fn apply(
    region: &str,
    category: TerminalClassification,
    exit_code: i32,
) -> TerminalClassification {
    if exit_code != REFUSAL_EXIT_CODE || category != TerminalClassification::Recoverable {
        return category;
    }
    announced(region).unwrap_or(category)
}

/// The category of a terminal record with the parsed `fields`
/// (`category=`, `exit_code=`) found in `region`, after [`apply`]. `None`
/// when either field is missing or unparseable, as the record parser needs.
#[must_use]
pub fn category_of(
    region: &str,
    fields: &std::collections::HashMap<&str, &str>,
) -> Option<TerminalClassification> {
    let category = fields.get("category")?.parse().ok()?;
    Some(apply(region, category, fields.get("exit_code")?.parse().ok()?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use TerminalClassification::{
        Fatal, Recoverable, SessionDown, SessionLimit, SessionMountStale, Success, TokenExhausted,
        TokenExpired,
    };

    #[test]
    fn the_marker_round_trips_through_the_parser() {
        let line = marker_line(SessionDown).unwrap();
        assert_eq!(line, "# LOOM_SESSION_REFUSAL v=1 category=SESSION_DOWN");
        assert_eq!(announced(&line), Some(SessionDown));
        assert_eq!(marker_line(Recoverable), None, "not a refusal category");
    }

    /// Add each new refusal category to this list.
    #[test]
    fn every_wire_name_parses_back_to_its_category() {
        let categories: &[TerminalClassification] = &[SessionDown, SessionMountStale];
        for &category in categories {
            let parsed: TerminalClassification = wire(category).unwrap().parse().unwrap();
            assert_eq!(parsed, category);
        }
    }

    #[test]
    fn a_refusal_relabels_only_a_recoverable_78_record() {
        let log = "noise\n# LOOM_SESSION_REFUSAL v=1 category=SESSION_DOWN\nmore\n";
        assert_eq!(apply(log, Recoverable, 78), SessionDown);
        assert_eq!(apply(log, Recoverable, 1), Recoverable);
        assert_eq!(apply("no marker", Recoverable, 78), Recoverable);
    }

    /// #10364: the stale-mount refusal rides the same line; the last
    /// announcement in the region is the one that refused.
    #[test]
    fn a_stale_mount_refusal_is_announced_and_read_back() {
        let line = marker_line(SessionMountStale).unwrap();
        assert_eq!(line, "# LOOM_SESSION_REFUSAL v=1 category=SESSION_MOUNT_STALE");
        assert_eq!(apply(&line, Recoverable, 78), SessionMountStale);
        assert_eq!(apply(&line, Recoverable, 1), Recoverable);
        let both = format!("{}\n{line}\n", marker_line(SessionDown).unwrap());
        assert_eq!(apply(&both, Recoverable, 78), SessionMountStale);
    }

    #[test]
    fn a_marker_never_erases_an_account_hold_or_any_other_verdict() {
        let log = "# LOOM_SESSION_REFUSAL v=1 category=SESSION_DOWN\n";
        let stale = "# LOOM_SESSION_REFUSAL v=1 category=SESSION_MOUNT_STALE\n";
        for kept in [TokenExhausted, TokenExpired, SessionLimit, Fatal, Success] {
            assert_eq!(apply(log, kept, 78), kept, "{kept:?} at exit 78 stays as reported");
            assert_eq!(apply(stale, kept, 78), kept, "{kept:?} at exit 78 stays as reported");
        }
        let fields =
            std::collections::HashMap::from([("category", "TOKEN_EXHAUSTED"), ("exit_code", "78")]);
        assert_eq!(category_of(log, &fields), Some(TokenExhausted));
    }

    #[test]
    fn malformed_or_unlisted_announcements_are_ignored() {
        for line in [
            "# LOOM_SESSION_REFUSAL v=2 category=SESSION_DOWN",
            "# LOOM_SESSION_REFUSAL v=1 category=TOKEN_EXHAUSTED",
            "# LOOM_SESSION_REFUSAL v=1 category=BOGUS",
            "# LOOM_SESSION_REFUSAL v=1 category=SESSION_DOWN extra=1",
            "  # LOOM_SESSION_REFUSAL v=1 category=SESSION_DOWN",
            "# LOOM_SESSION_REFUSAL category=SESSION_DOWN",
        ] {
            assert_eq!(apply(line, Recoverable, 78), Recoverable, "{line}");
        }
    }
}
