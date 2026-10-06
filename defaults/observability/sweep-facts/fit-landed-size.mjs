#!/usr/bin/env node
// fit-landed-size.mjs — fit the landed-size standardization parameters
// (Issues #9466/#9934), the one-time run `landed-size.sql`'s `params` CTE
// awaits: component means/SDs on log1p, per-model token factors, baseline
// window bounds, and the `params_version` bump.
//
//   node fit-landed-size.mjs <extract.json> [windowEnd]
//
// The extract is a frozen dump of the baseline window's landing sweeps:
//
//   { "capturedAt": "...", "windowStart": "...", "windowEnd": "...",
//     "landings": [ { "at": "...", "disposition": "landed", "result": "...",
//                     "model": "...", "hw_lines_added": 1034, ... } ] }
//
// Field names and presence semantics mirror the production record exactly
// (`telemetry-schema.md` §`sweep.outcome`): a field the sweep did not report
// is `null` here and stays absent downstream — the fit never coerces a
// missing reading to 0. Two paths produce an extract, and either is valid —
// they read the same records:
//
//   1. D1, the rollup's own store (the canonical rerun path):
//        wrangler d1 execute loom-fleet-telemetry --json \
//          --command "SELECT emitted_at AS at, disposition, result, pr_number
//                     AS pr, json_extract(payload,'$.models_used[0]') AS model,
//                     json_extract(payload,'$.hw_lines_added') AS hw_lines_added, ...
//                     FROM records WHERE kind='sweep.outcome'
//                     AND emitted_at >= '...' AND emitted_at < '...'"
//      (run `sweep-facts-rollup.sql` first and read `sweep_facts` instead if
//      the rollup is current — same rows, already denormalized).
//   2. The dashboard's `GET /api/history?kind=sweep.outcome&since=…`, paged —
//      the same `records` rows over the loom-ui seam.
//
// THE FIT (fixed here so reruns are comparable; every choice is restated in
// the provenance block it prints):
//
//   1. Landings: `disposition = 'landed'`, or the documented pre-#9441
//      fallback (`disposition` absent AND `result = 'success'` AND a
//      `pr_number`) — the same predicate `landed-size.sql`'s `landings` CTE
//      runs; if that CTE changes, change this.
//   2. Component presence mirrors the rollup's NULL propagation: `hw_lines`
//      needs BOTH `hw_lines_added` and `hw_lines_deleted` (their sum, the
//      #9430 added+deleted convention); `tokens` needs BOTH `tokens_in` and
//      `tokens_out`, AND a clean token verdict — `tokens_status` absent
//      (legacy rows) or `measured` (#9440/#9454: `suspect`,
//      `unattributable` and `not_spawned` are published numbers that are
//      not a measurement, so they fit into nothing).
//   3. Per-model token factors: `factor_m = exp(mean(ln tokens))` (the
//      geometric mean) over model m's token-eligible landings in the
//      window — so `log1p(tokens / factor_m)` is centered per model and
//      one global standardization is fair across models. Models with fewer
//      than MIN_MODEL_LANDINGS eligible landings get NO factor (an
//      under-fit factor must not impersonate a normalization); their
//      token component drops out downstream, as the SQL documents.
//   4. Standardization constants per component: mean and population SD
//      (divide by n — the baseline standardizes to unit variance by
//      construction) of `ln(1 + x)` — `log1p`, which SQLite's math set
//      lacks and `landed-size.sql` spells `ln(1.0 + x)` — over the
//      window's eligible values for that component.
//   5. Constants are printed rounded to 6 decimals for the SQL literal;
//      the provenance block records the exact values, counts and window.

import { readFileSync } from "node:fs";

/** A model with fewer eligible token landings than this gets no factor. */
const MIN_MODEL_LANDINGS = 5;
/** Decimal places kept in the SQL literals. */
const SQL_DECIMALS = 6;

const path = process.argv[2];
if (!path) {
  console.error("usage: node fit-landed-size.mjs <extract.json> [windowEnd]");
  process.exit(2);
}
const extract = JSON.parse(readFileSync(path, "utf8"));
const windowStart = extract.windowStart;
const windowEnd = process.argv[3] ?? extract.windowEnd;
if (!windowStart || !windowEnd) {
  console.error("the extract must carry windowStart, and windowEnd comes from it or argv[3]");
  process.exit(2);
}
const startMs = Date.parse(windowStart);
const endMs = Date.parse(windowEnd);
if (!Number.isFinite(startMs) || !Number.isFinite(endMs) || endMs <= startMs) {
  console.error("unusable window bounds");
  process.exit(2);
}

