// client-reference.mjs — a PORT of loom-ui's client-side landed-size scorer,
// for the parity checks in `run-parity.mjs` (Issue #12548).
//
// Ported by hand from 2AMLogic/loom-ui at 9f4d5f2a87e4ae9dc78de0cae81424dc08fee5a9:
//   web/src/pages/kpis/landingScore.ts   (parseOutcome, scoreFacts,
//                                          landingComponents, clientLsi, landingLsi)
//   web/src/pages/kpis/landingOutcome.ts (isLandingOutcome)
//   web/src/pages/kpis/deliveredModel.ts (collectLandings)
//
// It is a port, not an import: loom-ui is another repository and nothing here
// may read it at test time. Drift is caught only by re-diffing against the
// pinned files (see the feasibility doc). The standardization PARAMETERS are
// not ported — `clientParamsFromSql` reads them out of `../landed-size.sql`, so
// this repo keeps exactly one definition of them.

/** Parse the `params` CTE and `model_token_factors` out of landed-size.sql. */
export function clientParamsFromSql(sql) {
  const num = (name) => {
    const m = new RegExp(`([-0-9.]+)\\s+AS ${name}\\b`).exec(sql);
    if (!m) throw new Error(`landed-size.sql: no ${name}`);
    return Number(m[1]);
  };
  const version = /'([^']+)'\s+AS params_version/.exec(sql);
  if (!version) throw new Error("landed-size.sql: no params_version");
  const factors = {};
  const block = /model_token_factors\(model, factor\) AS \(\s*VALUES([\s\S]*?)\n\s*\)/.exec(sql);
  if (!block) throw new Error("landed-size.sql: no model_token_factors");
  for (const m of block[1].matchAll(/\('([^']+)',\s*([0-9.]+)\)/g)) factors[m[1]] = Number(m[2]);
  return {
    paramsVersion: version[1],
    meanLogHwLines: num("mean_log_hw_lines"),
    sdLogHwLines: num("sd_log_hw_lines"),
    meanLogHwFiles: num("mean_log_hw_files"),
    sdLogHwFiles: num("sd_log_hw_files"),
    meanLogNormTokens: num("mean_log_norm_tokens"),
    sdLogNormTokens: num("sd_log_norm_tokens"),
    cuts: [1, 2, 3, 4, 5, 6].map((i) => num(`cut_${i}`)),
    modelTokenFactors: factors,
  };
}

// landingScore.ts `num`: a negative reading is a counter reset → absent.
const num = (record, field) => {
  const v = record[field];
  return typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : undefined;
};
const signed = (record, field) => {
  const v = record[field];
  return typeof v === "number" && Number.isFinite(v) ? v : undefined;
};
const tokensClean = (status) => status === undefined || status === "measured";

function parseOutcome(record) {
  return {
    prNumber: num(record, "pr_number"),
    landedSize: signed(record, "landed_size"),
    lsi: num(record, "lsi"),
    hwLinesAdded: num(record, "hw_lines_added"),
    hwLinesDeleted: num(record, "hw_lines_deleted"),
    hwFiles: num(record, "hw_files"),
    tokensIn: num(record, "tokens_in"),
    tokensOut: num(record, "tokens_out"),
    tokensStatus: typeof record.tokens_status === "string" ? record.tokens_status : undefined,
  };
}

// landingOutcome.ts
function isLandingOutcome(record) {
  const disposition = typeof record.disposition === "string" ? record.disposition : undefined;
  if (disposition !== undefined) return disposition === "landed";
  return record.result === "success" && typeof record.pr_number === "number" && Number.isFinite(record.pr_number);
}

function scoreFacts(record, parsed) {
  const model = Array.isArray(record.models_used)
    ? typeof record.models_used[0] === "string" ? record.models_used[0] : undefined
    : typeof record.model === "string" ? record.model : undefined;
  return {
    ...(parsed.lsi !== undefined ? { lsi: parsed.lsi } : parsed.landedSize !== undefined ? { lsi: Math.exp(parsed.landedSize) } : {}),
    ...(parsed.hwLinesAdded !== undefined && { hwLinesAdded: parsed.hwLinesAdded }),
    ...(parsed.hwLinesDeleted !== undefined && { hwLinesDeleted: parsed.hwLinesDeleted }),
    ...(parsed.hwFiles !== undefined && { hwFiles: parsed.hwFiles }),
    ...(parsed.tokensIn !== undefined && parsed.tokensOut !== undefined && tokensClean(parsed.tokensStatus) && { tokens: parsed.tokensIn + parsed.tokensOut }),
    ...(model !== undefined && { model }),
    ...(parsed.tokensStatus !== undefined && { tokensStatus: parsed.tokensStatus }),
  };
}

function landingLsi(facts, P) {
  if (facts.lsi !== undefined) return facts.lsi;
  const z = [];
  if (facts.hwLinesAdded !== undefined && facts.hwLinesDeleted !== undefined) {
    z.push((Math.log1p(facts.hwLinesAdded + facts.hwLinesDeleted) - P.meanLogHwLines) / P.sdLogHwLines);
  }
  if (facts.hwFiles !== undefined) z.push((Math.log1p(facts.hwFiles) - P.meanLogHwFiles) / P.sdLogHwFiles);
  if (tokensClean(facts.tokensStatus) && facts.tokens !== undefined) {
    const factor = facts.model !== undefined ? P.modelTokenFactors[facts.model] : undefined;
    if (factor !== undefined) z.push((Math.log1p(facts.tokens / factor) - P.meanLogNormTokens) / P.sdLogNormTokens);
  }
  if (z.length === 0) return undefined;
  return Math.exp(z.reduce((s, v) => s + v, 0) / z.length);
}

/** deliveredModel.ts `collectLandings` over rows in fetch order. `landings` is
 * the earliest landing per repo#issue; `scored` is one per landing sweep. */
export function clientCollect(rows, P) {
  const byIssue = new Map();
  const bySweep = new Map();
  for (const row of rows) {
    if (row.kind !== "sweep.outcome") continue;
    const parsed = parseOutcome(row.record);
    if (!isLandingOutcome(row.record) || parsed.prNumber === undefined) continue;
    const repo = row.repo ?? "";
    const issue = row.issue ?? parsed.prNumber;
    const at = Date.parse(row.emittedAt);
    if (!Number.isFinite(at)) continue;
    const key = `${repo}#${issue}`;
    const sweepId = row.sweepId ? `sweep:${row.sweepId}` : `row:${row.id}`;
    const facts = scoreFacts(row.record, parsed);
    const landing = { repo, issue, at, sweepId, ...facts, lsi: landingLsi(facts, P) };
    if (!bySweep.has(sweepId)) bySweep.set(sweepId, landing);
    const existing = byIssue.get(key);
    if (existing && existing.at <= at) continue;
    byIssue.set(key, landing);
  }
  return { landings: [...byIssue.values()].sort((a, b) => a.at - b.at), scored: [...bySweep.values()].sort((a, b) => a.at - b.at) };
}
