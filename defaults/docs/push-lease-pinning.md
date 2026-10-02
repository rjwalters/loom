# Pinning `--force-with-lease` (shared-clone push safety)

**Rule**: in a Loom clone, never push with a bare `git push --force-with-lease`.
Always pin the expected value — `--force-with-lease=<branch>:<oid>` — to the
remote head the work being pushed is actually based on, captured **before** that
work rewrote the branch.

Filed as issue #9487 after a live data-loss near-miss on PR #9483.

## Why the bare flag is unsafe here

`git push --force-with-lease` with no `=<ref>:<expect>` value compares the
remote head against the **local remote-tracking ref**
`refs/remotes/origin/<branch>`. That is not the remote; it is a local cache of
it — and in a Loom clone it is a cache **shared by every linked worktree**.
`.loom/worktrees/issue-N` are `git worktree` links: one object store, one set of
remote-tracking refs, across every concurrently running agent.

So a sibling agent that pushes and then fetches — or that merely fetches —
fast-forwards *your* lease value to the commit *they* just published. The lease
is then satisfied by construction, your push is **accepted**, and their commit
is deleted. There is no error, no rejection, and no conflict marker: the check
compared against a ref somebody else updated, not against truth.

This is the same class of hazard as `refs/stash` being one stack shared by every
linked worktree (#4821 / #5754) — shared mutable state that looks per-worktree.

## The incident (PR #9483, 2026-09-29)

Two Doctor agents were dispatched against the same PR after a Judge rejection.
The first fixed and pushed (`23ad41605`, 09:15Z) and Judge approved it
(`loom:pr`, 09:27Z). The second, working from a view taken before that push,
pushed with a bare `--force-with-lease` at 09:38Z — **and it succeeded**,
overwriting an already-approved commit. It also invalidated green CI and
regressed the PR's label state.

The two independently derived fixes happened to be byte-identical, so nothing
was lost that time. Had they differed, the first Doctor's approved work would
have been destroyed silently.

## The trap: do not "freshen" the pin

The intuitive repair — `git fetch origin <branch>` immediately before the push,
then pin to the just-fetched value — **reintroduces the bug**. A pin read after
the sibling pushed *is* the sibling's commit, so the lease passes and the
clobber proceeds, now laundered as a "fresh" value.

The pin must be the head the pushing work is **based on**:

- **Doctor**: `CLAIM_HEAD_SHA`, the PR's `headRefOid` captured at claim time
  (step 2). Re-pin to a newer head **only** after deliberately rebasing onto it.
- **A rebase/publish script**: the branch's live remote head read **before** the
  rebase runs.

## Fails closed, not open

An expected value git cannot resolve locally — a sibling's commit this clone
never fetched — makes the push **rejected** (`! [rejected] … (stale info)`),
never accepted. The pinned form therefore fails closed in exactly the situation
where the bare form fails open.

## The helpers

`defaults/scripts/lib/push-lease-verify.sh` carries both halves of lease
truthfulness. Its own header is the authoritative API reference:

| Function | Use |
|---|---|
| `push_lease_live_tip <remote> <branch>` | Read the **live** remote head (`git ls-remote`, never a tracking ref). Call it where the branch state is read, and keep the value. |
| `push_lease_pin_flag <branch> <oid>` | Build `--force-with-lease=<branch>:<oid>`. Refuses an empty oid — there is deliberately no bare-flag fallback. |
| `push_lease_require_incorporated <oid> <local-ref>` | Refuse when the remote head is not an ancestor of what is being pushed, i.e. the remote holds commits this clone never incorporated. The pin would be accurate and the push would still delete them. |
| `push_landed_despite_rejection <remote> <branch> <sha>` | The other half (#6695): a *reported* rejection for an update that actually landed, via the Git-LFS pre-push hook race. |

Pinned callers today: `defaults/scripts/reconcile-stack.sh`,
`defaults/scripts/rebase-stacked-children.sh`, and every push recipe in the
Doctor role prompt (§"Pin the lease").

## Regression coverage

- `defaults/scripts/tests/test-reconcile-stack.sh` scenario **D2** reproduces
  the incident with real git: a sibling pushes *and* fetches between the pin and
  the push, so the shared tracking ref agrees with the moved remote. The pinned
  lease is rejected and the sibling's commit survives. Reverting the push to the
  bare flag makes that scenario fail with exit 0 — the clobber — which is the
  negative control for the fix.
- Scenario **D** covers a commit that landed *before* the run: refused as a
  precondition failure, before the rebase mutates anything.
- `defaults/scripts/tests/test-rebase-stacked-children.sh` scenarios **(c)**,
  **(j)**, **(k)** assert the pinned push argument, that the pin is read before
  the rebase, and the two refusals.