// --- 1. The window's landings, with the rollup's presence semantics --------
const inWindow = extract.landings.filter((row) => {
  const at = Date.parse(row.at);
  return Number.isFinite(at) && at >= startMs && at < endMs;
});
const landings = inWindow.filter(
  (row) => row.disposition === "landed" || (row.disposition == null && row.result === "success" && row.pr_number != null),
);

const both = (a, b) => (a != null && b != null ? a + b : null);
const tokenEligible = (row) => row.tokens_status == null || row.tokens_status === "measured";

const hwLines = landings.map((row) => both(row.hw_lines_added, row.hw_lines_deleted));
const hwFiles = landings.map((row) => row.hw_files);
const tokens = landings.map((row) => (tokenEligible(row) ? both(row.tokens_in, row.tokens_out) : null));
const models = landings.map((row) => row.model ?? null);

// --- 2. Per-model token factors (geometric mean, MIN_MODEL_LANDINGS floor) -
const byModel = new Map();
tokens.forEach((tokens_, index) => {
  if (tokens_ == null) return;
  const model = models[index];
  if (model == null) return;
  (byModel.get(model) ?? byModel.set(model, []).get(model)).push(tokens_);
});
const modelTokenFactors = [];
for (const [model, values] of [...byModel.entries()].sort()) {
  if (values.length < MIN_MODEL_LANDINGS) continue;
  const meanLog = values.reduce((sum, value) => sum + Math.log(value), 0) / values.length;
  modelTokenFactors.push({ model, factor: Math.exp(meanLog), n: values.length });
}
const factorOf = new Map(modelTokenFactors.map(({ model, factor }) => [model, factor]));

// --- 3. The normalized token values, then the three constants -------------
const normTokens = tokens.map((tokens_, index) => {
  const factor = factorOf.get(models[index]);
  return tokens_ != null && factor != null ? Math.log(1 + tokens_ / factor) : null;
});

const fitComponent = (values) => {
  const present = values.filter((value) => value != null).map((value) => Math.log(1 + value));
  if (present.length === 0) return null;
  const mean = present.reduce((sum, value) => sum + value, 0) / present.length;
  const sd = Math.sqrt(present.reduce((sum, value) => sum + (value - mean) ** 2, 0) / present.length);
  // A zero-SD component cannot standardize (the SQL's divide-by-zero fails
  // open into NULL for every row) — refuse the fit rather than publish it.
  if (sd === 0) {
    console.error(`a component is constant across the window (sd = 0) — a standardization on it is undefined; widen the window`);
    process.exit(1);
  }
  return { mean, sd, n: present.length };
};

/** The same fit over values that are ALREADY in log space — the normalized
 * token component arrives as `ln(1 + tokens/factor)` per row (each row's
 * own factor), so it must not be logged a second time. */
const fitLogged = (values) => {
  const present = values.filter((value) => value != null);
  if (present.length === 0) return null;
  const mean = present.reduce((sum, value) => sum + value, 0) / present.length;
  const sd = Math.sqrt(present.reduce((sum, value) => sum + (value - mean) ** 2, 0) / present.length);
  if (sd === 0) {
    console.error(`a component is constant across the window (sd = 0) — a standardization on it is undefined; widen the window`);
    process.exit(1);
  }
  return { mean, sd, n: present.length };
};

const linesFit = fitComponent(hwLines);
const filesFit = fitComponent(hwFiles);
const tokensFit = fitLogged(normTokens);
if (!linesFit || !filesFit || !tokensFit) {
  console.error("a component has no eligible values in the window — the fit cannot run");
  process.exit(1);
}

const round = (value) => Number(value.toFixed(SQL_DECIMALS));

// --- 4. Diagnostics ---------------------------------------------------------
const quantiles = (values, ps) => {
  const sorted = values.filter((v) => v != null).slice().sort((a, b) => a - b);
  return Object.fromEntries(ps.map((p) => [p, sorted[Math.min(sorted.length - 1, Math.floor(p * (sorted.length - 1)))]]));
};
const lsiOf = (row, index) => {
  const parts = [
    hwLines[index] != null ? (Math.log1p(hwLines[index]) - linesFit.mean) / linesFit.sd : null,
    hwFiles[index] != null ? (Math.log1p(hwFiles[index]) - filesFit.mean) / filesFit.sd : null,
    normTokens[index] != null ? (normTokens[index] - tokensFit.mean) / tokensFit.sd : null,
  ].filter((value) => value != null);
  return parts.length > 0 ? Math.exp(parts.reduce((sum, value) => sum + value, 0) / parts.length) : null;
};
const lsis = landings.map((row, index) => lsiOf(row, index));
const lsiValues = lsis.filter((value) => value != null);

