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
//! ---------------------------------------------------------------------------
//! Assertion classification (#3877 — prose vs. contract)
//! ---------------------------------------------------------------------------
//! Every `contains()` assertion below is tagged PROSE or CONTRACT:
//!
//! - **PROSE** — a section title / bold lead-in / wording that legitimately
//!   gets edited. These assert STRUCTURE/PRESENCE (heading prefix, a stable
//!   technical token, or a tolerant any-of set of phrasings) rather than exact
//!   wording, so an editorial reword that keeps the concept does NOT red-main
//!   `main` — only a deletion does. (Precedent: #3830→#3834, #3856→#3863 both
//!   red-mained on pinned prose titles.)
//! - **CONTRACT** — a stable identifier that MUST NOT drift: event topic
//!   strings, IPC variant names, wire-payload field names, config keys, env
//!   vars, CLI flags, file paths, schema ids, the complexity marker syntax,
//!   and the escalation-ladder ordering. These stay EXACT — their exactness is
//!   the value.
//!
//! If the markdown structure intentionally changes (e.g. a follow-up issue
//! adds a seventh topic), update this test together with the markdown so
//! the doc-lint stays in sync with the contract.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;

const SWEEP_MD_RELATIVE: &str = "../defaults/.claude/commands/loom/sweep.md";

/// CONTRACT: all six frozen topic strings from the Phase B taxonomy. These are
/// wire identifiers frozen for v0.10.0 — a rename is a real contract break, not
/// an editorial edit. Keep EXACT.
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
    // PROSE (structural presence): the section is anchored by its `## ` heading
    // + stable name prefix `Daemon event bus`; the check tolerates the appended
    // `(Phase B …)` ref suffix and fails only if the whole section is deleted.
    // There is no numeric anchor here, so the stable name prefix is the anchor.
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
    // CONTRACT: frozen topic strings (see REQUIRED_TOPICS). Keep EXACT.
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
    // CONTRACT: IPC request/variant identifiers are wire-protocol names. Keep
    // EXACT — a rename here is a real contract break.
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

    // CONTRACT: each sample pins a wire-frame topic string or a Rust event
    // variant name — both are stable wire identifiers. We deliberately do NOT
    // pin every payload field (those may evolve); the topic/variant name is the
    // contract. Keep these EXACT.
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
// are behavioral contracts the sweep orchestrator (an LLM subagent) interprets
// at dispatch time — there is no parser to unit-test. Per #3877 these are split
// into CONTRACT identifiers (grammar tokens, ladders, env vars, flags, markers,
// class names — pinned EXACT) and PROSE sentences (pinned structurally: a
// stable technical token or a tolerant any-of phrasing set) so an editorial
// reword can't red-main main while a deletion still fails.
// ---------------------------------------------------------------------------

/// #3702/#3705: the `model@effort` rung grammar, the `fable` top rung, the
/// effort passthrough happy path, and the Task-tool graceful-degradation
/// fallback are all documented.
#[test]
fn sweep_md_documents_effort_rung_grammar_and_fable() {
    let content = read_sweep_md();
    // CONTRACT: grammar tokens, the escalation-ladder ordering, the
    // effort-plumbing identifiers (env var + CLI flag), and the `fable` rung
    // name. These are stable identifiers the orchestrator + spawn-claude.sh
    // consume — drift is a real breakage. Keep EXACT.
    let contract_needles: &[&str] = &[
        "alias@effort",                         // rung grammar token
        "sonnet@xhigh",                         // grammar example / example rung
        "(model=sonnet, effort=xhigh)",         // grammar semantic expansion
        "sonnet → sonnet@xhigh → opus → fable", // effort-before-model ladder order
        "LOOM_EFFORT",                          // #3705 passthrough env
        "--effort",                             // #3705 CLI flag carrying effort
        "fable",                                // the top rung name
    ];
    for needle in contract_needles {
        assert!(
            content.contains(needle),
            "sweep.md is missing #3702/#3705 rung-grammar/fable contract token \
             `{needle}` — update sweep.md or this test if the change is intentional"
        );
    }

    // PROSE (structural): the Task-tool graceful-degradation half (#3705) must
    // stay documented. Anchor on the section-local fragment `degrades cleanly to
    // bare` (#3879) rather than a bare `degrade` stem: `degrade` also appears in
    // unrelated `merge-pr.sh --auto` prose, so a bare-stem anchor would still
    // pass on incidental matches even if the effort-degradation paragraph were
    // deleted (violating #3877 AC3). This fragment is unique to that paragraph
    // while staying prose-tolerant (the surrounding sentence can still be
    // reworded).
    assert!(
        content.contains("degrades cleanly to bare"),
        "sweep.md must document the Task-tool effort graceful-degradation \
         fallback (#3705) — anchored on the section-local fragment `degrades \
         cleanly to bare`, unique to that paragraph (#3879)"
    );
}

