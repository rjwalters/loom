/**
 * judge-fanout-workflow.js — DESIGN SKETCH (issue #3739, exploration only)
 * =========================================================================
 *
 * A Claude Code Dynamic Workflow that reviews ONE pull request by fanning out
 * dimension-scoped reviewers, running a typed adversarial "verify" pass, and
 * reducing to a single schema-shaped verdict.
 *
 * STATUS: DESIGN SKETCH — NOT WIRED INTO PRODUCTION.
 *   - This file is intentionally placed under defaults/scripts/experiments/,
 *     which is NOT a discovered `workflows/` directory, so Claude Code will not
 *     auto-load it. It is inert until a human deliberately copies/points a
 *     top-level session at it (see defaults/scripts/experiments/README.md).
 *   - It is NOT referenced by defaults/.claude/commands/loom/sweep.md or
 *     defaults/roles/judge.md. The production judge path is untouched.
 *   - The runnable prototype + measured comparison are DEFERRED to a follow-up
 *     (see docs/research/dynamic-workflows-evaluation.md → "What is deferred").
 *
 * SUBSTRATE: in-session, single-token, exactly ONE level deep (#3289).
 *   - `agent()` calls below are DIRECT — this workflow never calls `workflow()`,
 *     so it adds no second nesting level. The CLI itself rejects nested
 *     workflow() calls; we also avoid them by construction.
 *   - All agents share the session's single OAuth token. This workflow makes NO
 *     claim to multi-account rotation — that is a loom-daemon + spawn-claude.sh
 *     concern and lives on the other side of the substrate boundary.
 *
 * READ-ONLY / SIDE-EFFECT-FREE by construction:
 *   - Returns a verdict object. Applies NO labels, merges NOTHING, transitions
 *     no loom:pr / loom:changes-requested state, creates NO GitHub issues.
 *   - The caller (a future runnable prototype) decides what, if anything, to do
 *     with the verdict. During the experiment, the answer is "just record it".
 *
 * API: written against Claude Code CLI v2.1.206, whose workflow VM injects the
 * globals `agent`, `parallel`, `pipeline`, `workflow`, `budget`, `args`,
 * `console`, `log`, `phase`. There is NO `verify()` primitive; the adversarial
 * verify pass is an `agent()` call with a schema (confirmed against 2.1.206).
 *
 * Expected `args` shape (passed via the Workflow tool's `args`):
 *   {
 *     pr: number,             // PR number under review (for labeling only)
 *     diff: string,           // the unified diff text to review
 *     dimensions?: string[],  // optional override of the default dimension set
 *   }
 *
 * @workflow-meta (illustrative — the real meta header format is owned by the
 * CLI's workflow-discovery loader; reproduced here as documentation only):
 *   name: judge-fanout-experiment
 *   description: Multi-dimension PR review fan-out + adversarial verify (experiment #3739)
 *   whenToUse: Experiment only — never on the production sweep/judge path.
 */

/* eslint-disable no-undef */ // agent/parallel/budget/args are workflow-VM globals.

// --- Configuration ----------------------------------------------------------

const DEFAULT_DIMENSIONS = [
  "correctness",
  "security-credential-surface",
  "test-coverage",
  "perf-simplification",
];

// A finding a dimension reviewer emits. `evidenceLine` anchors the claim to the
// diff so the adversarial pass can check it is actually supported.
const FINDINGS_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["findings"],
  properties: {
    findings: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["severity", "dimension", "claim", "evidenceLine"],
        properties: {
          severity: { type: "string", enum: ["blocker", "major", "minor", "nit"] },
          dimension: { type: "string" },
          claim: { type: "string" },
          evidenceLine: {
            type: "string",
            description: "The diff hunk header or line the claim rests on.",
          },
        },
      },
    },
  },
};

// The adversarial pass re-emits each finding with a diffSupported verdict.
const VERIFIED_FINDINGS_SCHEMA = {
  type: "object",
  additionalProperties: false,
  required: ["findings"],
  properties: {
    findings: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        required: ["severity", "dimension", "claim", "diffSupported", "reason"],
        properties: {
          severity: { type: "string", enum: ["blocker", "major", "minor", "nit"] },
          dimension: { type: "string" },
          claim: { type: "string" },
          diffSupported: {
            type: "boolean",
            description: "True only if the diff actually contains evidence for the claim.",
          },
          reason: { type: "string", description: "Why the claim is / isn't supported." },
        },
      },
    },
  },
};

