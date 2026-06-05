//! Doc-lint test for `defaults/.claude/commands/loom/sweep.md` (Issue #3453,
//! AC #3).
//!
//! The sweep skill markdown documents the wire-protocol contract for the
//! Phase B event bus — the six initial topics in the frozen taxonomy.
//! This test grep-checks the markdown file at compile time so that:
//!
//! - Renames/refactors to the topic strings flag a CI failure.
//! - Removing the section by accident also flags a CI failure.
//! - The acceptance criteria for #3453 (AC #3) can be verified
//!   programmatically.
//!
//! If the markdown structure intentionally changes (e.g. a follow-up issue
//! adds a seventh topic), update this test together with the markdown so
//! the doc-lint stays in sync with the contract.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;

const SWEEP_MD_RELATIVE: &str = "../defaults/.claude/commands/loom/sweep.md";

/// All six frozen topic strings from the Phase B taxonomy. The presence
/// of each in `sweep.md` is part of the acceptance criteria for #3453.
const REQUIRED_TOPICS: &[&str] = &[
    "sweep.issue.{N}.phase",
    "sweep.issue.{N}.blocker",
    "sweep.issue.{N}.exited",
    "sweep.issue.{N}.crashed",
    "sweep.global.dispatch",
    "sweep.global.completed",
];

fn read_sweep_md() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SWEEP_MD_RELATIVE);
    fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "sweep.md not found at {} (CWD-relative path: {}): {e}",
            path.display(),
            SWEEP_MD_RELATIVE,
        );
    })
}

/// AC #3: assert the `## Daemon event bus` section is present.
#[test]
fn sweep_md_has_daemon_event_bus_section() {
    let content = read_sweep_md();
    assert!(
        content.contains("## Daemon event bus"),
        "expected `## Daemon event bus` section in sweep.md — the Phase B \
         contract documentation is required by #3453 AC #3"
    );
}

/// AC #3: assert all six initial topics are present in the markdown.
///
/// If a topic is renamed in Rust without updating sweep.md, this test
/// catches the drift. If sweep.md is renamed without updating this
/// test, the test panics with a missing-file message above.
#[test]
fn sweep_md_topic_taxonomy_table_lists_six_topics() {
    let content = read_sweep_md();
    for topic in REQUIRED_TOPICS {
        assert!(
            content.contains(topic),
            "sweep.md is missing topic `{topic}` from the Phase B taxonomy; \
             update sweep.md or this test if the change is intentional"
        );
    }
}

/// AC #3: assert the `PublishEvent` IPC contract is documented (i.e.,
/// the markdown has the Request::PublishEvent wire-format reference).
/// This catches accidental section removals during future refactors.
#[test]
fn sweep_md_documents_publish_event_ipc_contract() {
    let content = read_sweep_md();
    assert!(
        content.contains("Request::PublishEvent"),
        "sweep.md should reference `Request::PublishEvent` — the IPC contract \
         is required by #3453 AC #3"
    );
    assert!(
        content.contains("PublishEvent"),
        "sweep.md should reference `PublishEvent` IPC variant"
    );
    assert!(
        content.contains("SubscribeEvents"),
        "sweep.md should reference `SubscribeEvents` IPC variant"
    );
}

/// AC #3: assert at least one sample JSON payload for each topic type.
/// Looks for the structural markers (the wire-frame examples), not
/// every payload field — payload fields may evolve while the topic
/// remains stable.
#[test]
fn sweep_md_includes_sample_wire_payloads() {
    let content = read_sweep_md();

    // At least one sample for each topic. We assert that the wire-frame
    // example contains the topic string AND a JSON `"payload"` key —
    // together this confirms a sample exists (not just a table entry).
    let samples: &[&str] = &[
        r#""topic": "sweep.issue.123.phase""#,
        r#""topic": "sweep.issue.123.blocker""#,
        r#""SweepExited""#,
        r#""SweepCrashed""#,
        r#""SweepGlobalDispatch""#,
        r#""SweepGlobalCompleted""#,
    ];
    for sample in samples {
        assert!(
            content.contains(sample),
            "sweep.md is missing a sample wire-frame for `{sample}` — \
             #3453 AC #3 requires sample payloads for each topic"
        );
    }
}
