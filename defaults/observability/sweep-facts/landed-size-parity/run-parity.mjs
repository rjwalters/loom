#!/usr/bin/env node
// run-parity.mjs — landed-size parity checks (Issue #12548).
//
//   node run-parity.mjs [--require-candidate]
//
// Runs the synthetic corpus (`corpus.json`) through
//   (A) the SQL reference: `../landed-size.sql` executed by the real `sqlite3`
//       CLI over a `sweep_facts` table, and
//   (B) a port of loom-ui's client scorer (`client-reference.mjs`),
// and compares population (is this sweep a scored landing?) and LSI. It also
// checks that the vendored loom-ui parameter snapshot equals the SQL's, that
// first-per-issue selection agrees, and that `fit-landed-size.mjs` applies the
// half-open baseline window at its edges.
//
// What this does NOT do: execute any candidate SigNoz/ClickHouse dialect. No
// such engine is available or authorized here (see
// docs/observability/landed-size-dialect-feasibility.md); the probe at the end
// reports that as BLOCKED, never as a pass. `--require-candidate` turns the
// blocker into exit code 3 so a future run with an engine cannot be mistaken
// for a pass.
//
// Exit: 0 all checks hold; 1 a check failed; 2 environment problem (no sqlite3);
// 3 candidate engine required but unavailable.

import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { clientCollect, clientParamsFromSql } from "./client-reference.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const sqlPath = join(here, "..", "landed-size.sql");
const fitPath = join(here, "..", "fit-landed-size.mjs");
const sqlText = readFileSync(sqlPath, "utf8");
const corpus = JSON.parse(readFileSync(join(here, "corpus.json"), "utf8"));
const snapshot = JSON.parse(readFileSync(join(here, "loom-ui-params-snapshot.json"), "utf8"));
const TOL = 1e-9;

let failures = 0;
const check = (ok, label, detail = "") => {
  if (!ok) failures += 1;
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}${detail ? `  — ${detail}` : ""}`);
};
const close = (a, b) => (a == null || b == null ? a == null && b == null : Math.abs(a - b) <= TOL * Math.max(1, Math.abs(a), Math.abs(b)));

// --- 0. Parameters: SQL is the source; the loom-ui snapshot must equal it ---
const params = clientParamsFromSql(sqlText);
check(params.paramsVersion === snapshot.paramsVersion, "params_version matches loom-ui snapshot", `${params.paramsVersion} vs ${snapshot.paramsVersion}`);
for (const key of ["meanLogHwLines", "sdLogHwLines", "meanLogHwFiles", "sdLogHwFiles", "meanLogNormTokens", "sdLogNormTokens"]) {
  check(params[key] === snapshot[key], `param ${key} matches loom-ui snapshot`, `${params[key]} vs ${snapshot[key]}`);
}
check(JSON.stringify(params.cuts) === JSON.stringify(snapshot.cuts), "size-class cuts match loom-ui snapshot");
check(JSON.stringify(params.modelTokenFactors) === JSON.stringify(snapshot.modelTokenFactors), "model token factors match loom-ui snapshot");

// --- A. SQL reference through the real sqlite3 ------------------------------
const q = (v) => (v == null ? "NULL" : typeof v === "number" ? String(v) : `'${String(v).replaceAll("'", "''")}'`);
const factsRow = (c) => {
  const p = c.payload;
  return [c.row.repo, c.row.issue, c.row.sweep_id, c.row.emitted_at, p.result, p.disposition, p.tokens_status,
    p.models_used === undefined ? null : JSON.stringify(p.models_used), p.tokens_in, p.tokens_out,
    p.hw_lines_added, p.hw_lines_deleted, p.hw_files, p.pr_number];
};
const ddl = `CREATE TABLE sweep_facts (repo TEXT NOT NULL, issue INTEGER NOT NULL, sweep_id TEXT NOT NULL,
  emitted_at TEXT, result TEXT, disposition TEXT, tokens_status TEXT, models_used TEXT, tokens_in INTEGER,
  tokens_out INTEGER, hw_lines_added INTEGER, hw_lines_deleted INTEGER, hw_files INTEGER, pr_number INTEGER,
  PRIMARY KEY (repo, issue, sweep_id));`;