// --- Prompt builders (kept trivial — the real prompts would be fuller) ------

function reviewPrompt(dimension, diff) {
  return [
    `You are a code reviewer scoped to exactly ONE dimension: "${dimension}".`,
    `Review ONLY through that lens. Ignore issues outside your dimension.`,
    `For every finding, cite the diff hunk header or line it rests on (evidenceLine).`,
    `Do not invent issues; if the diff is clean for your dimension, return no findings.`,
    ``,
    `DIFF:`,
    diff,
  ].join("\n");
}

function verifyPrompt(rawFindings, diff) {
  return [
    `You are an ADVERSARIAL verifier. For each finding below, decide whether the`,
    `DIFF actually supports it (diffSupported=true) or whether it is an unverified`,
    `nit / hallucination / out-of-scope remark (diffSupported=false). Be strict:`,
    `a finding is supported ONLY if the cited evidence is really in the diff.`,
    ``,
    `FINDINGS:`,
    JSON.stringify(rawFindings, null, 2),
    ``,
    `DIFF:`,
    diff,
  ].join("\n");
}

// --- The workflow body ------------------------------------------------------
//
// A top-level `return` is how a workflow script yields its result to the
// Workflow tool. (Workflow scripts run in a VM where top-level await + return
// are allowed — see CLI 2.1.206.)

const dimensions = Array.isArray(args?.dimensions) && args.dimensions.length
  ? args.dimensions
  : DEFAULT_DIMENSIONS;

// Guard: this sketch is single-PR and single-token. Refuse absurd fan-outs so a
// mis-call can't blow the shared budget. (budget is a shared HARD ceiling in
// 2.1.206; agent() throws once it is exhausted anyway — this is belt-and-braces.)
if (dimensions.length > 8) {
  throw new Error(
    `judge-fanout: ${dimensions.length} dimensions requested; cap is 8 (single-PR, single-token experiment).`,
  );
}

// 1. FAN OUT — one reviewer per dimension. Direct agent() calls => exactly one
//    level deep. parallel() is a BARRIER: it awaits all reviewers.
const rawFindings = (
  await parallel(
    dimensions.map((dim) => () =>
      agent(reviewPrompt(dim, String(args?.diff ?? "")), {
        label: `review:${dim}`,
        phase: "review", // explicit phase group avoids racing the global phase()
        // #3705 note: effort IS recoverable in-session (agent accepts opts.effort);
        // correctness gets the highest tier, the rest medium.
        effort: dim === "correctness" ? "high" : "medium",
        schema: FINDINGS_SCHEMA,
      }),
    ),
  )
)
  // agent() returns null if the user skips it or it dies terminally — filter those.
  .filter(Boolean)
  .flatMap((r) => (r && Array.isArray(r.findings) ? r.findings : []));

// Short-circuit: nothing to verify.
if (rawFindings.length === 0) {
  return {
    pr: args?.pr ?? null,
    verdict: "approve",
    findings: [],
    dimensionsCovered: dimensions,
    note: "no findings from any dimension reviewer",
  };
}

// 2. ADVERSARIAL VERIFY — an agent() with a schema (NOT a verify() primitive).
//    Drops findings the diff doesn't actually support.
const verifyResult = await agent(verifyPrompt(rawFindings, String(args?.diff ?? "")), {
  label: "adversarial-verify",
  phase: "verify",
  effort: "high",
  schema: VERIFIED_FINDINGS_SCHEMA,
});

const verified = ((verifyResult && verifyResult.findings) || []).filter(
  (f) => f && f.diffSupported === true,
);

// 3. TYPED REDUCE — plain JS. No free-text/label parsing. READ-ONLY: we return a
//    verdict; we apply nothing to the PR.
const hasBlocker = verified.some((f) => f.severity === "blocker" || f.severity === "major");

return {
  pr: args?.pr ?? null,
  verdict: hasBlocker ? "changes-requested" : "approve",
  findings: verified,
  droppedUnverified: rawFindings.length - verified.length,
  dimensionsCovered: dimensions,
  // NOTE: the caller decides what to do with this. This workflow merges nothing,
  // labels nothing, and creates no issues — it is a review-quality experiment.
};
