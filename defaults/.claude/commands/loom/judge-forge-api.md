---
disable-model-invocation: true
---
# Judge: GraphQL Rate-Limit Exhaustion — REST Fallback for Labels/Comments

**Load when**: a `gh` write (comment, label edit, issue create) fails with a rate-limit signature, or you need the REST equivalent of a GraphQL mutation.

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

## GraphQL Rate-Limit Exhaustion — REST Fallback for Labels/Comments

`gh pr comment` and `gh pr edit` (both mandatory for every verdict — see the
"CRITICAL" note below) are **GraphQL-backed mutations**. GitHub's GraphQL
quota (5000/hr, shared across every agent + tool) and its REST quota are
**independent** — confirmed live during long sweeps (#4526, #4670, #4856):
GraphQL can read 0 remaining while REST still has ~4000 left. A rejection
whose text contains one of these five signatures (case-insensitive; `·` is a
space, #11411) is a rate limit, not a real failure, and has a REST
equivalent — do **not** give up or wait idly; retry the same mutation over REST:

| Signature | Seen as |
|---|---|
| `api rate limit·exceeded` | REST throttling (rare here) |
| `api rate limit·already exceeded` | GraphQL, as `GraphQL: … for user ID …` |
| `secondary rate limit` | burst throttling |
| `abuse detection mechanism` | burst throttling |
| `was submitted too quickly` | burst throttling |

A "GitHub API rate limit … sleep until reset" reminder on a tool result is a
text match on its output (#11411), not a quota reading: check
`gh api rate_limit` and keep working. Read role files with a file-read tool,
not `cat`.

REST equivalents for the mutations you actually need mid-review:

```bash
# gh pr comment <n> --body "..."   ->
gh api "repos/{owner}/{repo}/issues/<n>/comments" -F body="..."

# gh pr edit <n> --add-label "loom:pr"   ->
gh api "repos/{owner}/{repo}/issues/<n>/labels" -f "labels[]=loom:pr"

# gh pr edit <n> --remove-label "loom:reviewing"   ->
gh api "repos/{owner}/{repo}/issues/<n>/labels/loom%3Areviewing" -X DELETE
#                                                        ^^^ the ":" in a label
#   name must be percent-encoded as %3A in the DELETE path segment.
```

(The PR's **general conversation** comments and its labels live under
`/issues/<n>/...` — GitHub treats a PR as an issue for labels, comments and
state, and there is no `/pulls/<n>/labels`. **`/pulls/<n>/comments` does
exist**, and is a *different* thing: the PR's **inline, diff-anchored review
comments**. Reading only `/issues/<n>/comments` therefore sees the
conversation and none of the formal review feedback — the exact blind spot
behind #7647; see "Formal Review & Inline Thread Reconciliation" below.)
`gh api` expands the
literal `{owner}/{repo}` placeholder from the git remote with zero API calls
of its own — never resolve it via `gh repo view --json nameWithOwner`, which
is itself GraphQL-backed and fails first under the same exhaustion this
fallback exists for (#4659). Anything else — auth failure, network error, a
404 on a bad PR number — is **not** a rate limit; report it and do not
retry over REST. `merge-pr.sh`'s `lib/forge-helpers.sh` implements this same
signature table plus ready-made wrappers
(`forge_gh_comment_rl_safe`, `forge_gh_swap_label_rl_safe`,
`forge_gh_reopen_issue_rl_safe`, #4856, and `forge_gh_create_issue_rl_safe`,
#5047) if you are scripting rather than running `gh` interactively.

**Filing a follow-up issue has the same exposure, and its own tool.** `gh issue
create` is GraphQL-backed too, so it dies on the same exhaustion — use
`./.loom/scripts/create-issue.sh` instead of a bare `gh issue create` (#5047).
Same flags (`--title`, `--body`/`--body-file`, repeatable `--label`, `--repo`),
same printed issue URL, but it falls back to one REST POST that applies labels
**atomically with creation** (never create-then-label). Full rationale:
`.loom/docs/github-authentication.md` → "Filing issues under GraphQL
exhaustion". `loom-daemon forge issue create` is a byte-identical `gh`
passthrough and is **not** an alternative.

**This section covers labels/comments only** — `gh issue create` (used below
under "Creating Follow-up Issues" and "Raising Concerns") is a separate
GraphQL mutation with its own REST fallback: `.loom/docs/gh-issue-create-rest-fallback.md`
(or `forge_gh_create_issue_rl_safe` in the same `lib/forge-helpers.sh`, #5047).

**`GraphQL: Body is too long` is a different, non-rate-limit rejection — do
not apply this REST fallback to it (#6930).** A PR/issue body edit that
exceeds GitHub's ~256 KiB hard cap is not in the signature table above and is
not a quota problem — REST's `PATCH .../issues/{n}` "succeeds" only because
it skips the same size check, and a blind PATCH there can silently clobber a
concurrent writer's edit with no error raised. If you hit this rejection
while writing a PR body, switch to posting the update as a comment instead —
full incident and rationale in `.loom/docs/graphql-body-size-cap.md`.
