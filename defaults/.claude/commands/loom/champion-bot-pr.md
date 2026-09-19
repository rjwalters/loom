# Champion: Trusted-Bot Dependency PRs (opt-in)

Load this **only** when `champion-pr-merge.md` → "Trusted-Bot Dependency-PR
Pre-Check" told you to. It is inert in a repo that has not opted in. Operator
reference (config keys, the full allowlist, reason codes):
`.loom/docs/champion-bot-prs.md`.

## Why this class exists

A Dependabot PR is structurally unmergeable by the normal criteria:
`Cargo.toml` / `package.json` are on criterion #3's critical-file blocklist (and
*modifying a dependency manifest is the definition* of a dependency PR), and the
bumps queue faster than the pipeline drains them, so they age past criterion #5
into Doctor — which has no idea that `@dependabot rebase` is the correct refresh
mechanism. The steady state is unpatched security advisories accumulating
forever.

**CI-green is the real gate for a version bump**, and a squash-merged bump is
the easiest class of change to revert. So for a PR that is provably nothing but
dependency-manifest churn, criterion #3 is waived and criterion #5's Doctor
route is replaced by a rebase request. Everything else is enforced exactly as
before.

> There is no size ceiling to waive: `champion.auto_merge_max_lines` is retired
> (criterion #2's migration note), and criterion #2 already forbids justifying a
> hold by a line count. A generated lockfile's +621/−558 is not a risk axis.

## Step 1: Classify (never by eye)

Do **not** judge "is this dependency-only?" yourself. `.loom/scripts/champion-bot-pr.sh`
decides it deterministically, and you quote its answer. (#4613 is the precedent:
a Champion pass asserted "no critical-file changes" about a PR that removed a
workflow file, because it restated a file-list claim from memory instead of from
the list.)

```bash
REPO="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"

# Author from the forge, never from the branch name or title — both are
# forgeable by a human push. Plain `gh`, NOT "$GH_READ": this gates a merge.
BOT_AUTHOR="$(gh pr view "$PR_NUMBER" --json author --jq '.author.login')"
BOT_TITLE="$(gh pr view "$PR_NUMBER" --json title --jq '.title')"
gh pr view "$PR_NUMBER" --json body --jq '.body' > /tmp/botpr-body-$PR_NUMBER.txt

# The AUTHORITATIVE file list — paginated REST, never `gh pr view --json files`
# (silently truncates at 100 entries, #4613).
gh api "repos/$REPO/pulls/$PR_NUMBER/files" --paginate --jq '.[].filename' \
  > /tmp/botpr-files-$PR_NUMBER.txt

eval "$(./.loom/scripts/champion-bot-pr.sh classify \
  --author "$BOT_AUTHOR" \
  --title "$BOT_TITLE" \
  --body-file /tmp/botpr-body-$PR_NUMBER.txt \
  --files-from /tmp/botpr-files-$PR_NUMBER.txt \
  < <(gh pr diff "$PR_NUMBER"))"

echo "qualifies=$BOT_PR_QUALIFIES reason=$BOT_PR_REASON detail=$BOT_PR_DETAIL"
```

- **`BOT_PR_QUALIFIES=false`** → **stop reading this file.** Return to
  `champion-pr-merge.md` and evaluate criteria #1–#6 completely unmodified, as
  if this feature did not exist. This is the required fallback for *every*
  non-qualifying reason: a bot PR that also edits code, a bump over the semver
  ceiling, an untrusted author. Do not partially waive anything.
- **`BOT_PR_QUALIFIES=true`** → continue below.

## Step 2: The criteria that still apply, in full

| Criterion | Under this class |
|---|---|
| #1 Label check (`loom:pr`, no contradicting verdict label) | **Unchanged.** Judge approval is still required — Champion never picks a bot PR up straight from `loom:review-requested`. |
| #2 Merge-risk judgment | **Unchanged.** A standing `loom:operator` hold still binds. |
| #3 Critical-file exclusion | **Waived** — replaced by the classifier's dependency-manifest allowlist, which is strictly narrower. |
| #4 Mergeable | **Unchanged, hard.** |
| #5 Recency | **Waived as a Doctor route** — see Step 4. A stale bot PR gets a rebase request, never Doctor. |
| #6 CI status | **Unchanged, hard. This is the actual safety gate.** Every check `success` or `skipped`; any `failure`/`cancelled`/`pending` blocks the merge exactly as before. |

Run criteria #1, #2, #4 and #6 from `champion-pr-merge.md` verbatim. If any
fails, handle it with that file's normal path for that criterion.

## Step 3: Merge, with the waiver on the record

The pre-merge comment from `champion-pr-merge.md` → "Step 2: Add Pre-Merge
Comment" gains one **mandatory** block naming what was waived and why — quoting
the classifier's own output, never a restatement:

```markdown
**Trusted-bot dependency PR** (`champion.autoMergeDependabot`)

- Author: `<BOT_AUTHOR>` — matched `champion.trustedBotAuthors` exactly
- Diff: <N> file(s), all dependency manifests/lockfiles (classifier verdict
  `<BOT_PR_REASON>`)
- Waived: `<BOT_PR_WAIVED>` — manifest filenames are not a risk signal for
  this class, and a stale bot PR is refreshed by its own bot, not by Doctor
- **Not waived**: Judge approval (`loom:pr`), mergeable state, and CI green —
  every check `success`/`skipped`. CI is the gate here.
- Semver: `<BOT_PR_BUMP_LEVEL>` (ceiling `<BOT_PR_MAX_SEMVER>`)
```

Then merge with `./.loom/scripts/merge-pr.sh "$PR_NUMBER"` exactly as Step 3 of
`champion-pr-merge.md` does. Nothing about the merge itself changes.

## Step 4: Stale or conflicting → ask for a rebase, never Doctor

Doctor must **never** check out a Dependabot branch: Dependabot owns it, a
human-authored force-push there is discarded on the bot's next run, and the
Doctor cycle is spent for nothing. The correct refresh mechanism is the bot's
own command.

When a qualifying PR fails criterion #4 (`CONFLICTING`) or would have been
routed to Doctor by criterion #5, post the rebase request **once per head SHA**
instead:

```bash
REBASE_MARKER="<!-- champion:dependabot-rebase-requested -->"
HEAD_SHA="$(gh pr view "$PR_NUMBER" --json headRefOid --jq '.headRefOid')"

# Already asked for THIS head? Then the bot has not acted yet — say nothing.
ALREADY="$(gh pr view "$PR_NUMBER" --json comments \
  --jq "[.comments[] | select(.body | startswith(\"$REBASE_MARKER\")) |
         select(.body | contains(\"$HEAD_SHA\"))] | length")"

if [ "$ALREADY" -eq 0 ]; then
  gh pr comment "$PR_NUMBER" --body "$REBASE_MARKER
@dependabot rebase

**Champion**: this dependency PR is out of date with \`main\` (head \`$HEAD_SHA\`).
Requesting a rebase from the bot that owns this branch rather than routing it to
Doctor — a Doctor cycle on a Dependabot branch is discarded by the bot's next run.
Re-evaluating on the next tick.

*Automated by Champion role*"
fi
```

Keep `loom:pr`. Do **not** add `loom:changes-requested`, do **not** add
`loom:blocked`, do **not** route to Doctor, and do **not** re-post while the
head SHA is unchanged — the marker plus the SHA is what makes this idempotent
across ticks. If the bot cannot rebase (it says so in a reply), that is the
point at which the normal `loom:operator` hold applies.

## What this class must never do

- **Never merge without `loom:pr`.** The Judge-bypass variant floated in issue
  #4765 was explicitly superseded by the operator ruling of 2026-09-14, which
  requires Judge approval. A bot PR sitting in `loom:review-requested` is
  Judge's to clear, not Champion's to skip.
- **Never waive CI.** A pending, failing or cancelled check blocks the merge,
  full stop. Everything else here rests on CI being the gate.
- **Never widen the allowlist by eye.** If a file "looks like" a manifest but
  the classifier rejected it, the PR falls back to the strict criteria. Widening
  the set is a change to `loom-daemon/src/bot_pr/manifest.rs` with tests, not a
  judgment call in a pass.
- **Never infer the author from the branch or title.** `dependabot/cargo/...`
  is a branch name anyone with push access can create.
