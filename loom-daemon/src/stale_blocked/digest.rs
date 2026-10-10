//! Champion's merge-risk hold digest is not a parked artifact (issue #9397).
//!
//! Champion keeps one pinned issue per repository, overwritten every pass,
//! listing the PRs it is holding (`champion-pr-merge.md` § "Per-PR Digest").
//! It carries `loom:blocked` only because that label is what keeps Curator
//! off it. It has no blocker to cite and never will, so every `loom:blocked`
//! re-check reported it as an undocumented block on every run.
//!
//! [`super::batch::gather_filtered`] therefore drops it at **selection**,
//! before any read. That covers every verdict (a digest row quoting
//! `Blocked by #N` would otherwise surface as stale or prose-only) and every
//! caller: the advisory, the cleared-blocker notifier, and the release pass,
//! which must never strip the label that keeps the digest out of curation.
//!
//! The rule is the fleet convention settled in 2AMLogic/2am#1507, and the one
//! Champion's own Step 0 uses to find the digest it overwrites (#7338): the
//! exact title, OR a body that starts with the marker. Both halves are needed:
//! a digest that predates the marker is title-only.

use super::Artifact;
use crate::forge_listing::RestIssue;

/// The digest issue's title. Mirrors `DIGEST_TITLE` in `champion-pr-merge.md`;
/// the test below pins the two together.
pub const DIGEST_TITLE: &str = "Champion: Merge-Risk Hold Digest";

/// The marker Champion writes at position 0 of the digest body. Mirrors
/// `DIGEST_MARKER` in `champion-pr-merge.md`.
pub const DIGEST_MARKER: &str = "<!-- champion:merge-risk-hold-digest -->";

/// Whether an **issue** with this title and body is Champion's digest.
///
/// The title must match exactly and the marker must be at byte 0 of the body.
/// `starts_with`, never `contains`: an ordinary issue that quotes the marker
/// while discussing it is still an ordinary issue.
///
/// Both inputs are forge text. Matching either one only removes a row from an
/// advisory, and the row must already carry `loom:blocked`, which takes triage
/// rights to apply.
#[must_use]
pub fn is_merge_risk_digest(title: &str, body: &str) -> bool {
    title == DIGEST_TITLE || body.starts_with(DIGEST_MARKER)
}