const inserts = corpus.cases.map((c) => `INSERT INTO sweep_facts VALUES (${factsRow(c).map(q).join(", ")});`).join("\n");
let sqlRows;
try {
  const script = `${ddl}\n${inserts}\n${sqlText}\n.mode json\nSELECT * FROM issue_landed_size ORDER BY repo, issue, landed_at, landing_sweep_id;`;
  const out = execFileSync("sqlite3", [":memory:"], { input: script, encoding: "utf8", maxBuffer: 1 << 26 });
  sqlRows = JSON.parse(out);
} catch (error) {
  console.error(`cannot run the SQL reference with sqlite3: ${error.message}`);
  process.exit(2);
}
const sqlBySweep = new Map(sqlRows.map((r) => [r.landing_sweep_id, r]));

// --- B. Client port ---------------------------------------------------------
const historyRows = corpus.cases.map((c, index) => ({
  id: c.row.export_id ?? 5000 + index,
  emittedAt: c.row.emitted_at,
  hostId: c.row.host_id,
  kind: "sweep.outcome",
  repo: c.row.repo,
  issue: c.row.issue,
  sweepId: c.row.sweep_id,
  record: c.payload,
}));
// The order `fetchLandingRecords` hands rows over: newest first, id descending.
historyRows.sort((a, b) => Date.parse(b.emittedAt) - Date.parse(a.emittedAt) || b.id - a.id);
const client = clientCollect(historyRows, params);
const clientBySweep = new Map(client.scored.map((l) => [l.sweepId.replace(/^sweep:/, ""), l]));

// --- Compare per case --------------------------------------------------------
const classOf = (lsi) => {
  if (lsi == null) return null;
  const labels = ["1", "2", "3", "5", "8", "13"];
  const i = params.cuts.findIndex((cut) => lsi <= cut);
  return i === -1 ? "21" : labels[i];
};
let maxAbs = 0;
for (const c of corpus.cases) {
  const s = sqlBySweep.get(c.row.sweep_id);
  const k = clientBySweep.get(c.row.sweep_id);
  const sqlLanding = s !== undefined;
  const clientLanding = k !== undefined;
  const sqlLsi = s?.LSI ?? undefined;
  const clientLsi = k?.lsi ?? undefined;
  const samePop = sqlLanding === clientLanding;
  const sameLsi = close(sqlLsi, clientLsi);
  if (samePop && sameLsi && sqlLsi != null) maxAbs = Math.max(maxAbs, Math.abs(sqlLsi - clientLsi));
  if (c.expect === "match") {
    check(samePop && sameLsi, `case ${c.id}`, `landing sql=${sqlLanding} client=${clientLanding}; LSI sql=${sqlLsi ?? "NULL"} client=${clientLsi ?? "absent"}`);
  } else {
    const differs = c.diverges === "population" ? !samePop : samePop && !sameLsi;
    check(differs, `case ${c.id} (documented ${c.diverges} divergence)`, `landing sql=${sqlLanding} client=${clientLanding}; LSI sql=${sqlLsi ?? "NULL"} client=${clientLsi ?? "absent"}`);
  }
  if (s) {
    check(s.size_class === classOf(sqlLsi) || (s.size_class == null && sqlLsi == null), `case ${c.id}: size_class ${s.size_class ?? "NULL"} follows the fixed cuts`);
    if (s.landed_size != null) check(Math.abs(Math.exp(s.landed_size) - s.LSI) <= 1e-12 * Math.max(1, s.LSI), `case ${c.id}: LSI = exp(landed_size)`);
  }
}
console.log(`INFO  max |sql LSI − client LSI| over agreeing cases: ${maxAbs.toExponential(3)} (neither side rounds; tolerance ${TOL})`);

// Anchors the corpus must keep exercising.
const neg = sqlBySweep.get("s-negative-landed-size");
check(neg && neg.landed_size < 0 && neg.LSI > 0, "anchor: negative landed_size with positive LSI is covered", neg ? `landed_size=${neg.landed_size} LSI=${neg.LSI}` : "missing");
const none = sqlBySweep.get("s-no-components");
check(none && none.landed_size == null && none.LSI == null && none.size_class == null, "anchor: no components → NULL landed_size/LSI/class, not 0");
check(sqlBySweep.get("s-model-unattributed")?.z_tokens != null, "anchor: <unattributed> model carries its own factor");

