---
name: loom-curator-rate-budget
description: "`gh issue view/edit/close` costs ~8-10 GraphQL requests per curated issue, from the one 5,000/hour pool the whole fleet shares; 8 parallel curators exhausted it."
---
<!-- loom-managed-skill -->
<!-- GENERATED FILE — DO NOT EDIT DIRECTLY.
     Produced by `loom-daemon generate-agent-skills` from
     defaults/.claude/commands/loom/curator-rate-budget.md (the same source Claude Code
     reads as /loom:curator-rate-budget via .claude/commands/loom/). This is the
     cross-vendor skill-discovery surface (.agents/skills/<name>/SKILL.md)
     read natively by Codex, Kimi Code, Mistral Vibe, and Grok — see
     runtime-adapters.md §5. To change this file, edit the source above
     and re-run the generator; CI (`loom-daemon generate-agent-skills
     --check`) fails if this file is stale. -->

# Curator: GraphQL/REST Budget (#10039)

`gh issue view/edit/close` costs ~8-10 GraphQL requests per curated issue, from
the one 5,000/hour pool the whole fleet shares; 8 parallel curators exhausted it.

## REST for per-issue reads/writes (`{owner}/{repo}` expands in a checkout)

- Read body/labels/state: `gh api repos/{owner}/{repo}/issues/N`; comments: `.../issues/N/comments --paginate`. Batch with one `.../issues?labels=loom:triage&state=open&per_page=50`, never a `gh issue view` loop.
- Add label: `gh api repos/{owner}/{repo}/issues/N/labels -f 'labels[]=loom:curating'`; remove: `gh api -X DELETE repos/{owner}/{repo}/issues/N/labels/loom%3Acurating`.
- Edit body: `gh api -X PATCH repos/{owner}/{repo}/issues/N -F body=@file` (capital `-F`); verify the prior read succeeded first.
- Close: `gh api -X PATCH repos/{owner}/{repo}/issues/N -f state=closed -f state_reason=not_planned`.

The gh-cached path is an acceptable alternative where one exists.

## Check both pools before claiming each issue

```bash
gh api rate_limit --jq .resources.core.remaining
gh api graphql -f query='{rateLimit{used remaining resetAt}}'
```

Back off (stop claiming, report the rest as deferred, no retry loops) when
either remaining is under ~500, or the GraphQL read is itself refused.

## Parallelism

Run at most ~3 Curators at once per identity (all repos combined); fewer as the pool drains.

## Measurement recipe (NOT yet measured)

Read `used` from both pools before and after one 15-issue batch with no other
agents on the identity; report the deltas (expected GraphQL ~0 beyond budget reads).
