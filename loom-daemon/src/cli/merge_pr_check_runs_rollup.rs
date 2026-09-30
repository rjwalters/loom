//! `loom-daemon merge-pr check-runs-rollup` (#8191 slice): the check-runs
//! rollup PARSE at the top of every `_wait_for_checks_then_sync_merge` poll.
//!
//! # Protocol
//!
//! Reads one check-runs rollup payload on stdin — exactly the bytes
//! `forge_get_check_runs` printed, unvalidated — and prints a
//! sentinel-led header followed by one record per name:
//!
//! ```text
//! LOOM-CHECK-RUNS-ROLLUP<TAB><total_count><TAB><failing-any><TAB><pending-any><TAB><pending-lines>
//! FAILING<TAB><name>
//! PENDING<TAB><name>
//! ```
//!
//! - `<total_count>` is ASCII digits, always — the `.total_count // 0`
//!   alternative and the retired `[[ =~ ^[0-9]+$ ]]` gate are both applied
//!   here, so the caller's `-gt` arithmetic can never be handed `7.0`.
//! - `<failing-any>` / `<pending-any>` are `1`/`0`, and they answer the
//!   question the shell's `[[ -n "$failing" ]]` actually asked: whether the
//!   NEWLINE-JOINED list is non-empty. That is not "the list has rows" — a
//!   lone check-run named `""` produced one empty line, which command
//!   substitution stripped to the empty string, so the retired shell read it
//!   as "nothing pending". Answering it here keeps that verbatim instead of
//!   re-deriving it from the record count, which would flip it.
//! - `<pending-lines>` is `printf '%s\n' "$pending" | wc -l` — the count the
//!   shell narrates as "N check(s) still running". `1` for an empty list,
//!   because `printf` still emits a line; the caller only reads it under
//!   `<pending-any>`.
//! - The `FAILING`/`PENDING` records are in `unique` order (sorted,
//!   de-duplicated) and exist only to be re-joined into the NUL frame
//!   `merge-pr checks-failure` consumes, which drops empty lines — so an
//!   empty name's record is harmless there and is emitted for fidelity.
//!
//! Always exits 0 for readable stdin, INCLUDING a payload no JSON parser can
//! walk: that case is `…ROLLUP\t0\t0\t0\t1` with no records, which is what
//! the retired `jq … 2>/dev/null || true` produced and what routes the poll
//! into #6169's zero-row guard rather than declaring settlement. Exit 2 is
//! reserved for stdin that could not be READ at all — a different event, and
//! one the caller must not confuse with "the rollup was empty".
//!
//! Anything else — a missing/older binary, a clap usage error, silence — must
//! be read by the caller as "this poll was never classified" and treated as
//! still-pending: which checks are failing or running is then unknown, and
//! the one thing a fault must never do is authorize a merge on unclassified
//! checks. Degrading to the all-empty answer instead would be worse than the
//! retired shell, because a poll that had already latched
//! `observed_checks=true` would read the empty answer as settlement.
//!
//! The payload arrives on stdin, never in argv: a rollup is
//! forge-controlled text with no length bound, and check names are set by
//! whoever authored the workflow.

use anyhow::Result;
use loom_daemon::merge_pr::check_runs_rollup::{parse, Rollup};
use std::io::Read;

#[derive(clap::Args)]
pub(crate) struct CheckRunsRollupArgs {}

impl CheckRunsRollupArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut raw = Vec::new();
        if std::io::stdin().read_to_end(&mut raw).is_err() {
            eprintln!("merge-pr check-runs-rollup: could not read the rollup payload on stdin");
            std::process::exit(2);
        }
        // Lossy rather than fatal on bad UTF-8, like every other stdin-fed
        // verb in this family: a payload that is not valid UTF-8 is not valid
        // JSON either, so it lands on the all-empty answer regardless, and
        // refusing to read it would turn a forge hiccup into exit 2.
        let rollup = parse(&String::from_utf8_lossy(&raw));
        print!("{}", render(&rollup));
        Ok(())
    }
}

/// The full stdout block for one parsed rollup (trailing newline included).
fn render(r: &Rollup) -> String {
    let mut out = format!(
        "LOOM-CHECK-RUNS-ROLLUP\t{}\t{}\t{}\t{}\n",
        r.total_count,
        u8::from(!Rollup::joined(&r.failing).is_empty()),
        u8::from(!Rollup::joined(&r.pending).is_empty()),
        Rollup::line_count(&r.pending),
    );
    for name in &r.failing {
        out.push_str(&format!("FAILING\t{name}\n"));
    }
    for name in &r.pending {
        out.push_str(&format!("PENDING\t{name}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_then_records_in_unique_order() {
        let out = render(&parse(
            r#"{"total_count":3,"check_runs":[
                {"name":"zeta","status":"completed","conclusion":"failure"},
                {"name":"Alpha","status":"completed","conclusion":"timed_out"},
                {"name":"Build","status":"in_progress","conclusion":null}]}"#,
        ));
        assert_eq!(
            out,
            "LOOM-CHECK-RUNS-ROLLUP\t3\t1\t1\t1\nFAILING\tAlpha\nFAILING\tzeta\nPENDING\tBuild\n"
        );
    }

    #[test]
    fn a_walkable_but_empty_rollup_reports_nothing_and_still_exits_through_the_header() {
        assert_eq!(
            render(&parse(r#"{"total_count":0,"check_runs":[]}"#)),
            "LOOM-CHECK-RUNS-ROLLUP\t0\t0\t0\t1\n"
        );
    }

    #[test]
    fn malformed_input_is_the_same_all_empty_header_not_an_error() {
        assert_eq!(render(&parse("not json")), "LOOM-CHECK-RUNS-ROLLUP\t0\t0\t0\t1\n");
    }

    #[test]
    fn a_lone_empty_name_reports_pending_any_zero_like_the_shells_n_test() {
        // One record, but the joined string was empty — `[[ -n "$pending" ]]`
        // was false, so the caller must not see a 1 here.
        let out =
            render(&parse(r#"{"total_count":1,"check_runs":[{"name":"","status":"queued"}]}"#));
        assert_eq!(out, "LOOM-CHECK-RUNS-ROLLUP\t1\t0\t0\t1\nPENDING\t\n");
    }

    #[test]
    fn pending_lines_counts_names_not_records_when_a_name_is_empty() {
        let out = render(&parse(
            r#"{"total_count":2,"check_runs":[
                {"name":"","status":"queued"},{"name":"CI","status":"queued"}]}"#,
        ));
        // Joined string is "\nCI": non-empty, two lines.
        assert_eq!(out, "LOOM-CHECK-RUNS-ROLLUP\t2\t0\t1\t2\nPENDING\t\nPENDING\tCI\n");
    }
}
