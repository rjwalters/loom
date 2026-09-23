/**
 * Browser-navigable forge URLs for the identifiers the telemetry carries.
 *
 * Every `repo` in the fleet snapshot is an `owner/repo` slug taken from the
 * checkout's `origin` remote (`.loom/docs/telemetry-schema.md`), so — like
 * `sweepTimelineView.ts`'s `prLinkFor` — these resolve against github.com.
 * The same field falls back to a workspace-root *path* when no GitHub remote
 * resolves, and the public view nulls it for a private repo, so every builder
 * here returns `undefined` for anything that is not a clean two-segment slug
 * rather than emitting a link that 404s or leaks a path.
 */

import { el } from "./dom";

const SLUG = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;

/** `text` as an off-site link when `href` resolved, or as the same plain
 * `<span>` the views rendered before links existed when it did not — a redacted
 * private repo, a path-shaped slug, an issue-less sweep — so nothing in the
 * card changes shape based on linkability. */
export function forgeLink(
  text: string,
  href: string | undefined,
  className: string,
  title?: string,
): HTMLElement {
  if (href === undefined) return el("span", { class: className, title }, text);
  return el(
    "a",
    { class: `${className} forge-link`, href, title, target: "_blank", rel: "noopener noreferrer" },
    text,
  );
}

/** `https://github.com/<owner>/<repo>` for a well-formed slug, else `undefined`. */
export function repoUrl(repo: string | undefined): string | undefined {
  if (!repo || !SLUG.test(repo)) return undefined;
  return `https://github.com/${repo}`;
}

/** The issue page for a sweep's `issue` in `repo`. */
export function issueUrl(repo: string | undefined, issue: number | undefined): string | undefined {
  const base = repoUrl(repo);
  if (base === undefined || issue === undefined) return undefined;
  return `${base}/issues/${issue}`;
}

/** The branch page for the sweep's `feature/issue-<N>` worktree branch —
 * the name `.loom/scripts/worktree.sh` always assigns, so no branch name
 * has to travel in the telemetry. GitHub's branch page surfaces the open PR
 * banner and the commits, which is what "where is this sweep's work" wants. */
export function branchUrl(repo: string | undefined, issue: number | undefined): string | undefined {
  const base = repoUrl(repo);
  if (base === undefined || issue === undefined) return undefined;
  return `${base}/tree/feature/issue-${issue}`;
}

/** Phases at which the sweep's branch exists on the forge. `curator` (and a
 * sweep that has not reported a phase) runs before any worktree is pushed,
 * so linking to the branch then would 404. */
const BRANCH_PHASES = new Set(["builder", "judge", "doctor", "merge"]);

/** Where a sweep's `#N` label should go: the branch once Builder has pushed
 * one, the issue before that. `undefined` when there is nothing linkable. */
export function sweepWorkUrl(
  repo: string | undefined,
  issue: number | undefined,
  phase: string | undefined,
): string | undefined {
  return phase !== undefined && BRANCH_PHASES.has(phase)
    ? branchUrl(repo, issue)
    : issueUrl(repo, issue);
}

/** Tooltip companion to `sweepWorkUrl`, naming the destination. */
export function sweepWorkTitle(issue: number | undefined, phase: string | undefined): string | undefined {
  if (issue === undefined) return undefined;
  return phase !== undefined && BRANCH_PHASES.has(phase)
    ? `Branch feature/issue-${issue} on the forge`
    : `Issue #${issue} on the forge`;
}