/// #3702: the Curator complexity marker is documented as precedence tier 2.5
/// with its one-bump / never-fable bound.
#[test]
fn sweep_md_documents_complexity_marker_tier() {
    let content = read_sweep_md();
    // CONTRACT: the marker syntax and the `sonnet → opus` one-tier bump are
    // stable identifiers. Keep EXACT.
    assert!(
        content.contains("<!-- loom:complexity=complex -->"),
        "sweep.md must document the `<!-- loom:complexity=complex -->` marker \
         syntax (#3702 tier 2.5)"
    );
    assert!(
        content.contains("sonnet → opus"),
        "sweep.md must document the `sonnet → opus` one-tier bump (#3702 tier 2.5)"
    );
    // PROSE (structural): the tier is anchored by its NUMBER (`Tier 2.5`); the
    // "— Curator complexity marker" title text is prose that can be reworded.
    assert!(
        content.contains("Tier 2.5"),
        "sweep.md must document the complexity marker as precedence `Tier 2.5` \
         (#3702) — asserted by tier number, not by exact title wording"
    );
    // CONTRACT (bound): the never-to-`fable` ceiling is a hard bound; the
    // backtick-wrapped `fable` makes it identifier-shaped. Pin the tight
    // `never to `fable`` fragment (not the full "One bump maximum, …" sentence)
    // so a reword of the lead-in survives while removing the ceiling fails.
    assert!(
        content.contains("never to `fable`"),
        "sweep.md must document the marker's never-to-`fable` ceiling (#3702) — \
         the marker can lift one tier and never reach the top rung"
    );
}

/// #3702: a `MODEL_REFUSAL` at a fable rung drops one rung down WITHOUT
/// consuming a Doctor cycle.
#[test]
fn sweep_md_documents_refusal_fallback() {
    let content = read_sweep_md();
    // CONTRACT: the error class name and the one-rung-down `fable → opus`
    // fallback are stable identifiers. Keep EXACT.
    assert!(
        content.contains("MODEL_REFUSAL"),
        "sweep.md must reference the `MODEL_REFUSAL` class (#3702 refusal fallback)"
    );
    assert!(
        content.contains("fable → opus"),
        "sweep.md must document the fable→opus one-rung-down refusal fallback (#3702)"
    );
    // PROSE (structural / tolerant): the "does not cost a Doctor cycle" semantic
    // is wording; accept equivalent phrasings so a reword survives while a
    // deletion of the no-cost guarantee still fails.
    let no_cost_phrases: &[&str] = &[
        "without consuming a Doctor cycle",
        "without spending a Doctor cycle",
        "does not consume a Doctor cycle",
        "not consume a Doctor cycle",
    ];
    assert!(
        no_cost_phrases.iter().any(|p| content.contains(p)),
        "sweep.md must state the refusal fallback re-dispatches without \
         consuming a Doctor cycle (#3702) — asserted via a tolerant phrasing set"
    );
}

/// #3702: the hard invariant that Judge model resolution can never resolve to
/// `fable`, regardless of ladder contents or any marker.
#[test]
fn sweep_md_asserts_no_fable_judge_invariant() {
    let content = read_sweep_md();
    // PROSE (structural / tolerant): the no-Fable-Judge invariant is stated in
    // two places with two phrasings ("Judge model resolution can never resolve
    // to" and "Judge dispatch never resolves to"). Accept either so a reword of
    // one survives, while a deletion of BOTH — i.e. the invariant truly gone —
    // still fails. The `fable` exclusion itself is a hard contract.
    let invariant_phrases: &[&str] = &[
        "Judge model resolution can never resolve to",
        "Judge dispatch never resolves to",
        "Judge model would ever be `fable`",
    ];
    assert!(
        invariant_phrases.iter().any(|p| content.contains(p)),
        "sweep.md must state the no-Fable-Judge hard invariant (#3702): Judge \
         model resolution can never resolve to `fable` — asserted via a tolerant \
         phrasing set"
    );
}

// ---------------------------------------------------------------------------
// Issue #3725 — model-cost experiment mode. The tri-state setting, the two-arm
// A/B, the resume-safe stratified assignment, the tier-2.5 suppression, the
// durable store, the exact-cost harvest, and the canary guardrail are behavioral
// contracts the sweep orchestrator interprets. Per #3877, config keys / env
// vars / flags / paths / schema ids / field names are pinned EXACT (CONTRACT),
// while bold lead-ins and wording are pinned structurally (PROSE).
// ---------------------------------------------------------------------------