/// Whether the gatherer should drop this listing row. Issues only: a pull
/// request is never the digest, whatever its title says.
#[must_use]
pub fn excluded(kind: Artifact, row: &RestIssue) -> bool {
    kind == Artifact::Issue
        && is_merge_risk_digest(
            row.title.as_deref().unwrap_or_default(),
            row.body.as_deref().unwrap_or_default(),
        )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::super::batch::{gather_all, gather_filtered, Gathering, Options};
    use super::super::batch_tests::{fleet, issue, row, state, Fake};
    use super::super::budget::Floor;
    use super::super::notify::gather_cited;
    use super::super::{classify, Verdict};
    use super::*;

    const PROMPT: &str =
        include_str!("../../../defaults/.claude/commands/loom/champion-pr-merge.md");

    fn titled(mut r: RestIssue, title: &str) -> RestIssue {
        r.title = Some(title.to_string());
        r
    }

    fn opts(limit: u32) -> Options {
        Options {
            limit,
            no_prs: false,
            floor: Floor::default(),
        }
    }

    fn numbers(g: &Gathering) -> Vec<i64> {
        g.items.iter().map(|i| i.number).collect()
    }

    /// A marker digest with comments, a title-only digest with comments and an
    /// empty body, and one ordinary undocumented issue.
    fn two_digests_and_one_ordinary() -> Fake {
        let mut fake = Fake::default();
        let marker_body = format!("{DIGEST_MARKER}\n| PR | reason |\n");
        fake.rows
            .push(titled(row(10, &marker_body, 3, false, &["loom:blocked"]), "renamed digest"));
        fake.rows
            .push(titled(row(11, "", 2, false, &["loom:blocked"]), DIGEST_TITLE));
        fake.rows.push(issue(12, "nothing cited"));
        fake
    }

    #[test]
    fn constants_match_the_champion_prompt() {
        for line in [
            format!("DIGEST_TITLE=\"{DIGEST_TITLE}\""),
            format!("DIGEST_MARKER=\"{DIGEST_MARKER}\""),
        ] {
            assert!(
                PROMPT.lines().any(|l| l.trim() == line),
                "champion-pr-merge.md no longer assigns `{line}`; update \
                 stale_blocked::digest to match, or the digest is reported as \
                 an undocumented block again (#9397)"
            );
        }
    }

    #[test]
    fn exact_title_or_leading_marker_is_a_digest() {
        let marked = format!("{DIGEST_MARKER}\n\n| PR |");
        assert!(is_merge_risk_digest(DIGEST_TITLE, ""));
        assert!(is_merge_risk_digest(DIGEST_TITLE, "any body at all"));
        assert!(is_merge_risk_digest("any title", &marked));
        assert!(is_merge_risk_digest("any title", DIGEST_MARKER));
        assert!(is_merge_risk_digest(DIGEST_TITLE, &marked));
    }

    #[test]
    fn a_marker_anywhere_but_position_zero_is_not_a_digest() {
        for body in [
            format!(" {DIGEST_MARKER}"),
            format!("\n{DIGEST_MARKER}"),
            format!("Intro line\n{DIGEST_MARKER}\n"),
            format!("The marker is `{DIGEST_MARKER}`."),
            String::new(),
        ] {
            assert!(!is_merge_risk_digest("some issue", &body), "{body:?}");
        }
    }

    #[test]
    fn a_title_that_only_contains_the_digest_title_is_not_a_digest() {
        for title in [
            format!("Re: {DIGEST_TITLE}"),
            format!("{DIGEST_TITLE} is reported as undocumented"),
            format!("{DIGEST_TITLE} "),
            DIGEST_TITLE.to_lowercase(),
            String::new(),
        ] {
            assert!(!is_merge_risk_digest(&title, "nothing"), "{title:?}");
        }
    }

    #[test]
    fn digests_are_dropped_and_an_ordinary_undocumented_issue_is_still_reported() {
        let mut fake = two_digests_and_one_ordinary();
        let g = gather_all(&mut fake, &fleet(), opts(1000));
        assert_eq!(numbers(&g), vec![12]);
        let evidence = g.items[0].evidence.as_ref().expect("evaluated");
        assert_eq!(classify(evidence), Verdict::Undocumented);
    }

    #[test]
    fn a_digest_costs_no_comment_read() {
        let mut fake = two_digests_and_one_ordinary();
        gather_all(&mut fake, &fleet(), opts(1000));
        assert!(fake.comment_calls.is_empty(), "{:?}", fake.comment_calls);
    }

    #[test]
    fn a_digest_does_not_count_against_the_limit() {
        let mut fake = two_digests_and_one_ordinary();
        let g = gather_all(&mut fake, &fleet(), opts(1));
        assert_eq!(numbers(&g), vec![12]);
    }

    #[test]
    fn a_digest_citing_a_closed_blocker_is_still_dropped() {
        let mut fake = Fake::default();
        let body = format!("{DIGEST_MARKER}\n| #40 | Blocked by #77 |\n");
        fake.rows.push(issue(10, &body));
        fake.rows
            .push(titled(issue(11, "Blocked by #77"), DIGEST_TITLE));
        fake.states.insert((None, 77), state("CLOSED"));
        let g = gather_all(&mut fake, &fleet(), opts(1000));
        assert!(g.items.is_empty(), "{:?}", numbers(&g));
        assert!(fake.state_calls.is_empty(), "{:?}", fake.state_calls);
        assert!(fake.closing_calls.is_empty(), "{:?}", fake.closing_calls);
    }

    #[test]
    fn a_pull_request_with_the_digest_title_or_marker_is_not_dropped() {
        let mut fake = Fake::default();
        fake.rows
            .push(titled(row(20, "nothing cited", 0, true, &["loom:blocked"]), DIGEST_TITLE));
        fake.rows
            .push(row(21, DIGEST_MARKER, 0, true, &["loom:blocked"]));
        let g = gather_all(&mut fake, &fleet(), opts(1000));
        assert_eq!(numbers(&g), vec![20, 21]);
        assert!(g.items.iter().all(|i| i.kind == Artifact::Pr));
    }

    /// The release pass and the notifier both select through `keep`.
    #[test]
    fn a_filtered_gather_never_offers_a_digest_to_its_caller() {
        let mut fake = two_digests_and_one_ordinary();
        let mut offered: Vec<i64> = Vec::new();
        let g = gather_filtered(&mut fake, &fleet(), opts(1000), &mut |_, n, _| {
            offered.push(n);
            true
        });
        assert_eq!(offered, vec![12]);
        assert_eq!(numbers(&g), vec![12]);
    }

    #[test]
    fn the_cleared_blocker_notifier_never_returns_a_digest() {
        let mut fake = Fake::default();
        let body = format!("{DIGEST_MARKER}\nBlocked by #77\n");
        fake.rows.push(issue(10, &body));
        fake.rows.push(issue(12, "Blocked by #77"));
        fake.states.insert((None, 77), state("CLOSED"));
        let out = gather_cited(&mut fake, &fleet(), opts(1000), &[77]);
        assert_eq!(numbers(&out.gathering), vec![12]);
        let cited: Vec<i64> = out.cited.keys().map(|(_, n)| *n).collect();
        assert_eq!(cited, vec![12]);
    }
}