console.log(`# fit-landed-size — ${new Date().toISOString()}`);
console.log(`# extract: ${path} (capturedAt ${extract.capturedAt ?? "n/a"})`);
console.log(`# window:  [${windowStart}, ${windowEnd})  — ${inWindow.length} outcome rows, ${landings.length} landings`);
console.log(`#   hw_lines  eligible: ${linesFit.n}   mean(ln1p)=${linesFit.mean} sd=${linesFit.sd}`);
console.log(`#   hw_files  eligible: ${filesFit.n}   mean(ln1p)=${filesFit.mean} sd=${filesFit.sd}`);
console.log(`#   tokens    eligible: ${tokensFit.n}   mean(ln1p norm)=${tokensFit.mean} sd=${tokensFit.sd}`);
console.log(`#   token verdicts excluded (suspect/unattributable/not_spawned): ${
  landings.filter((row) => !tokenEligible(row) && both(row.tokens_in, row.tokens_out) != null).length
}`);
console.log(`#   models without a factor (< ${MIN_MODEL_LANDINGS} eligible): ${
  [...byModel.entries()].filter(([, v]) => v.length < MIN_MODEL_LANDINGS).map(([m, v]) => `${m}(${v.length})`).join(", ") || "none"
}`);
console.log(`# LSI over the window: n=${lsiValues.length} median=${quantiles(lsiValues, [0.5])[0.5]} p10=${quantiles(lsiValues, [0.1])[0.1]} p90=${quantiles(lsiValues, [0.9])[0.9]}`);
console.log("");
console.log("-- params CTE replacement for landed-size.sql (rounded literals) --------------");
console.log(`    params AS (`);
console.log(`        SELECT`);
console.log(`            'v1-${windowEnd.slice(0, 10)}'   AS params_version,`);
console.log(`            '${windowStart}'  AS baseline_start,`);
console.log(`            '${windowEnd}'    AS baseline_end,`);
console.log(`            ${round(linesFit.mean)}  AS mean_log_hw_lines,`);
console.log(`            ${round(linesFit.sd)}    AS sd_log_hw_lines,`);
console.log(`            ${round(filesFit.mean)}  AS mean_log_hw_files,`);
console.log(`            ${round(filesFit.sd)}    AS sd_log_hw_files,`);
console.log(`            ${round(tokensFit.mean)} AS mean_log_norm_tokens,`);
console.log(`            ${round(tokensFit.sd)}   AS sd_log_norm_tokens,`);
console.log(`            1.0  AS cut_1,`);
console.log(`            2.0  AS cut_2,`);
console.log(`            3.0  AS cut_3,`);
console.log(`            5.0  AS cut_4,`);
console.log(`            8.0  AS cut_5,`);
console.log(`            13.0 AS cut_6`);
console.log(`    ),`);
// VALUES, not UNION ALL — D1 caps compound SELECT at 5 terms (#10066), and a
// fleet with six or more fitted models would exceed it as a SELECT chain.
// Multi-row VALUES is exempt from that limit.
console.log(`    model_token_factors(model, factor) AS (`);
console.log(`        VALUES`);
console.log(modelTokenFactors
  .map(({ model, factor }) => `            ('${model}', ${round(factor)})`)
  .join(",\n"));
console.log(`    ),`);
console.log("");
console.log("-- provenance (paste beside the params CTE) -----------------------------------");
console.log(JSON.stringify({
  params_version: `v1-${windowEnd.slice(0, 10)}`,
  window: { start: windowStart, end: windowEnd },
  extract: { capturedAt: extract.capturedAt ?? null, rows: inWindow.length, landings: landings.length },
  components: {
    hw_lines: linesFit, hw_files: filesFit, norm_tokens: tokensFit,
  },
  model_token_factors: modelTokenFactors,
  excluded_token_verdicts: landings.filter((row) => !tokenEligible(row) && both(row.tokens_in, row.tokens_out) != null).length,
  lsi: { n: lsiValues.length, ...quantiles(lsiValues, [0.1, 0.5, 0.9]) },
  definition: {
    landings: "disposition='landed' or (disposition absent AND result='success' AND pr_number present)",
    components: "log1p; hw_lines needs both hw_*_lines fields; tokens need both fields AND a clean tokens_status",
    factors: `per-model geometric mean of tokens, models under ${MIN_MODEL_LANDINGS} eligible landings get none`,
    constants: "mean and population SD over the window's eligible values",
  },
}, null, 1));
