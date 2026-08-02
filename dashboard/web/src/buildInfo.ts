/**
 * The page footer's build/commit stamp (issue #4958).
 *
 * ## Why this exists
 *
 * Before this the deployed Worker had no visible link back to the commit it
 * was built from — the only way to answer "is the live dashboard current?"
 * was a `wrangler` login. The Worker now stamps the deploying commit into
 * `window.__LOOM_FLEET__.commit` (`../../src/index.ts`'s `handleRoot`, same
 * global `accountMenu.ts` reads the auth state from) whenever the CI deploy
 * workflow set `BUILD_COMMIT`; this module renders it.
 *
 * `/api/version` reports the same value for scripted checks — see that
 * route's doc — this module is the at-a-glance surface for a human looking
 * at the page.
 */

import { readAuthState } from "./api";

/** Truncated to the short SHA a person actually reads/pastes; the footer is
 * not the place for the full 40-char hash. */
function shortCommit(commit: string): string {
  return commit.slice(0, 12);
}

/**
 * Render the build-info footer into `container`, replacing its contents.
 *
 * Renders nothing (an empty footer) when no commit was stamped — a local
 * `wrangler dev` run or a Miniflare test env, where `BUILD_COMMIT` is never
 * set. That is a legitimate state, not an error, so this deliberately does
 * not render a "not built" placeholder that could be confused for a real
 * problem.
 */
export function renderBuildInfo(container: HTMLElement, scope: typeof globalThis = globalThis): void {
  const { commit } = readAuthState(scope);
  container.textContent = commit ? `build ${shortCommit(commit)}` : "";
}

/** Render into `#build-info` if the host page has one. A page without the
 * container simply has no build-info footer — not an error. */
export function wireBuildInfo(doc: Document, scope: typeof globalThis = globalThis): void {
  const container = doc.getElementById("build-info");
  if (container instanceof HTMLElement) renderBuildInfo(container, scope);
}