/// #3725: the tri-state experiment setting + env override are documented with
/// the string-valued guard precedence, and the two arms are named.
#[test]
fn sweep_md_documents_model_experiment_mode() {
    let content = read_sweep_md();
    // CONTRACT: config key, env var, the exact tri-state value grammar, the arm
    // model mappings, the durable-store path, the join key, and the transcript
    // schema id are all stable identifiers. Keep EXACT.
    let contract_needles: &[&str] = &[
        "sweep.modelExperiment",               // config key
        "LOOM_MODEL_EXPERIMENT",               // env var
        "`off` | `observe` | `experiment`",    // tri-state value grammar
        "Arm A = opus-first",                  // arm→model mapping
        "Arm B = sonnet-first + escalate",     // arm→model mapping
        ".loom/stats/sweep-model-stats.jsonl", // durable store path
        "agent-id` join key",                  // harvest join key
        "loom.transcript-index/v1",            // #3726 transcript schema id
    ];
    for needle in contract_needles {
        assert!(
            content.contains(needle),
            "sweep.md is missing #3725 experiment-mode contract token `{needle}` \
             — update sweep.md or this test if the change is intentional"
        );
    }
    // PROSE (structural): the assignment property is documented via a bold
    // lead-in ("Deterministic, resume-safe, stratified assignment.") that gets
    // edited. Anchor on the stable technical token `stratified` (matches
    // "stratified"/"stratification") so a reword survives and a deletion fails.
    assert!(
        content.contains("stratified"),
        "sweep.md must document the deterministic, resume-safe, stratified arm \
         assignment (#3725) — anchored structurally on `stratified`"
    );

    // PROSE (structural presence): the subsection heading is anchored by its
    // `### ` prefix + stable name; the `(sweep.modelExperiment / …)` suffix is
    // tolerated. Fails only if the whole subsection is deleted.
    assert!(
        content.contains("### Model-cost experiment mode"),
        "sweep.md must retain the `### Model-cost experiment mode` subsection \
         (#3725) — asserted by heading prefix, tolerating the appended ref suffix"
    );
}

/// #3725 (hard AC): in `experiment` mode the forced arm SUPPRESSES the tier-2.5
/// complexity bump so Arm B stays sonnet on `complex`-marked issues.
#[test]
fn sweep_md_documents_experiment_tier_2_5_suppression() {
    let content = read_sweep_md();
    // CONTRACT (prefix-tolerant): the suppression note is anchored by its
    // `Experiment-mode suppression (issue #3725` title + issue ref. The prefix
    // match (no closing paren) tolerates appended issue refs — e.g. a future
    // edit turning `(issue #3725)` into `(issue #3725, #NNNN)`. See #3833/#3837
    // for why exact-paren literals red main on doc edits.
    assert!(
        content.contains("Experiment-mode suppression (issue #3725"),
        "sweep.md must document the tier-2.5 suppression note (#3725 hard AC; \
         tolerates appended issue refs)"
    );
    // CONTRACT: the caps `SUPPRESSES` verb and the `tier-2.5` target are the
    // load-bearing tokens of the suppression semantic. Assert both present
    // (structural two-token check) rather than the exact "SUPPRESSES this
    // tier-2.5 bump" phrase whose connective wording can drift.
    assert!(
        content.contains("SUPPRESSES") && content.contains("tier-2.5"),
        "sweep.md must state the forced arm SUPPRESSES the tier-2.5 bump (#3725)"
    );
    // PROSE (structural): the "marker used only as the stratification key while
    // an arm is forced" semantic is wording; anchor on the stable noun phrase
    // `stratification key` (drop the brittle "only as the" lead-in).
    assert!(
        content.contains("stratification key"),
        "sweep.md must state the marker is used as the stratification key while \
         an arm is forced (#3725) — anchored on the `stratification key` phrase"
    );
}

/// #3725: the canary guardrail and the exact-per-role-cost harvest are pinned.
#[test]
fn sweep_md_documents_experiment_guardrail_and_harvest() {
    let content = read_sweep_md();
    // CONTRACT: the canary env-var+value, the harvest flag, and the cache
    // token-usage field names are stable identifiers. `canary-only` and
    // `cache-aware` are distinctive hyphenated terms naming the guardrail /
    // costing property. Keep EXACT.
    let contract_needles: &[&str] = &[
        "canary-only",                    // guardrail term
        "LOOM_MODEL_EXPERIMENT_CANARY=1", // canary env var + value
        "--model-experiment",             // harvest CLI flag
        "cache-aware",                    // costing property term
        "cache_read_input_tokens",        // usage-block field name
        "token_fidelity",                 // record field name
    ];
    for needle in contract_needles {
        assert!(
            content.contains(needle),
            "sweep.md is missing #3725 guardrail/harvest contract token \
             `{needle}` — update sweep.md or this test if the change is intentional"
        );
    }
}
