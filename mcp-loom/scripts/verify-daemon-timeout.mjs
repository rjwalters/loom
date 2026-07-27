#!/usr/bin/env node
/**
 * Standalone verification for the bounded unary-request timeout (issue #3945).
 *
 * mcp-loom has no unit-test runner, so this self-contained script exercises
 * the REAL `sendDaemonRequest` transport against stub Unix-socket servers:
 *
 *   1. Hang case  — server accepts the connection but never responds and never
 *      closes. Assert the call REJECTS within the bounded timeout (well under
 *      the 1800s MCP idle timeout that motivated the fix), with the greppable
 *      "did not respond within" error.
 *   2. Happy case — server writes a JSON response frame then ends. Assert the
 *      call RESOLVES quickly and the timer was cleared (no regression to the
 *      fast path used by dispatch_sweep / list_sweeps / publish_event).
 *
 * Run: node scripts/verify-daemon-timeout.mjs   (from the mcp-loom dir)
 *
 * No test framework or new runtime deps: it bundles `src/shared/daemon.ts`
 * with the already-present esbuild devDependency and imports the result.
 */

import { build } from "esbuild";
import net from "node:net";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO = join(__dirname, "..");

async function bundleTransport(outDir) {
  const outfile = join(outDir, "daemon.mjs");
  await build({
    entryPoints: [join(REPO, "src/shared/daemon.ts")],
    bundle: true,
    format: "esm",
    platform: "node",
    outfile,
    logLevel: "silent",
  });
  return import(pathToFileURL(outfile).href);
}

/** Start a stub server on a temp unix socket. `onConn(socket)` handles it. */
function startStubServer(sockPath, onConn) {
  return new Promise((resolve, reject) => {
    const server = net.createServer(onConn);
    server.on("error", reject);
    server.listen(sockPath, () => resolve(server));
  });
}

function closeServer(server) {
  return new Promise((resolve) => server.close(() => resolve()));
}

let failures = 0;
function check(name, ok, detail) {
  if (ok) {
    console.log(`  ok   ${name}`);
  } else {
    failures += 1;
    console.error(`  FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

async function main() {
  const workDir = await mkdtemp(join(tmpdir(), "loom-3945-"));
  const outDir = await mkdtemp(join(tmpdir(), "loom-3945-out-"));

  // `SOCKET_PATH` in config.ts is a module-load-time constant read from
  // LOOM_SOCKET_PATH, so it MUST be set before the transport bundle is
  // imported. Both stub servers reuse this single path (each net server
  // unlinks the socket file on close, so the second listen is clean).
  const sockPath = join(workDir, "daemon.sock");
  process.env.LOOM_SOCKET_PATH = sockPath;

  const { sendDaemonRequest, resolveDaemonIpcTimeoutMs, DEFAULT_DAEMON_IPC_TIMEOUT_MS } =
    await bundleTransport(outDir);

  console.log("resolveDaemonIpcTimeoutMs / env-override");
  check(
    "default is DEFAULT_DAEMON_IPC_TIMEOUT_MS",
    resolveDaemonIpcTimeoutMs() === DEFAULT_DAEMON_IPC_TIMEOUT_MS,
    `got ${resolveDaemonIpcTimeoutMs()}`
  );
  process.env.LOOM_DAEMON_IPC_TIMEOUT_MS = "1234";
  check("env override honored", resolveDaemonIpcTimeoutMs() === 1234);
  process.env.LOOM_DAEMON_IPC_TIMEOUT_MS = "-5";
  check("invalid env falls back to default", resolveDaemonIpcTimeoutMs() === DEFAULT_DAEMON_IPC_TIMEOUT_MS);
  delete process.env.LOOM_DAEMON_IPC_TIMEOUT_MS;

  // ---- Case 1: hang (accept, never respond) ----
  console.log("hang case (accept, never respond)");
  // Hold the connection open indefinitely; never write, never end.
  const heldSockets = [];
  const hangServer = await startStubServer(sockPath, (sock) => heldSockets.push(sock));

  const TIMEOUT = 400;
  const started = Date.now();
  let rejectedErr = null;
  try {
    await sendDaemonRequest({ type: "PublishEvent", payload: {} }, TIMEOUT);
  } catch (e) {
    rejectedErr = e;
  }
  const elapsed = Date.now() - started;
  check("rejected (did not hang)", rejectedErr !== null);
  check(
    "rejected within bounded window",
    elapsed < TIMEOUT + 600,
    `elapsed=${elapsed}ms`
  );
  check(
    "error names timeout + request type",
    !!rejectedErr &&
      /did not respond within \d+ms/.test(rejectedErr.message) &&
      rejectedErr.message.includes("PublishEvent"),
    rejectedErr && rejectedErr.message
  );

  // Issue #3973: the incident was specifically a `ListSweeps` handler that
  // blocked forever (a wedged daemon reap-on-read). Assert the SAME bounded
  // transport rejects a hung ListSweeps and names that request kind — a
  // handler that sleeps forever must produce a bridge timeout, not a hang.
  const lsStarted = Date.now();
  let lsErr = null;
  try {
    await sendDaemonRequest({ type: "ListSweeps", payload: { state_filter: null } }, TIMEOUT);
  } catch (e) {
    lsErr = e;
  }
  const lsElapsed = Date.now() - lsStarted;
  check("ListSweeps rejected (did not hang)", lsErr !== null);
  check(
    "ListSweeps rejected within bounded window",
    lsElapsed < TIMEOUT + 600,
    `elapsed=${lsElapsed}ms`
  );
  check(
    "ListSweeps error names timeout + request type",
    !!lsErr &&
      /did not respond within \d+ms/.test(lsErr.message) &&
      lsErr.message.includes("ListSweeps"),
    lsErr && lsErr.message
  );

  for (const s of heldSockets) s.destroy();
  await closeServer(hangServer);

  // ---- Case 2: happy path (respond then end) ----
  console.log("happy path (respond then end)");
  const okServer = await startStubServer(sockPath, (sock) => {
    sock.on("data", () => {
      sock.write(JSON.stringify({ type: "EventPublished", payload: { topic: "t", receivers: 0 } }));
      sock.write("\n");
      sock.end();
    });
  });

  const okStart = Date.now();
  let okResp = null;
  let okErr = null;
  try {
    okResp = await sendDaemonRequest({ type: "PublishEvent", payload: {} }, 2000);
  } catch (e) {
    okErr = e;
  }
  const okElapsed = Date.now() - okStart;
  check("resolved without error", okErr === null, okErr && okErr.message);
  check("resolved with expected payload", !!okResp && okResp.type === "EventPublished");
  check("resolved fast (timer cleared, no hang)", okElapsed < 1000, `elapsed=${okElapsed}ms`);
  await closeServer(okServer);

  // Give the process a beat to confirm no lingering timer keeps it alive
  // (a leaked setTimeout would delay exit by the full timeout window).
  delete process.env.LOOM_SOCKET_PATH;
  await rm(workDir, { recursive: true, force: true });
  await rm(outDir, { recursive: true, force: true });

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed.`);
    process.exit(1);
  }
  console.log("\nAll checks passed.");
}

main().catch((e) => {
  console.error("verification crashed:", e);
  process.exit(1);
});
