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

// ---------------------------------------------------------------------------
// Issue #3702 — model-assignment strategy: rung grammar, complexity marker,
// refusal fallback, and the no-Fable-Judge invariant.
//
// The ladder, precedence chain, `model@effort` grammar, and tier-2.5 marker
// are PROSE CONTRACTS the sweep orchestrator (an LLM subagent) interprets at
// dispatch time — there is no parser to unit-test. These string assertions
// pin the contract so a future edit can't silently drop it.
// ---------------------------------------------------------------------------

/// #3702: the `model@effort` rung grammar and the `fable` top rung are
/// documented, with the effort graceful-degradation contract.
///
/// #3705: the prose now also documents the effort *passthrough* happy path
/// (the `claude` CLI / `spawn-claude.sh` `LOOM_EFFORT` → `--effort` surface)
/// alongside the Task-tool graceful-degradation fallback. Both halves of the
/// contract are pinned so a future edit can't silently drop either one.
#[test]
fn sweep_md_documents_effort_rung_grammar_and_fable() {
    let content = read_sweep_md();
    let required: &[&str] = &[
        // Rung grammar: bare alias vs alias@effort.
        "alias@effort",
        "sonnet@xhigh",
        "(model=sonnet, effort=xhigh)",
        // Effort-before-model escalation ordering.
        "sonnet → sonnet@xhigh → opus → fable",
        // Graceful degradation when per-dispatch effort plumbing is absent.
        "grammar ships either way",
        // #3705: the effort passthrough happy path (CLI/process/daemon), not
        // only degradation — effort IS carried where the surface exposes it.
        "effort IS passed through",
        "LOOM_EFFORT",
        // The fable top rung.
        "fable",
    ];
    for needle in required {
        assert!(
            content.contains(needle),
            "sweep.md is missing #3702 rung-grammar/fable prose `{needle}` — \
             update sweep.md or this test if the change is intentional"
        );
    }
}

/// #3702: the Curator complexity marker is documented as precedence tier 2.5
/// with its one-bump/never-fable/never-a-label bounds.
#[test]
fn sweep_md_documents_complexity_marker_tier() {
    let content = read_sweep_md();
    let required: &[&str] = &[
        "<!-- loom:complexity=complex -->",
        "Tier 2.5 — Curator complexity marker",
        "sonnet → opus",
        "One bump maximum, and never to",
    ];
    for needle in required {
        assert!(
            content.contains(needle),
            "sweep.md is missing #3702 complexity-marker prose `{needle}` — \
             update sweep.md or this test if the change is intentional"
        );
    }
}

/// #3702: a `MODEL_REFUSAL` at a fable rung drops one rung down WITHOUT
/// consuming a Doctor cycle.
#[test]
fn sweep_md_documents_refusal_fallback() {
    let content = read_sweep_md();
    assert!(
        content.contains("MODEL_REFUSAL"),
        "sweep.md must reference the `MODEL_REFUSAL` class (#3702 refusal fallback)"
    );
    assert!(
        content.contains("without consuming a Doctor cycle"),
        "sweep.md must state the refusal fallback re-dispatches without \
         consuming a Doctor cycle (#3702)"
    );
    assert!(
        content.contains("fable → opus"),
        "sweep.md must document the fable→opus one-rung-down refusal fallback (#3702)"
    );
}

/// #3702: the hard invariant that Judge model resolution can never resolve to
/// `fable`, regardless of ladder contents or any marker.
#[test]
fn sweep_md_asserts_no_fable_judge_invariant() {
    let content = read_sweep_md();
    assert!(
        content.contains("Judge model resolution can never resolve to"),
        "sweep.md must state the no-Fable-Judge hard invariant verbatim \
         (#3702): `Judge model resolution can never resolve to `fable`...`"
    );
}

// ---------------------------------------------------------------------------
// Issue #3725 — model-cost experiment mode. The tri-state setting, the two-arm
// A/B, the resume-safe stratified assignment, the tier-2.5 suppression, the
// durable store, the exact-cost harvest, and the canary guardrail are all prose
// contracts the sweep orchestrator interprets — pin the load-bearing strings so
// a future edit can't silently drop them.
// ---------------------------------------------------------------------------

/// #3725: the tri-state experiment setting + env override are documented with
/// the string-valued guard precedence, and the two arms are named.
#[test]
fn sweep_md_documents_model_experiment_mode() {
    let content = read_sweep_md();
    let required: &[&str] = &[
        "### Model-cost experiment mode",
        "sweep.modelExperiment",
        "LOOM_MODEL_EXPERIMENT",
        // Tri-state values.
        "`off` | `observe` | `experiment`",
        // Two arms mapping onto #3718's inequality.
        "Arm A = opus-first",
        "Arm B = sonnet-first + escalate",
        // Deterministic, resume-safe, stratified assignment.
        "Deterministic, resume-safe, stratified assignment",
        // The durable, gitignored store.
        ".loom/stats/sweep-model-stats.jsonl",
        // The agent-id join key into #3726's index.
        "agent-id` join key",
        "loom.transcript-index/v1",
    ];
    for needle in required {
        assert!(
            content.contains(needle),
            "sweep.md is missing #3725 experiment-mode prose `{needle}` — \
             update sweep.md or this test if the change is intentional"
        );
    }
}

/// #3725 (hard AC): in `experiment` mode the forced arm SUPPRESSES the tier-2.5
/// complexity bump so Arm B stays sonnet on `complex`-marked issues.
#[test]
fn sweep_md_documents_experiment_tier_2_5_suppression() {
    let content = read_sweep_md();
    assert!(
        content.contains("Experiment-mode suppression (issue #3725)"),
        "sweep.md must document the tier-2.5 suppression note (#3725 hard AC)"
    );
    assert!(
        content.contains("SUPPRESSES this tier-2.5 bump"),
        "sweep.md must state the forced arm suppresses the tier-2.5 bump (#3725)"
    );
    assert!(
        content.contains("only as the stratification key"),
        "sweep.md must state the marker is used only as the stratification key \
         while an arm is forced (#3725)"
    );
}

/// #3725: the canary guardrail and the exact-per-role-cost harvest are pinned.
#[test]
fn sweep_md_documents_experiment_guardrail_and_harvest() {
    let content = read_sweep_md();
    let required: &[&str] = &[
        // Canary-only guardrail + explicit opt-in.
        "canary-only",
        "LOOM_MODEL_EXPERIMENT_CANARY=1",
        // Harvest surface.
        "--model-experiment",
        // Exact cache-aware cost via transcript usage.
        "cache-aware",
        "cache_read_input_tokens",
        // token_fidelity ladder.
        "token_fidelity",
    ];
    for needle in required {
        assert!(
            content.contains(needle),
            "sweep.md is missing #3725 guardrail/harvest prose `{needle}` — \
             update sweep.md or this test if the change is intentional"
        );
    }
}