// --- First-per-issue (the client's `landings` population) --------------------
// SQL has no dedupe: one row per landing sweep. The client's count/effort
// population keeps the earliest landing per repo#issue; reproduce it in SQL.
const firstSql = new Map();
for (const r of [...sqlRows].sort((a, b) => (a.landed_at < b.landed_at ? -1 : a.landed_at > b.landed_at ? 1 : 0))) {
  const key = `${r.repo}#${r.issue}`;
  if (!firstSql.has(key)) firstSql.set(key, r.landing_sweep_id);
}
const firstClient = new Map(client.landings.map((l) => [`${l.repo}#${l.issue}`, l.sweepId.replace(/^sweep:/, "")]));
const tieKey = "acme/widgets#19";
for (const [key, sweep] of firstClient) {
  if (key === tieKey || !firstSql.has(key)) continue;
  check(firstSql.get(key) === sweep, `first-per-issue ${key} agrees`, `sql=${firstSql.get(key)} client=${sweep}`);
}
check(firstClient.get(tieKey) === "s-tie-b", "equal-timestamp tie: client keeps the first-processed row (highest export id)", `client=${firstClient.get(tieKey)}`);
console.log("INFO  equal-timestamp ties are order-dependent in the client; a SQL dedupe needs an explicit tie-break (sweep_id/export id) to match.");

// --- Baseline-window edges, via the real fitter -------------------------------
{
  const windowStart = "2026-10-01T00:00:00.000Z";
  const windowEnd = "2026-10-02T00:00:00.000Z";
  const ms = Date.parse(windowStart);
  const iso = (t) => new Date(t).toISOString();
  const landed = (at, i) => ({ at, disposition: "landed", model: "m1", hw_lines_added: 10 * (i + 1), hw_lines_deleted: i, hw_files: i + 1, tokens_in: 1000 * (i + 2), tokens_out: 500 * i });
  const base = Array.from({ length: 8 }, (_, i) => landed(iso(ms + (i + 1) * 3_600_000), i));
  const edges = [
    landed(windowStart, 3), // exactly windowStart: in
    landed(windowEnd, 4), //   exactly windowEnd: out (half-open)
    landed(iso(ms - 1), 5), //  1 ms before: out
    landed(iso(Date.parse(windowEnd) - 1), 6), // 1 ms before end: in
    { at: iso(ms + 5 * 3_600_000), disposition: "noop_already_done", model: "m1" }, // in window, not a landing
  ];
  const dir = mkdtempSync(join(tmpdir(), "lsi-fit-"));
  try {
    const file = join(dir, "extract.json");
    writeFileSync(file, JSON.stringify({ capturedAt: "synthetic", windowStart, windowEnd, landings: [...base, ...edges] }));
    const run = spawnSync("node", [fitPath, file], { encoding: "utf8" });
    const m = /(\d+) outcome rows, (\d+) landings/.exec(run.stdout);
    check(run.status === 0 && m !== null, "fit-landed-size.mjs runs on the synthetic extract", run.status === 0 ? "" : run.stderr.trim());
    if (m) {
      check(Number(m[1]) === 8 + 2 + 1 && Number(m[2]) === 8 + 2, "fitter window is [start, end): start edge in, end edge and 1 ms before start out", `rows=${m[1]} landings=${m[2]}`);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

// --- Candidate dialect probe --------------------------------------------------
const probe = ["clickhouse-local", "clickhouse"].map((bin) => ({ bin, ok: spawnSync(bin, ["--version"], { encoding: "utf8" }).status === 0 }));
const engine = probe.find((p) => p.ok);
console.log("");
if (engine) {
  console.log(`CANDIDATE  a local '${engine.bin}' exists, but no candidate query is registered in this harness and a local engine is NOT the deployed SigNoz query surface (which rejects JSON* functions, code 48). No parity is claimed.`);
} else {
  console.log("CANDIDATE  BLOCKED — no ClickHouse engine on PATH (clickhouse-local, clickhouse), and live SigNoz execution is outside this issue's authorization. No candidate-dialect parity is claimed.");
}
console.log(failures === 0 ? "\nALL CHECKS HOLD" : `\n${failures} CHECK(S) FAILED`);
if (failures > 0) process.exit(1);
if (process.argv.includes("--require-candidate")) process.exit(3);
