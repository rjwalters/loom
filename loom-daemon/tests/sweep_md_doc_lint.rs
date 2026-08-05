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

/// #3702/#4238: the Curator complexity marker is documented as precedence
/// tier 2.5, resolved through the `sweep.tierModels[<runtime>][<tier>]`
/// tier→model map (PR #4278 replaced the earlier `sonnet → opus` one-tier
/// bump with this map plus an `Exit 3` unconfigured fall-through), with its
/// never-`fable` ceiling bound.
#[test]
fn sweep_md_documents_complexity_marker_tier() {
    let content = read_sweep_md();
    // CONTRACT: the templated marker syntax, the tier-value list, the
    // tier-model map identifier, and the resolver script name are stable
    // identifiers. Keep EXACT. (No concrete model ID is asserted here — the
    // doc-lint stays runtime-neutral per #4278's intent.)
    assert!(
        content.contains("<!-- loom:complexity=<tier> -->"),
        "sweep.md must document the `<!-- loom:complexity=<tier> -->` marker \
         syntax (#3702/#4238 tier 2.5)"
    );
    assert!(
        content.contains("`mechanical` | `routine` | `complex`"),
        "sweep.md must document the three complexity-marker values \
         `mechanical` | `routine` | `complex` (#3702/#4238 tier 2.5)"
    );
    assert!(
        content.contains("sweep.tierModels"),
        "sweep.md must document the `sweep.tierModels` tier→model map that \
         resolves the Tier 2.5 complexity marker (#4238, replacing the old \
         `sonnet → opus` bump)"
    );
    assert!(
        content.contains("resolve-tier-model.sh"),
        "sweep.md must reference `resolve-tier-model.sh` as the resolver for \
         the Tier 2.5 complexity marker (#4238)"
    );
    // CONTRACT (fall-through bound): `Exit 3` is the unconfigured-map
    // fall-through that replaced the one-tier bump as the tier-2.5
    // resolution semantics (#4238) — keep EXACT.
    assert!(
        content.contains("Exit 3"),
        "sweep.md must document the `Exit 3` unconfigured tier/runtime \
         fall-through for the Tier 2.5 complexity marker (#4238)"
    );
    // PROSE (structural): the tier is anchored by its NUMBER (`Tier 2.5`); the
    // "— Curator complexity marker" title text is prose that can be reworded.
    assert!(
        content.contains("Tier 2.5"),
        "sweep.md must document the complexity marker as precedence `Tier 2.5` \
         (#3702) — asserted by tier number, not by exact title wording"
    );
    // CONTRACT (bound): the never-`fable` ceiling is a hard bound. Pin the
    // unique `` Never resolves to `fable` `` bullet lead-in (not a bare
    // `fable` stem — the separate No-Fable-Judge paragraph also contains
    // `fable` and would keep this assertion passing even if the tier-2.5
    // ceiling bullet were deleted).
    assert!(
        content.contains("Never resolves to `fable`"),
        "sweep.md must document the marker's `Never resolves to `fable`` \
         ceiling (#3702/#4238) — the marker can lift one tier and never reach \
         the top rung"
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

// ---------------------------------------------------------------------------
// Issue #4111 — the daemon self-claim check must be a MANDATORY step
// evaluated BEFORE the `loom:building` skip test, not an exception clause
// buried after it. #3823/#3967 both closed green while the child still
// self-skipped its own daemon claim; the doc-lint below is the cheapest
// mechanical "consumer-side" check available for this prose-compliance
// contract — it can't verify an LLM actually follows the ordering at
// runtime, but it CAN verify the ordering the LLM is asked to follow is
// textually correct, and fails loudly if a future edit silently reverts the
// restructure back to an exception-clause shape.
// ---------------------------------------------------------------------------

/// #4111 (AC #2's cheapest mechanical check): "Step 1a — daemon self-claim
/// check" must appear in the "1. Per-issue pre-flight" section BEFORE the
/// `loom:building` skip-test bullet, so the marker check is structurally a
/// precondition of that bullet rather than a footnote appended after it.
#[test]
fn sweep_md_step_1a_self_claim_check_precedes_loom_building_skip_bullet() {
    let content = read_sweep_md();

    // CONTRACT: "Step 1a" is the stable anchor for the mandatory daemon
    // self-claim check (#4111); the skip-test bullet's opening clause is the
    // stable anchor for the loom:building rule it must precede. Both are
    // load-bearing identifiers — a rename of either without updating this
    // test is exactly the drift this doc-lint exists to catch.
    let step_1a_pos = content.find("Step 1a").unwrap_or_else(|| {
        panic!(
            "sweep.md is missing the `Step 1a` daemon self-claim check anchor \
             — #4111 requires the marker check be a MANDATORY, separately \
             numbered pre-flight step, not an exception clause folded into \
             the loom:building skip bullet"
        )
    });
    let skip_bullet_pos = content
        .find("If the issue already has `loom:building`")
        .unwrap_or_else(|| {
            panic!(
                "sweep.md is missing the `loom:building` skip-test bullet — \
                 the #3823-era pre-flight rule this doc-lint anchors to"
            )
        });

    assert!(
        step_1a_pos < skip_bullet_pos,
        "sweep.md's `Step 1a` daemon self-claim check (byte offset {step_1a_pos}) \
         must appear BEFORE the `loom:building` skip-test bullet (byte offset \
         {skip_bullet_pos}) in the \"1. Per-issue pre-flight\" section — #4111's \
         entire fix is making the marker check evaluate first. If this ever \
         regresses (Step 1a moved after the skip bullet, or collapsed back \
         into an inline exception clause), the marker becomes prose-optional \
         again exactly as it was when #4111 was filed."
    );

    // CONTRACT: the check must be stated as MANDATORY, not advisory — a
    // reword that softens "MANDATORY" to something optional-sounding would
    // reintroduce the exact compliance gap #4111 fixed.
    assert!(
        content.contains("MANDATORY"),
        "sweep.md's Step 1a daemon self-claim check must be stated as \
         MANDATORY (#4111) — the prior #3823 phrasing was a non-mandatory \
         exception clause and was silently skipped by a daemon-dispatched \
         child in production"
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

// ---------------------------------------------------------------------------
// Issue #4505 — the aggressive `all`-sentinel `loom:blocked` taxonomy row must
// not strip `loom:blocked` on unparseable blocker text that is actually an
// explicit hold/defer instruction (e.g. "hold until <milestone>"). The
// no-parseable-dependency branch must check for hold/defer phrasing BEFORE
// falling through to the fast/sloppy remove-and-attempt behavior, and route a
// match to a new `would skip (explicit hold: "<phrase>")` planned action
// instead of unblocking.
// ---------------------------------------------------------------------------

/// #4505: the `loom:blocked` row of the aggressive candidate taxonomy table
/// documents the hold/defer-phrase check ahead of the fast/sloppy fallback,
/// and the fast/sloppy fallback itself remains documented as the behavior for
/// genuinely empty/unparseable blocker text (unchanged).
#[test]
fn sweep_md_documents_explicit_hold_check_in_blocked_taxonomy_row() {
    let content = read_sweep_md();

    // CONTRACT: the hold/defer phrase vocabulary the taxonomy row instructs
    // the orchestrator to match (case-insensitive, instruction-shaped
    // fragments — not bare "hold"/"wait" substrings). Keep EXACT: these are
    // the specific fragments #4505's acceptance criteria enumerate.
    let hold_defer_phrases: &[&str] = &[
        "hold until",
        "wait until",
        "defer",
        "not before",
        "do not start",
    ];
    for phrase in hold_defer_phrases {
        assert!(
            content.contains(phrase),
            "sweep.md's `loom:blocked` aggressive-taxonomy row is missing the \
             hold/defer phrase `{phrase}` (#4505) — the no-parseable-dependency \
             branch must check for hold/defer instructions before falling \
             through to the fast/sloppy remove-and-attempt behavior"
        );
    }

    // CONTRACT: the new routing verb for a hold/defer match — must NOT remove
    // `loom:blocked` and must NOT build. Keep EXACT.
    assert!(
        content.contains(r#"skip with `explicit hold: "<quoted phrase>"`"#),
        "sweep.md's `loom:blocked` taxonomy row must route a hold/defer-phrase \
         match to `explicit hold: \"<quoted phrase>\"` (#4505) — this is what \
         prevents the fast/sloppy fallback from overriding an author's \
         explicit hold instruction"
    );

    // CONTRACT: the fast/sloppy fallback for genuinely empty/unparseable text
    // (no hold/defer phrasing) must remain documented unchanged.
    assert!(
        content.contains("remove `loom:blocked` and attempt anyway (fast/sloppy)"),
        "sweep.md's `loom:blocked` taxonomy row must retain the fast/sloppy \
         remove-and-attempt fallback for truly empty/unparseable blocker text \
         with no hold/defer phrasing (#4505 — additive, not a regression on \
         the existing default path)"
    );
}

// ---------------------------------------------------------------------------
// Issue #4670 — GraphQL-exhaustion REST fallback for Mode B issue discovery and
// Mode C PR discovery. GraphQL (`gh issue list` / `gh pr list` / `gh label
// list`) and REST (`gh api repos/{owner}/{repo}/…`) draw on independent quotas,
// so an exhausted GraphQL budget must not strand candidate resolution while
// thousands of REST requests remain. The fallback is orchestrator-executed
// prose (no script backs Mode B/C candidate discovery), so this doc-lint is the
// only mechanical guard against a future edit silently deleting it.
//
// Per #3877: the detection signatures, REST endpoints, jq filters, and the
// local repo-resolution command are CONTRACT (pinned EXACT — they are the
// literal strings an orchestrator must reproduce); section lead-ins and the
// fail-safe/ladder semantics are PROSE (pinned structurally / via tolerant
// phrasing sets).
// ---------------------------------------------------------------------------

/// #4670: BOTH Mode B (issue discovery) and Mode C (PR discovery) carry a
/// GraphQL-exhaustion fallback section — one before the `### Mode C` heading,
/// one after. A single shared section in only one mode is the regression this
/// ordering check exists to catch.
#[test]
fn sweep_md_documents_graphql_exhaustion_fallback_in_both_mode_b_and_mode_c() {
    let content = read_sweep_md();

    // PROSE (structural presence): the bold lead-in name is the anchor; the
    // trailing "(REST issue discovery, #4670)" / "(REST PR discovery, #4670)"
    // qualifiers are editorial and deliberately not pinned.
    let occurrences = content.matches("GraphQL-exhaustion fallback").count();
    assert!(
        occurrences >= 2,
        "sweep.md must document a `GraphQL-exhaustion fallback` in BOTH the \
         Mode B (issue discovery) and Mode C (PR discovery) sections (#4670); \
         found {occurrences} occurrence(s)"
    );

    let mode_c_heading = content
        .find("### Mode C — PR-set mode")
        .expect("sweep.md is missing the `### Mode C — PR-set mode` heading");
    let first_fallback = content
        .find("GraphQL-exhaustion fallback")
        .expect("checked non-empty above");
    let last_fallback = content
        .rfind("GraphQL-exhaustion fallback")
        .expect("checked non-empty above");

    assert!(
        first_fallback < mode_c_heading,
        "sweep.md's first `GraphQL-exhaustion fallback` (byte offset \
         {first_fallback}) must sit in the Mode B section, i.e. BEFORE the \
         `### Mode C — PR-set mode` heading (byte offset {mode_c_heading}) — \
         #4670 requires issue discovery to have its own REST fallback"
    );
    assert!(
        last_fallback > mode_c_heading,
        "sweep.md's last `GraphQL-exhaustion fallback` (byte offset \
         {last_fallback}) must sit in the Mode C section, i.e. AFTER the \
         `### Mode C — PR-set mode` heading (byte offset {mode_c_heading}) — \
         #4670 requires PR discovery to have its own REST fallback"
    );
}

/// #4670: the exhaustion-detection signature table is pinned EXACT. It mirrors
/// `defaults/scripts/check-duplicate.sh`'s `is_rate_limit_error()` (itself
/// mirrored from `loom-daemon/src/rate_limit_breaker.rs`'s
/// `RATE_LIMIT_SIGNATURES`) — deriving a new, subtly different signature set is
/// exactly the drift this pins against. Note that `api rate limit exceeded` and
/// `api rate limit already exceeded` are NOT substrings of each other (the word
/// "already" breaks the contiguous match), so both must be present.
#[test]
fn sweep_md_pins_rate_limit_signature_table() {
    let content = read_sweep_md();

    // CONTRACT: the five signature strings. Keep EXACT.
    let signatures: &[&str] = &[
        "api rate limit exceeded",
        "api rate limit already exceeded",
        "secondary rate limit",
        "abuse detection mechanism",
        "was submitted too quickly",
    ];
    for signature in signatures {
        assert!(
            content.contains(signature),
            "sweep.md's GraphQL-exhaustion detection is missing the rate-limit \
             signature `{signature}` (#4670) — the table must mirror \
             check-duplicate.sh's is_rate_limit_error() / \
             rate_limit_breaker.rs's RATE_LIMIT_SIGNATURES, not a new one"
        );
    }

    // CONTRACT: the provenance pointers keep future editors on the shared
    // table instead of re-deriving one. Keep EXACT (file/symbol names).
    for needle in ["is_rate_limit_error()", "RATE_LIMIT_SIGNATURES"] {
        assert!(
            content.contains(needle),
            "sweep.md must cite `{needle}` as the source of the rate-limit \
             signature table (#4670)"
        );
    }
}

/// #4670: the REST endpoints, pagination parameters, and the PR-exclusion jq
/// filter that make the fallback actually executable.
#[test]
fn sweep_md_pins_rest_fallback_endpoints_and_pagination() {
    let content = read_sweep_md();

    // CONTRACT: REST paths, pagination flags/params, and the jq filter are
    // literal strings the orchestrator reproduces. Keep EXACT.
    let contract_needles: &[&str] = &[
        "repos/{owner}/{repo}/issues",   // Mode B REST listing
        "repos/{owner}/{repo}/pulls",    // Mode C REST listing
        "repos/{owner}/{repo}/labels",   // unknown-label guard REST rung
        "--paginate",                    // paginated listing
        "per_page=100",                  // explicit limit (never REST's 30)
        "select(.pull_request == null)", // /issues returns PRs too
    ];
    for needle in contract_needles {
        assert!(
            content.contains(needle),
            "sweep.md's REST fallback is missing contract token `{needle}` \
             (#4670) — update sweep.md or this test if the change is intentional"
        );
    }
}

/// #4670 (AC: no second GraphQL call to resolve the repo): owner/repo must be
/// resolved locally, and `gh repo view --json nameWithOwner` must be explicitly
/// called out as the anti-pattern (it is itself GraphQL-backed — #4659).
#[test]
fn sweep_md_forbids_graphql_repo_resolution_in_rest_fallback() {
    let content = read_sweep_md();

    // CONTRACT: the local resolution command. Keep EXACT — it is the whole
    // point of the AC (zero API calls, survives total GraphQL outage).
    assert!(
        content.contains("git remote get-url origin"),
        "sweep.md's REST fallback must resolve owner/repo locally from \
         `git remote get-url origin` (#4670/#4659) — a GraphQL-backed lookup \
         fails before the REST fallback is ever attempted"
    );

    // PROSE (structural / tolerant): the prohibition on the GraphQL-backed
    // `gh repo view` lookup is stated twice with two phrasings (Mode B: "Do
    // **not** call …"; Mode C: "**never** …"). Accept either so a reword of one
    // survives while deleting BOTH — i.e. the warning truly gone — still fails.
    let prohibition_phrases: &[&str] = &[
        "Do **not** call `gh repo view --json nameWithOwner`",
        "never** `gh repo view --json nameWithOwner`",
        "not** call `gh repo view",
        "NOT `gh repo view --json nameWithOwner`",
    ];
    assert!(
        prohibition_phrases.iter().any(|p| content.contains(p)),
        "sweep.md must warn against `gh repo view --json nameWithOwner` for \
         repo resolution in the REST fallback (#4670/#4659) — asserted via a \
         tolerant phrasing set"
    );
}

/// #4670 (AC: non-rate-limit failures keep fail-safe behavior): auth/network
/// errors must NOT be mistaken for quota exhaustion and silently retried.
#[test]
fn sweep_md_documents_non_rate_limit_failsafe() {
    let content = read_sweep_md();

    // PROSE (structural / tolerant): the "only a rate-limit signature triggers
    // the fallback" semantic is wording; accept equivalent phrasings so a
    // reword survives while a deletion of the guard still fails.
    let failsafe_phrases: &[&str] = &[
        "NOT exhaustion",
        "not exhaustion",
        "is not a rate limit",
        "not be mistaken for quota exhaustion",
    ];
    assert!(
        failsafe_phrases.iter().any(|p| content.contains(p)),
        "sweep.md must state that non-rate-limit failures (auth, network, 404) \
         are NOT exhaustion and must keep today's fail-safe behavior (#4670) — \
         asserted via a tolerant phrasing set"
    );
}

/// #4670 (AC: preserve the unknown-label safety rule): the label guard degrades
/// GraphQL → REST → `.github/labels.yml`, with the YAML as the LAST rung, not
/// the first.
#[test]
fn sweep_md_documents_label_guard_rest_before_yaml_ladder() {
    let content = read_sweep_md();

    // CONTRACT: the degraded-fallback file path stays exact (it is a real
    // path the orchestrator reads).
    assert!(
        content.contains(".github/labels.yml"),
        "sweep.md must retain `.github/labels.yml` as the degraded label \
         fallback (#4670 keeps it, demoted to the last rung)"
    );

    // PROSE (structural / tolerant): the ordering semantic — REST is tried
    // before the YAML subset — stated in both the Mode B offline-fallback
    // paragraph and the fallback ladder item.
    let ladder_phrases: &[&str] = &[
        "only when the REST read also fails",
        "only if REST fails too",
        "last* rung",
        "last rung",
    ];
    assert!(
        ladder_phrases.iter().any(|p| content.contains(p)),
        "sweep.md must document `.github/labels.yml` as the LAST rung of the \
         GraphQL → REST → YAML label ladder (#4670): the live REST label set is \
         preferred because the YAML is only the Loom-managed subset — asserted \
         via a tolerant phrasing set"
    );
}

/// #4505: the dry-run per-candidate planned-action enumeration documents the
/// new `would skip (explicit hold: "<phrase>")` action alongside the other
/// aggressive-mode `would ...` actions.
#[test]
fn sweep_md_documents_explicit_hold_planned_action() {
    let content = read_sweep_md();

    // CONTRACT: the exact planned-action string a dry-run render must use for
    // an explicit-hold candidate. Keep EXACT — this is the confirmation-gate
    // rendering an operator reads before anything mutates.
    assert!(
        content.contains(r#"would skip (explicit hold: "<phrase>")"#),
        "sweep.md's per-candidate planned-action enumeration is missing \
         `would skip (explicit hold: \"<phrase>\")` (#4505) — the dry-run plan \
         and confirmation gate must render explicit-hold candidates distinctly \
         from a generic `would unblock (...), build`"
    );
}

// ---------------------------------------------------------------------------
// Issue #5208 — the `all` sentinel's orphaned-claim recovery pass must surface a
// VISIBLE, operator-actionable warning at the candidate-set / confirmation-gate
// output when `recover-orphaned-shepherds.sh` cannot run (no loom-daemon binary
// resolved, or any other non-zero exit), instead of only logging a swallowed
// best-effort failure. On a host that never built/installed loom-daemon this
// pass silently did nothing, letting a stale `loom:building` claim mask a
// buildable issue with no operator signal. The two failure classes must be
// distinguished ("no binary resolved" is build/install-actionable; any other
// exit is surfaced verbatim), and a clean (exit 0) probe must add NO warning so
// the common path stays noise-free.
// ---------------------------------------------------------------------------

/// #5208: the "Orphaned-claim recovery pass" bullet documents a read-only
/// capability pre-probe that surfaces a distinct, operator-actionable `⚠`
/// warning at the confirmation gate when recovery cannot run, distinguishing
/// "no binary resolved" from any other non-zero exit, and stays silent on the
/// exit-0 common path.
#[test]
fn sweep_md_documents_orphan_recovery_gate_warning() {
    let content = read_sweep_md();

    // CONTRACT: the script's exact exit-1 signature the pre-probe greps for to
    // classify the "no binary resolved" case. This mirrors
    // `defaults/scripts/recover-orphaned-shepherds.sh`'s error output — deriving
    // a subtly different substring here is exactly the drift this pins against.
    assert!(
        content.contains("no loom-daemon binary could be resolved"),
        "sweep.md's orphaned-claim recovery pass must key its \
         operator-actionable warning off the script's exact \
         `no loom-daemon binary could be resolved` exit-1 signature (#5208) — \
         so it can distinguish that case from any other non-zero exit"
    );

    // CONTRACT: the two distinct operator-facing warning lead-ins. Keep EXACT —
    // these are the strings an operator reads at the confirmation gate, and the
    // whole point of #5208 is that they are SURFACED (not swallowed) and that
    // the two failure classes render differently.
    assert!(
        content.contains("⚠ orphan-claim recovery unavailable: no loom-daemon binary resolved"),
        "sweep.md must surface the operator-actionable \
         `⚠ orphan-claim recovery unavailable: no loom-daemon binary resolved` \
         warning at the confirmation gate when the recovery pre-probe finds no \
         usable loom-daemon binary (#5208) — not only log a swallowed best-effort \
         non-zero exit"
    );
    assert!(
        content.contains("⚠ orphan-claim recovery pre-probe failed"),
        "sweep.md must surface a DISTINCT `⚠ orphan-claim recovery pre-probe \
         failed` warning for any non-`no-binary` recovery failure (#5208) — the \
         two classes must be distinguishable so the operator knows which remedy \
         applies (build/install vs. diagnose the quoted error)"
    );

    // PROSE (structural / tolerant): the exit-0 no-noise guarantee must stay
    // documented so a future edit can't make the common path warn spuriously.
    let no_noise_phrases: &[&str] = &[
        "Emit no warning",
        "no spurious annotation on a healthy host",
        "a clean pre-probe (exit 0) adds nothing",
    ];
    assert!(
        no_noise_phrases.iter().any(|p| content.contains(p)),
        "sweep.md must state that a successful (exit 0) orphaned-claim recovery \
         pre-probe produces NO warning (#5208 edge case) — the new visibility \
         must not create noise on the common path; asserted via a tolerant \
         phrasing set"
    );
}
