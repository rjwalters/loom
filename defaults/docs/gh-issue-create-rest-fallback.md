# `gh issue create` REST fallback (GraphQL rate-limit exhaustion)

`gh issue create` is a **GraphQL-backed mutation**. GitHub's GraphQL quota
(5000/hr, shared across every agent + tool) and its REST quota are
**independent** — the same fact `judge.md`'s "GraphQL Rate-Limit
Exhaustion — REST Fallback for Labels/Comments" section and #4856's
`forge_gh_comment_rl_safe` / `forge_gh_reopen_issue_rl_safe` /
`forge_gh_swap_label_rl_safe` already rely on for labels, comments, and
reopen. Before #5047, issue **creation** was the one filing mutation left
uncovered: every role prompt that files issues (architect, auditor,
builder-complexity, builder-pr, curator, doctor, hermit, hermit-patterns,
judge) died outright when GraphQL exhausted, even though the REST pool
typically sits nearly untouched (observed live: `core` 19/5000 used vs.
`graphql` 1378/5000 used).

This page is the **single source** for the fallback recipe — role prompts
link here rather than repeating it. If the recipe needs to change, change it
here (and in `forge_gh_create_issue_rl_safe`, its scripted equivalent), not
in nine separate files.

## The five-signature table

A rejection whose text contains one of these (case-insensitive) is a rate
limit, not a real failure — retry the same creation over REST instead of
giving up:

| Signature | Seen as |
|---|---|
| `api rate limit exceeded` | REST itself throttling (rare on the fallback path) |
| `api rate limit already exceeded` | GraphQL: `GraphQL: API rate limit already exceeded for user ID …` |
| `secondary rate limit` | either transport, burst throttling |
| `abuse detection mechanism` | either transport, burst throttling |
| `was submitted too quickly` | either transport, burst throttling |

Anything else — auth failure, network error, a validation error (e.g. a
nonexistent label) — is **not** a rate limit; report it and do not retry
over REST.

## The recipe: atomic create + label, never create-then-label

**Labels must be applied in the same request as creation**, on both the
primary and fallback paths. A create-then-label two-step doubles the request
count (worse under the exact quota pressure this fallback exists for) and
can half-fail, leaving an unlabeled issue behind.

```bash
# Primary path — unchanged, still try this first:
gh issue create --repo "$NWO" --title "$TITLE" --body "$BODY" --label "$LABEL"

# On a rate-limit rejection (see the signature table above), fall back to a
# REST POST with labels in the SAME payload:
jq -n --arg t "$TITLE" --arg b "$BODY" --arg labels "$LABEL" \
  '{title: $t, body: $b, labels: ($labels | split(","))}' > payload.json
gh api --method POST "repos/$NWO/issues" --input payload.json --jq '.html_url'
```

`--input payload.json` (a real file) also sidesteps the guard false positive
where a heredoc body containing `>=` gets misclassified as a Bash redirect,
and correctly handles a multi-line/markdown body — unlike `-f`/`--raw-field`,
which does not expand `@file` and would post the literal string (the same
`-f` vs. `-F` trap documented in `judge.md` for comments).

## Scripted callers: `forge_gh_create_issue_rl_safe`

If you are scripting rather than running `gh` interactively, use the
ready-made wrapper in `lib/forge-helpers.sh` instead of hand-rolling the
above:

```bash
source "$(dirname "${BASH_SOURCE[0]}")/lib/forge-helpers.sh"

# forge_gh_create_issue_rl_safe NWO TITLE BODY [LABELS_CSV]
url=$(forge_gh_create_issue_rl_safe "$NWO" "$TITLE" "$BODY" "loom:triage,bug")
```

It tries `gh issue create` first, falls back to the REST POST above on a
rate-limit rejection (applying labels atomically either way), prints the
created issue's URL on success, and propagates any non-rate-limit failure
without attempting the REST call. GitHub-only, like the sibling `*_rl_safe`
helpers — gate calls on `FORGE_TYPE == github` the same way callers already
gate `forge_gh_comment_rl_safe` et al.

## `loom-daemon forge issue create` does NOT get this fallback

`loom-daemon forge issue <args…>` is a byte-identical passthrough to `gh
issue <args…>` (see `loom-daemon/src/forge_cmd.rs`'s `gh_passthrough`) — it
execs the real `gh` binary with the same arguments and inherits the same
GraphQL cost, with **no** REST-fallback interception for `issue create`
specifically (the passthrough is generic across every `gh issue` subcommand,
not create-aware). It is not a safe alternative to reach for under GraphQL
exhaustion. Use the recipe or helper above instead.

## Serialize issue creation, fallback or not

The REST fallback does not change the existing serialization requirement
(#3707): `gh issue create` (and its REST equivalent) returns a
server-assigned number with no client-side coordination, so concurrent
issue-filing agents in the same repo still race on issue numbers and can
cross-contaminate bodies. One issue-creating agent finishes its entire
filing burst — REST fallback included — before the next starts.
