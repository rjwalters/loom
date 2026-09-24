//! Population scoping: what the gate fires on, and — at least as important —
//! what it does not.

use super::*;

fn labels(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn proposal_labels_are_in_scope() {
    for l in PROPOSAL_LABELS {
        let t = trigger("anything", "", &labels(&[l]));
        assert_eq!(t, Some(Trigger::ProposalLabel((*l).to_string())), "{l}");
    }
}

#[test]
fn ordinary_labels_are_not() {
    assert_eq!(
        trigger(
            "bug: the retry loop double-counts attempts",
            "## Steps to reproduce\n1. run it\n\n## Expected\nonce\n\n## Actual\ntwice\n",
            &labels(&["loom:triage", "loom:curated", "tier:goal-supporting"]),
        ),
        None
    );
}

/// The acceptance criterion #8396 phrases as "routine bug fixes with a
/// verifiable acceptance criterion are demonstrably unaffected". The ordinary
/// bug-report headings must not be an incident vocabulary.
#[test]
fn ordinary_bug_report_headings_do_not_fire() {
    for heading in [
        "Steps to Reproduce",
        "Expected behaviour",
        "Actual behaviour",
        "Current behavior",
        "Symptoms",
        "Description",
        "Problem Statement",
        "Acceptance Criteria",
        "Test Plan",
        "Affected Files",
    ] {
        let body = format!("## {heading}\n\nsomething happened in the parser.\n");
        assert_eq!(trigger("bug: parser", &body, &[]), None, "{heading}");
    }
}

#[test]
fn incident_headings_fire() {
    let body = "## What happened (2026-09-16, robb-pro, launchd, 0.19.59)\n\n- the daemon wedged\n";
    assert!(matches!(
        trigger("daemon stopped heartbeating", body, &[]),
        Some(Trigger::IncidentReport(_))
    ));
}

#[test]
fn reversal_claim_fires_regardless_of_labels_or_author() {
    // The human-filed edge case #8396's Test Plan asks to resolve on purpose:
    // no proposal label, no incident heading, still gated.
    let t = trigger(
        "Make the watchdog restart a wedged daemon",
        "This reverses the documented report-only design.",
        &labels(&["loom:triage"]),
    );
    assert_eq!(t, Some(Trigger::ReversalClaim("reverses the documented")));
}

/// #7855's actual rescoping sentence. The phrase is a trigger because it is
/// the named anti-pattern, not because of who wrote it.
#[test]
fn the_filing_is_the_ruling_is_a_trigger() {
    assert_eq!(
        trigger("x", "the filing is the ruling, so we build it", &[]),
        Some(Trigger::ReversalClaim("the filing is the ruling"))
    );
}

#[test]
fn bare_reversal_words_do_not_fire() {
    for body in [
        "revert the commit that broke CI",
        "override the default timeout with a flag",
        "this is documented in CLAUDE.md",
        "we deliberately kept the old name",
        "reverse the list before rendering",
    ] {
        assert_eq!(trigger("t", body, &[]), None, "{body}");
    }
}

#[test]
fn trigger_order_is_label_then_claim_then_heading() {
    let body = "## What happened\n\nthis reverses the documented design\n";
    assert!(matches!(
        trigger("t", body, &labels(&["loom:hermit"])),
        Some(Trigger::ProposalLabel(_))
    ));
    assert!(matches!(trigger("t", body, &[]), Some(Trigger::ReversalClaim(_))));
}

#[test]
fn headings_skip_fenced_blocks() {
    let body = "```bash\n# What happened here is a shell comment\n```\n\n## Summary\ntext\n";
    assert_eq!(headings(body), vec!["Summary".to_string()]);
    assert_eq!(trigger("t", body, &[]), None);
}

#[test]
fn headings_need_a_space_after_the_hashes() {
    assert_eq!(headings("#nothashtag\n## Real\n"), vec!["Real".to_string()]);
}

#[test]
fn trigger_display_is_parseable() {
    assert_eq!(Trigger::ProposalLabel("loom:hermit".into()).to_string(), "label:loom:hermit");
    assert_eq!(
        Trigger::ReversalClaim("reverses the documented").to_string(),
        "reversal-claim:reverses the documented"
    );
    assert_eq!(
        Trigger::IncidentReport("What happened".into()).to_string(),
        "incident-report:What happened"
    );
}
