# Fork Drift Guide

This repository (`rjwalters/loom`) is the upstream that other repos fork —
most notably [`gpeyton/loom`](https://github.com/gpeyton/loom), which diverged
by 115 commits before anyone noticed (**#4165**), forcing a large one-shot
harvest triage (**#4190**–**#4200**). ADR-0012
(`docs/adr/0012-runtime-adapter-contract.md`) commits us to ongoing
collaboration with that fork — and likely future ones, e.g.
`mattcproctor/loom`, the origin of the fork's own port of this workflow — so
divergence will keep accruing. This guide explains the automated
drift-monitoring workflow and the manual procedure for harvesting fork-only
work back into this repo.

## The drift-monitoring workflow

`.github/workflows/fork-drift.yml` runs a daily check (disabled by default —
see below) that, for each tracked fork:

1. Fetches the fork's `main` branch and computes commit counts in both
   directions (`git rev-list --count`):
   - **fork-ahead** (`main..fork/main`) — fork-only commits not yet in this
     repo. This is the **harvestable work** signal.
   - **fork-behind** (`fork/main..main`) — commits this repo has that the
     fork hasn't synced. Reported as divergence-risk context only; syncing
     that direction is the *fork's own* drift workflow's job, not ours.
2. Writes both counts, per tracked fork, to the GitHub Actions job summary
   (`$GITHUB_STEP_SUMMARY`) for every run, whether or not drift was detected.
3. If a fork is ahead (fork-ahead > 0), opens **one** deduplicated
   operator-facing issue titled `chore: review <fork> fork drift`. If that
   issue is already open, the workflow adds a comment with refreshed counts
   instead of opening a duplicate — re-running the workflow while drift
   persists never spams new issues, it just updates the existing one.
4. If a tracked fork is unreachable (renamed, deleted, network failure), the
   step fails loudly for that fork with a clear `::error::` annotation rather
   than silently reporting zero drift or filing a bogus report.

### Tracked forks

The `TRACKED_FORKS` env var in the workflow is a newline-delimited list of
`owner/repo` entries. It is currently seeded with `gpeyton/loom` only.
`mattcproctor/loom` is a known second fork, listed commented-out in the
workflow as an example — uncomment it there if it starts to warrant recurring
drift observation.

### Enabling the schedule

Like the other `.github/workflows/loom-*.yml` support-role workflows, the
daily cron trigger ships **commented out** so Actions minutes aren't burned
without an explicit opt-in. To enable it:

1. Open `.github/workflows/fork-drift.yml` and uncomment the `schedule:` /
   `- cron:` lines.
2. Commit the change.

No additional secrets are required — the workflow uses the default
`GITHUB_TOKEN` (`issues: write`, `contents: read`), no `CLAUDE_API_KEY`.

### Manual smoke test

Trigger a one-off run at any time via the Actions UI ("Run workflow" on the
"Fork Drift Monitor" workflow) or:

```bash
gh workflow run fork-drift.yml
```

This is safe to run repeatedly — the dedup logic (exact title match, not
substring) means a second run while drift is still present updates the
existing issue rather than creating a new one.

## Interpreting the report

When the workflow opens (or updates) a `chore: review <fork> fork drift`
issue, the body/comment reports:

- **Fork-ahead** — how many fork-only commits are not yet harvested into this
  repo. This is the number to act on.
- **Fork-behind** — how many commits this repo has that the fork hasn't
  synced (context only; not actionable from this side).

The issue is labeled `loom:operator-only` when that label exists on the repo
(see `.github/labels.yml`: "Requires human action outside automation") —
deciding *what* to harvest and *how* (cherry-pick vs. invite a fork PR) is a
human/triage call, and the label keeps this recurring report out of the
autonomous work-finder's queue.

## Harvest procedure

1. **Fetch the fork:**
   ```bash
   git remote add fork-gpeyton-loom https://github.com/gpeyton/loom.git 2>/dev/null || \
     git remote set-url fork-gpeyton-loom https://github.com/gpeyton/loom.git
   git fetch fork-gpeyton-loom main
   ```

2. **Review the diff** before triaging anything:
   ```bash
   git log --oneline main..fork-gpeyton-loom/main   # fork-only commits
   git show <sha>                                   # inspect a specific commit
   ```

3. **Triage each commit or logical bucket** into one of two paths, following
   the pattern used by the original #4165 triage (its output was the
   #4190–#4200 sibling harvest issues):
   - **Cherry-pick candidate**: file a `loom:triage` issue describing the
     commit(s), the adaptation needed (if any — direction reversals, this
     repo's naming/labels, etc.), and a link to the source commit. Let the
     issue lifecycle (`loom:triage` → Curator → `loom:issue` → Builder) pick
     it up normally.
   - **Runtime-neutral surface**: per ADR-0012, prefer inviting a PR *from*
     the fork rather than cherry-picking — comment on the dedup issue (or a
     new issue) asking the fork maintainer to open the PR upstream. This
     preserves attribution and reduces re-divergence for shared-contract work
     (spawn dispatcher, error-classification tables, runtime adapters, etc.).
   - It is also fine to **explicitly decline** a commit (fork-specific
     design decision that doesn't apply upstream) — note the reasoning in a
     comment rather than leaving it silently unaddressed.

4. **Close the dedup issue** with a comment once the fork-ahead drift for
   that fork has been fully triaged (harvested via issues/PRs, or explicitly
   declined). The next drift check will reopen a fresh dedup issue if new
   fork-ahead commits accumulate afterward.

## See also

- `.github/workflows/fork-drift.yml` — the workflow itself
- **#4165** — the fork-divergence triage this workflow follows up on
  (closed; this workflow is its drift-containment follow-up)
- ADR-0012 (`docs/adr/0012-runtime-adapter-contract.md`) — the fork
  collaboration model; scopes the PR-over-cherry-pick rule to runtime-neutral
  work
- **#4190**–**#4200** — sibling harvest issues from the #4165 triage; the
  pattern step 3 above follows
- Fork PR `gpeyton/loom#45` / commit `3c80bec9` — source material this
  workflow and guide were adapted from (direction reversed: that workflow
  watches upstream from the fork's side; this one watches forks from
  upstream's side)
- `docs/guides/development.md` — general contribution setup
- `docs/guides/testing.md` — running the full test matrix
