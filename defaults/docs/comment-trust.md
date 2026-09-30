# Comment Trust: whose text counts as a control signal

Loom must be able to manage **any** repository, including public ones that
accept contributions from anyone. Everything an outsider can write (comments,
reviews, issue and PR bodies, fork branches) is therefore untrusted input. This
page states the rule for the narrow slice of that text Loom *acts on*: HTML
markers such as `<!-- loom:verdict-sha sha=… verdict=… -->` and control phrases
such as `Champion Review: APPROVED` (#9548).

For the companion rule about text an *agent* reads (prompt injection), see
[`untrusted-external-content.md`](untrusted-external-content.md).

## The rule

**Outsider text is content, not control.** A marker or phrase that changes
Loom's behaviour counts only when the forge says it was authored by a trusted
identity. A well-formed marker from anyone else is prose: it reads exactly as
if it were absent. It can neither vouch for a verdict nor invalidate one.

A comment or review is trusted when its author is:

| Author | Why |
|---|---|
| A repo insider by `author_association`: `OWNER`, `MEMBER`, `COLLABORATOR` | They can change labels anyway. `CONTRIBUTOR`, `FIRST_TIME_CONTRIBUTOR`, `FIRST_TIMER` and `NONE` never count: one merged fork PR makes anyone a contributor. |
| One of **this** fleet's GitHub Apps, matched exactly | The fleet's Apps appear as `NONE`/`CONTRIBUTOR`, so without this rule Loom could not read its own markers. The roster is [`forge_identity::FleetLogins`](github-authentication.md): the writer, every reader, `legacyLogins`, and the `loom-fleet-dispatch(-<digits>)` default family. The author must be *spelled* as an App (`x[bot]` from REST, `app/x` from GraphQL, or a `Bot` type): a user may register the bare slug, never `x[bot]`. |
| This daemon's own identity | Compared with the same account kind: the user `x` is never the App `x[bot]`. |
| A login in `forge.trustedCommenters` | An explicit allowlist, same account-kind rule (list `helper[bot]` to allow an App). |

**Another Loom installation's markers are not ours.** A foreign fleet emits
perfectly well-formed Loom markers; its Apps are not in this roster, so they
count for nothing here.

## Where it is enforced

The predicate lives once, in `loom-daemon/src/comment_trust.rs`:

- **Rust readers** filter at fetch. `claim_reconciliation`'s comment fetch
  returns trusted bodies only, so the verdict backstop, the stale-verdict
  dedup, and the base-conflict "is this flag ours?" check never see an
  untrusted marker. `star_liveness::trust` delegates to the same predicate.
- **Shell readers** call `loom-daemon forge trusted-comments`, which reads a
  comment listing on stdin and prints the trusted subset in the same shape.
  `verdict-staleness-guard.sh` and `check-promotion-landed.sh` use it.
- **A structural test** (`comment_trust::tests::verdict_sha_readers_go_through_the_trust_filter`)
  fails when a new Rust file handles `loom:verdict-sha` without being reviewed
  into the list of filtered readers.

When the filter cannot run (no `loom-daemon`, or one that predates the verb),
the answer degrades toward safety: the verdict guard treats every marker as
absent (`UNVERIFIABLE`, never `FRESH`), names the cause in `REASON`, and
suppresses `--anchor`; the promotion backstop exits 1 and reconciles nothing.
Merge paths treat an `UNVERIFIABLE` whose reason says the markers "could not be
authenticated" as a reason not to merge.

## `gh --json comments` cannot name an App

`gh issue view --json comments` (and `gh pr view`) reports an App author as the
bare slug, `{"login":"loom-fleet-dispatch"}`, indistinguishable from a user of
that name. A bare login is therefore treated as a user, and in that shape the
fleet's own comments pass only by association or allowlist. A reader that must
believe fleet-authored markers fetches the REST listing instead:

```bash
gh api "repos/{owner}/{repo}/issues/$N/comments" --paginate \
  | loom-daemon forge trusted-comments
```

## Configuration

```json
{ "forge": { "trustedCommenters": ["release-bot", "helper-app[bot]"] } }
```

Anything other than an array of logins is ignored with a warning: a malformed
value never widens trust.

## Loom writes only to repos it manages

The same principle, in the other direction: an installation that can work on
any public repository must never act as a control plane on one it does not
manage. GitHub lets any account comment on any public issue, so a write aimed
at the wrong repository does not fail. Its label edits are refused, but its
comments, markers included, land.

The wrong repository comes from `gh`, not from any explicit choice. Without
`--repo`, and for `{owner}/{repo}` and `gh repo view`, `gh` resolves the base
repository from `GH_REPO`, then a `gh repo set-default` pin, then the remotes
ranked **`upstream` > `github` > `origin`**. A fork checkout therefore reads
and writes the upstream project.

A comment, label edit, merge or lease write to `OWNER/REPO` is allowed only
when all three hold:

1. **Resolved from the checkout, the target is its `origin`.** A checkout
   that `gh` resolves elsewhere is refused, not redirected, because its reads
   go there too. If `origin` is the repository Loom manages, pin it:
   `gh repo set-default OWNER/REPO`.
2. **The repository is managed:** the `origin` of a workspace in this
   daemon's registry, or of the Loom-installed checkout the call runs in.
3. **The credential has WRITE.** A user token needs `push`, `maintain` or
   `admin`; an App installation token needs the repository in its
   installation. Probed once per repository per hour
   (`LOOM_WRITE_SCOPE_TTL_SECS`). On Gitea only rules 1 and 2 apply.

Anything unverifiable is a refusal. Reads are never gated.

- **Daemon:** `loom-daemon/src/write_scope.rs`. Claim reconciliation,
  quarantine reconciliation, star liveness and dispatch skip a refused
  workspace, logging the reason once. Explicit-target writes (roster
  heartbeat, dependency classification) check the named repository;
  `notify-cleared-blockers` and the stale-check redate take the repository
  `merge-pr.sh` already vetted. The structural test
  `write_scope::tests::daemon_write_paths_are_scoped` fails when a new daemon
  file writes to the forge without being reviewed into its list.
- **Shell:** `loom-daemon forge may-write [--repo OWNER/REPO]` prints the
  repository to name on the write (exit 0) or the reason (exit 1).
  `loom_write_repo` in `lib/forge-helpers.sh` wraps it, and `post-verdict.sh`,
  `verdict-staleness-guard.sh --clear/--anchor`, `merge-pr.sh`, the
  `forge-helpers.sh` comment, label, reopen, create and merge wrappers, and
  the lease publish/renew scripts all use it, passing `--repo` or
  `repos/OWNER/REPO` explicitly.
- **Without the verb** (no `loom-daemon`, or an older one), the permission
  check cannot run. The fallback allows a write only from a checkout whose
  one remote is `origin`, only to `origin`, and only with `GH_REPO` unset or
  equal, so it can never reach another project. A fork checkout therefore
  cannot write at all until the daemon is rolled.
