# ADR-0022: Merge by Default — Merge Commits Over Squash, With Rebase Second

## Status

Accepted (issue #9105)

## Context

Loom's merge-method resolution had been biasing toward squash for two
historical reasons, and both have since lost the ground under them:

1. **Historic hardcode.** Every merge call site hardcoded `squash` (#7754
   replaced that with auto-detection), but the *preference order* the
   auto-detecters use — `squash > merge > rebase` in the shell runtime
   (`forge-merge-method.sh`, `scripts/install/forge-detect.sh`,
   `defaults/scripts/lib/forge-helpers.sh`) and the Rust twin (`loom-daemon`
   `forge_merge_method`), and the installer's degenerate fallback
   (`setup-repository-settings.sh`) — all still encode squash as the
   least-surprising default.
2. **The ancestor rationale is void.** Squash was once chosen so the
   **parent's** merge commits stayed `git merge-base` ancestors of the child
   branch for stacked-PR reconciliation. Stack reconciliation is now handled
   explicitly by `reconcile-stack.sh` / `merge-pr.sh` (the post-merge choke
   point, the #3747 v2 pre-merge ordering guard, the #3752 amend rebase),
   using `git rebase --onto` under **any** merge method. Stacked
   reconciliation no longer depends on the parent's merge-commit ancestry,
   so the reason to prefer squash is gone.

In addition, Loom's own `blame-issue.sh` offline resolution of
"line → PR → issue → role reporter" had to be taught to match the
`Merge pull request #N from ...` merge-commit subject (this repo itself is
currently configured squash-only on GitHub, so every `git log` line a
Builder lands today only carries the `(#N)` squash-form join key incidentally
— merge commits carry it by construction).

Squash-merging a branch discards the branch's commit history from `main`:
every commit, every author identity, every timestamp, and the
commit-to-PR/issue join key for lines introduced by non-final commits.
Loom's blame/audit surface (`blame-issue.sh`, the role reports it feeds)
wants exactly that record. The operator decision (see issue #9105): merge
commits are the repository default going forward, across **every**
Loom-managed repo, not just this one.

## Decision

**Merge commits are Loom's default merge method.** Concretely:

1. **Preference order inverts.** Every merge-method resolution site —
   `forge_detect_merge_method` (shell runtime), `detect_merge_method`
   (installer twin), `resolve_merge_method` (Rust daemon), and the
   `forge_merge_pr` / `auto-merge` plumbing defaults — now prefers
   **`merge > rebase > squash`** when a repo allows more than one strategy:
   merge commits first (full history preserved), rebase second, squash only
   when it is the repo's *only* allowed strategy.
2. **Fail-open becomes `merge`.** On probe failure (network/auth),
   unparseable responses, or a forge reporting every allow-flag false, the
   resolvers now fail open to **`merge`** rather than `squash`. A repo that
   genuinely disallows merge commits will then **fail the merge loudly**
   (the forge rejects the requested method) instead of silently squashing
   history — a loud, fixable failure beats a silent rewrite.
3. **Squash-only repos are still respected (no forced migration).**
   `requested_merge_method_allowed` (#8845) validation is unchanged: an
   explicit `--merge-method` request and the repo's configured strategies
   still govern. A repo configured squash-only keeps receiving squash
   merges; the preference only decides *among what is already allowed*.
   Repos currently configured squash-only are migrated to
   merge-commit-only by an operator action (PATCH of the repo settings —
   recorded in the #9105 PR body as a live change), not by the merge path.
4. **Installer degenerate fallback enables merge-commit-only.** When
   `setup-repository-settings.sh` must create settings from scratch (the
   degenerate all-false state neither forge's UI permits), it now enables
   **merge commit only** (`allow_merge_commit: true`, squash and rebase
   disabled; Gitea: `allow_merge_commits: true`, `default_merge_style:
   "merge"`) instead of squash-only. The `respect_existing` guarantee
   (#7754) is unchanged: a repo that allows *any* strategy is left alone.
5. **Prose follows the mechanism.** Every role prompt, doc, and script
   header that asserted "this repo squash-merges" as a *current* rule is
   updated to the merge-commit default (historical incident narratives that
   record a squash merge having happened are left as-is). `blame-issue.sh`
   and its test fixture now match merge-commit subjects offline, alongside
   the squash `(#N)` suffix.

## Consequences

- **History is the default record.** `main` keeps every commit, author, and
  timestamp of every merged branch; `blame-issue.sh`, role reports, and any
  future audit tooling can join line → commit → PR → issue → role without a
  squash-message convention. The `(#N)` squash suffix is no longer load-
  bearing for repo history (it remains the offline join key for
  squash-configured repos).
- **Loud failures at the forge.** A merge commit requested on a
  squash-only repo is rejected by the forge and surfaces as a normal merge
  failure instead of a silent history rewrite. Operators migrating
  long-squash repos to merge commits do it explicitly (settings PATCH),
  which is what the #9105 PR body documents for this repository.
- **Stacked-PR reconciliation is unchanged in behavior.**
  `reconcile-stack.sh`'s `git rebase --onto <default> <parent> <child>`
  works identically under merge commits (the child's parent-branch commits
  are then already ancestors of `main`, which only makes the re-root
  cleaner); the #3747 v2 pre-merge ordering guard and the #3752 post-amend
  child rebase are merge-method-agnostic and are unaffected.
- **`git revert` semantics differ.** Reverting a merge-commit-merged PR
  requires `git revert -m 1 <merge-sha>` (main's parent) rather than a
  plain `git revert <squash-sha>`; the Champion revertability criterion and
  its auto-merge comment now say so.
- **Squash remains available, never forced on top of configuration.**
  `--merge-method squash` still works where the repo allows it, and
  squash-only repos keep squashing. This is a *default and direction*,
  not a coercion.
- **No daemon behavioral surface changes beyond the resolver.** The merge
  APIs, `--merge-method` validation, Gitea decline behavior, and the
  partial-increment / stacked-PR machinery are all untouched; the change is
  confined to which method gets *chosen* by default and what the installer
  writes for a settings-less repo.