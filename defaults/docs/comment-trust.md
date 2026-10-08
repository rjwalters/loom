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
| A login on the **fleet admin roster**, `fleet/admins.json` in the fleet store | `{"admins": ["turian", "rjwalters"]}`, read through the fleet store reader (`fleet.repo`, `fleet.ref`), never from the repo being judged. User accounts only (App-spelled entries are ignored). Applies in every fleet repo (#10303). |

One row is **record-scoped**, not author-scoped:

| Record | Why |
|---|---|
| A verified signed `loom:operator-decision` marker line, whoever posted it | A fleet admin's decision relayed by a tool (e.g. a dashboard App) that must stay an untrusted author. Only the parsed record counts: the rest of that comment, its author, the author's other comments and markers, and the issue body all stay untrusted. See [Signed operator decisions](#signed-operator-decisions-10827) (#10827). |

**Another Loom installation's markers are not ours.** A foreign fleet emits
perfectly well-formed Loom markers; its Apps are not in this roster, so they
count for nothing here.

### Trap: an admin with private org membership reads as `CONTRIBUTOR`

`author_association` reflects *public* organization membership and explicit
collaborator records, not the live permission level. A repo admin (or org
owner) whose org membership is **private**, or an outside collaborator, is
reported as `CONTRIBUTOR`, so every verdict marker they post is dropped. The
verdict-staleness pass then falls back to an older trusted marker and clears a
fresh `loom:pr` (#9709). Fleet admins belong on the fleet admin roster
(below), which covers every fleet repo at once; list other such reviewers
explicitly in `forge.trustedCommenters` (see Configuration below). Trust is
otherwise not widened automatically; instead, when a newer marker was dropped this way, the
stale-clear notice (from either the daemon pass or
`verdict-staleness-guard.sh --clear`, both rendered by
`loom-daemon forge verdict-stale-notice`'s template) and the daemon log line
name the login, its `author_association`, and `forge.trustedCommenters`,
rather than claiming the head SHA moved.

## Where it is enforced

The predicate lives once, in `loom-daemon/src/comment_trust.rs`:

- **Rust readers** filter at fetch. `claim_reconciliation`'s comment fetch
  returns trusted bodies only, so the verdict backstop, the stale-verdict
  dedup, and the base-conflict "is this flag ours?" check never see an
  untrusted marker. `star_liveness::trust` delegates to the same predicate.
- **Shell readers** call `loom-daemon forge trusted-comments`, which reads a
  comment listing on stdin and prints the trusted subset in the same shape.
  `verdict-staleness-guard.sh` and `check-promotion-landed.sh` use it.
  `loom-daemon forge verdict-stale-notice --label L --marker-sha M --head-sha H`
  reads the *raw* listing on stdin only to name a dropped newer marker's
  author in the stale-clear notice (attribution, never evidence); without the
  verb, the guard posts a one-line notice carrying the same dedup marker.
  Empty or whitespace-only stdin exits 1 like any other non-listing: an
  empty listing is `[]`, so nothing at all means the fetch never happened.
- **The other Rust marker readers** (#9548, High slice) filter the same way,
  through `comment_trust::records`:

  | Reader | Marker | When the author is untrusted |
  |---|---|---|
  | Lease probes (claim reconciliation, orphan recovery, dispatch tie-break, mid-build watchdog) | `loom:lease` | Not a lease: it can neither hold a claim, win the tie-break, nor fence a cleanup. |
  | Claim reconciliation | `loom:claim-activity` / `loom:standdown` | Not claimant activity: it cannot keep a dead claim alive. |
  | Review-conflict pass | `loom:base-conflict flagged` | Not "ours", so a Judge's verdict is never undone. |
  | Quarantine reconciliation | `Auto-quarantined by loom-daemon (#3939)` | Not the daemon's quarantine; the marker must also *start* the comment. |
  | Dependency classification | `champion:proposal-escalated` / `dep-cycle` / `proposal-unescalated` | Absent. |
  | `premise-check` | `loom:premise-check … verdict=` | Absent (comments; a body counts only when its author is trusted). |
  | Required-check re-date | `loom:stale-check-redate` / its hold marker | Not attempt state. |
  | Mechanical-capability lane | body `loom:capability=` | No declaration: the park stays. |
  | Role-shard roster | `loom:roster` | Not a ring member. |
  | Open-linked-PR guard | a fork PR's `Closes #N` | Not a linked PR (a same-repo branch always counts). |

  Readers that used `gh … --json comments` now read the REST listing, whose
  author spelling can name an App. A REST comment listing that comes back
  empty or unparseable is a failed read, never "no comments".

  Body markers (`premise-check`, `loom:capability=`) are trusted by the
  **issue author**, because the forge does not say who last edited a body. A
  trusted insider's marker edited into an outsider-filed body is therefore
  ignored. That fails closed: post the record as a comment instead.
- **Structural tests** fail when a new Rust file handles a covered marker
  without being reviewed (`verdict_sha_readers_go_through_the_trust_filter`,
  `structure_tests::every_covered_marker_file_is_reviewed`), and when a
  reviewed file gains a comment fetch outside its filtered call sites
  (`structure_tests::every_comment_fetch_in_a_reviewed_file_is_a_filtered_call_site`).

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

## Fleet admin roster (#10303)

Precedence: the roster and `forge.trustedCommenters` are a **union**; the
per-repo list extends the roster and neither can remove the other. The
association, Apps and self rules are unchanged.

- Source: `fleet/admins.json` in the store named by `fleet.repo` /
  `LOOM_FLEET_REPO` (proposed contract; fleet-gitops publishes it).
- Cache: resolved once per process per store and kept for
  `forge.fleetAdminsTtlSecs` (default 300). A stale on-disk snapshot is served
  only while younger than 24h, else the roster is unavailable.
- **Fails closed**: store unset, fetch error, missing file, malformed JSON or
  a non-array `admins` yield an empty roster (trust equals the pre-roster
  behaviour). Each cause is logged once at warn, and
  `TrustPolicy::sources_consulted()` reports `fleet admin roster ...
  (unavailable: <reason>)` for the ignored-marker notice.

## Signed operator decisions (#10827)

**Threat model.** A fleet may record an operator's decision through a narrow
GitHub App that also files bot issues, so the App must stay an untrusted
author: listing it in `forge.trustedCommenters` would make every marker it
writes, including forged decision text from automated paths, count as
control. Instead, one line of its comment counts when it carries an Ed25519
signature by a key the fleet published. Anyone can copy a signed line; they
cannot alter one or move it to another repo or issue, and it expires.

**What verification grants.** Exactly one record: fleet admin `by` decided
`decision` on issue `issue` of `repo` at `at`. Not the comment's other text,
not its author, not the author's other comments or markers, not the issue
body. `TrustPolicy::trusts` is unchanged (`loom-daemon/src/comment_trust/
decision.rs` is a separate API, and its only consumer is the promotion gate
below). A decision comment's human-readable line is content; only the
verified marker is state.

### Wire format

One whole line of the comment (a trailing `\r` is allowed), fields in this
order, single spaces, nothing after `-->`:

```text
<!-- loom:operator-decision v1 repo=<owner/name> issue=<n> decision=<approve|defer|reject> by=<login> at=<YYYY-MM-DDTHH:MM:SSZ> key=<key-id> sig=ed25519:<base64> -->
```

- `repo`, `by`: lower case (the one normalisation rule; GitHub names are
  case-insensitive). `by` is a user login, never `x[bot]`.
- `issue`: a positive decimal integer, no leading zero.
- `decision`: `approve`, `defer` or `reject`. Any other word is prose even
  with a valid signature.
- `at`: UTC, whole seconds, `Z` suffix; no fraction, no offset.
- `key`: `[a-z0-9][a-z0-9._-]{0,63}`, an id from `fleet/decision-signers.json`.
- `sig`: padded standard base64 of the 64-byte signature.

Anything else is rejected, never normalised: a duplicate, unknown or
reordered field, extra whitespace, trailing data, upper case, a non-canonical
timestamp, number or base64, an unknown version. A comment with two marker
lines counts for nothing.

**Signed bytes**: UTF-8 (ASCII), seven `\n`-terminated lines, terminal
newline included:

```text
loom:operator-decision v1
repo=<repo>
issue=<issue>
decision=<decision>
by=<by>
at=<at>
key=<key>
```

### Verification (fails closed)

A line is a decision only when all hold; otherwise it is prose:

1. it parses strictly (above);
2. `repo`/`issue` are where the comment sits (the comment's own `issue_url`,
   which must also match the issue it was listed under);
3. `key` names an **active** key in `fleet/decision-signers.json`, read
   through the fleet store reader (`fleet.repo`/`fleet.ref`), never from the
   judged repository; lookup is by id only, never by trying every key;
4. the signature verifies under that key;
5. `by` is on the fleet admin roster `fleet/admins.json`, checked separately
   (holding a key adds no admin);
6. `at` is no older than `forge.operatorDecisionMaxAgeSecs` (default 7 days,
   capped at 30) and no more than 5 minutes in the future.

Missing or malformed signer data, a cached store snapshot older than 24h, an
unknown/revoked key, a bad signature, the wrong place, a non-admin `by`, or an
out-of-window `at` all leave the line prose.

**Replay.** Among the issue's verified, in-window records the greatest `at`
wins; on equal `at` the more conservative decision (`reject` > `defer` >
`approve`), then the greater `(key, sig)`. Invalid records take no part, so a
newer forged marker cannot shadow an older valid one, and when every record
has aged out the issue has no signed decision again.

Diagnostics name reason categories (`bad-signature:1,expired:1`) and key ids,
never a marker payload or signature.

### Signer keys: `fleet/decision-signers.json`

```json
{"version": 1, "keys": [
  {"id": "dash-2026-10", "alg": "ed25519", "public_key": "<base64 of 32 bytes>", "state": "active"},
  {"id": "dash-2026-04", "alg": "ed25519", "public_key": "<base64 of 32 bytes>", "state": "revoked"}
]}
```

Strict: unknown fields, a version other than 1, more than 16 keys, a duplicate
id or public key, a non-`ed25519` alg, a state other than `active`/`revoked`,
or non-canonical base64 rejects the **whole file**. Cached like the admin
roster (`forge.decisionSignersTtlSecs`, default 300; a stale snapshot only
while younger than 24h).

**Rotation**: add the new key as `active` beside the old one (two active keys
verify side by side, each only its own markers), move the signer over, then
mark the old key `revoked` or delete it. A revoked or removed key verifies
nothing, including markers it already signed.

**Private keys never live in this repository or the fleet store.** Loom
stores and reads public keys only and never signs. The signing tool keeps its
private key under the credential policy
([credential-storage](credential-storage.md)). With OpenSSL:

```bash
printf 'loom:operator-decision v1\nrepo=%s\nissue=%s\ndecision=%s\nby=%s\nat=%s\nkey=%s\n' \
  "$REPO" "$N" "$DECISION" "$BY" "$AT" "$KEY" > msg.bin
openssl pkeyutl -sign -inkey "$PRIVATE_KEY_PEM" -rawin -in msg.bin | base64   # sig=ed25519:<this>
```

### Published test vector

Test-only key: seed `05c3574eb3a78c83d2719f2755c0f1cac6fdec94fe8694b70a26dface35cbc08`
(`sha256("loom-10827-test-vector-key-a")`), public key
`4hMOQPTWOkE461Va0/ykByEMayRT+DM/4or9N+OA86Q=`. Signed with OpenSSL 4.0.3 and
pinned in `comment_trust::decision::tests`:

```text
<!-- loom:operator-decision v1 repo=acme/widgets issue=42 decision=approve by=octo-admin at=2026-10-08T12:00:00Z key=test-a sig=ed25519:rRAbMpmWIg/OA7RxZswtB15XMruYCHRmGxR4MbdiuJTrh1+AkHRUvpuvi/cR4TPAxCnFJiFG94rgLYB69sZ1CA== -->
```

## Promotion author gate (#10827)

The rules above cover markers, not who wrote the issue being promoted. Every
automatic `loom:curated` → `loom:issue` write first asks
`loom-daemon forge promotion-gate --issue N [--repo OWNER/REPO]`:

| Answer | When |
|---|---|
| `GATE=ELIGIBLE` | The body author is trusted (the table above); or the issue carries a direct operator star (`loom:operator-priority` / `loom:operator-high-priority`) whose newest `labeled` event's actor is trusted (the table above) or a user with repository role `triage` or better (an App's role is never asked, so the issues-write App cannot star its own issue; the daemon's `*-inherited` stars do not count); or its newest verified signed decision is `approve` |
| `GATE=HOLD` | An untrusted body author with none of those, or a newest signed decision of `defer`/`reject` |
| `GATE=UNAVAILABLE` | The issue or its comments could not be read, or a star's applier could not be resolved and nothing else decided |

Callers branch on the `GATE=` line, never the exit code; anything but
`ELIGIBLE` (including no output from an older binary) means do not promote.
Passing is not approval: every other promotion criterion still applies, and a
signed `approve` is evidence of who decided, interpreted by the normal
workflow. A held issue is left exactly as it was. `NOTICE=needed` plus
`NOTICE_BODY=` offers one explanatory comment (`<!-- loom:promotion-author-gate -->`),
needed only until a trusted author has posted it, so retries never repeat it.
`REASON=` is safe to post; `DETAIL=` (ignored-marker counts, signer and roster
state) may name the fleet store and stays local.

Callers: Champion Step 3b and Pass 0c (`check-promotion-landed.sh --apply`
exits 14, `DECISION=GATED`, writing and posting nothing), and Curator's
starred promotion (Priority 0 adds `loom:issue` only on `GATE=ELIGIBLE`, so a
star the untrusted App applied itself promotes nothing). `/loom:sweep`'s
approval gate executes an approval
already given (an operator dispatch, a star, or the red-fix lane's own
trusted-author check). Restoring `loom:issue` after a claim or park
(`loom:building`/`loom:blocked` → `loom:issue`) re-grants an approval the
issue already had and is not a promotion.

## Configuration

```json
{ "forge": { "trustedCommenters": ["release-bot", "helper-app[bot]"] } }
```

Anything other than an array of logins is ignored with a warning: a malformed
value never widens trust.

## Shell and role-prompt readers

`loom-daemon forge trusted-comments --fetch N [--repo owner/name]` fetches
issue/PR N's REST listing itself and prints the trusted subset (exit 1 when it
cannot). `--with-body` puts the issue/PR first, so its body survives only when
its author is trusted; `--gh-shape` prints `gh --json comments` field names
(`author.login`, `authorAssociation`, `body`, `createdAt`) for splicing into a
`gh … --json` document. Readers and their direction when the filter is
unavailable:

| Reader | Markers / phrases | Filter unavailable |
|---|---|---|
| `claim-staleness.sh` | `loom:claim-activity`, `loom:standdown` | `unknown` (never stomp) |
| `sweep-lease-fence.sh` | `loom:lease`, `loom:lease-yield` | fails open, as on an unreadable listing |
| `sweep-lease-publish.sh` | same | publishes anyway, as on an unreadable listing |
| `sweep-lease-renew.sh` | same | exit 1, nothing patched |
| `classify-ac-verification.sh` | `loom:ac-verified` (comments; the PR body only when its author is trusted) | no evidence: the issue stays held |
| Champion merge precheck | hold markers, release phrases, new Judge reviews | skip the PR this pass |
| Champion criterion #5 | "real activity" comments | the raw read (it can only read as more active) |
| Critical-file hold | `champion:critical-file-*`, `hold-state` | the raw read (bookkeeping only; a FAIL never merges) |
| Champion epic | epic verdict / escalation markers | skip the epic this pass |
| Judge fast-track | `loom:conflict-only` | full evaluation |
| Curator AC-hold check | `champion:ac-hold` | treated as no hold |

Role prompts that read forge text carry either the full untrusted-content
block or a one-line pointer to this page; see
[`untrusted-external-content.md`](untrusted-external-content.md).

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
   (`LOOM_WRITE_SCOPE_TTL_SECS`); when a re-probe cannot answer, a WRITE
   verified in the last 24 hours still counts, and a definitive "no" never
   does. On Gitea the same rule probes `GET /api/v1/repos/{owner}/{repo}`'s
   `permissions` object for the authenticated user: `push` or `admin` is
   WRITE. The credential is the one the writes carry — `GITEA_TOKEN`, then
   `forge.gitea.token` in `.loom/config.json`, then `FORGE_TOKEN` (the
   documented generic fallback); `GITEA_URL` and `GITEA_USERNAME` beat their
   config keys the same way — and an instance that cannot be reached or
   authenticated is an unverifiable probe: refused, never guessed.

Anything unverifiable is a refusal. Reads are never gated.

- **Daemon:** `loom-daemon/src/write_scope.rs`. Claim reconciliation,
  quarantine reconciliation, star liveness, sweep dispatch and every
  scheduled role tick skip a refused workspace, logging the reason once. The
  `forge issue|pr` write passthroughs and `forge auto-merge` /
  `disable-auto-merge` vet their target first. The roster heartbeat checks its
  configured repository; `notify-cleared-blockers` and the stale-check redate
  take the repository `merge-pr.sh` already vetted; dependency classification
  resolves `origin` (never gh's preference) or takes its caller's explicit
  `--repo`. The structural test
  `write_scope::tests::daemon_write_paths_are_scoped` fails when a new daemon
  file writes to the forge without being reviewed into its list. One
  autonomous write sits outside the rule above, scoped narrower by that test:
  the declared captain's ETA fit publication (#10395) goes to the configured
  fleet store (`fleet.repo`, never gh's resolution) under its writer App, and
  only to the dedicated `fleet.etaFitRef` branch, never the reviewed one.
- **Shell:** `loom-daemon forge may-write [--repo OWNER/REPO]` prints the
  repository to name on the write (exit 0) or the reason (exit 1).
  `loom_write_repo` in `lib/forge-helpers.sh` wraps it. Every script that
  writes uses it and then passes `--repo` or `repos/OWNER/REPO` explicitly:
  `post-verdict.sh`, `verdict-staleness-guard.sh --clear/--anchor`,
  `merge-pr.sh`, `create-pr.sh`, `check-promotion-landed.sh --apply`,
  `check-main-clean.sh`, `claim-staleness.sh`, `classify-capacity-defer.sh`,
  `clean-stale-building-labels.sh`, `rebase-stacked-children.sh`,
  `reconcile-stack.sh`, `sync-labels.sh`, the lease publish/renew scripts,
  and the `forge-helpers.sh` comment, label, reopen, create and merge
  wrappers. `write_scope::tests::shell_write_paths_are_vetted` fails when a
  script under `defaults/scripts` writes without it.
- **Cross-repo writes** (`create-issue.sh --repo X`, `sync-labels.sh --repo
  X`) now need X to be the origin of a registered workspace. To manage X
  from here, register its checkout: `loom-daemon workspace add <path>`.
- **Without the verb** (no `loom-daemon`, or an older one), the permission
  check cannot run. The fallback allows a write only from a checkout whose
  one remote is `origin`, only to `origin`, and only with `GH_REPO` unset or
  equal, so it can never reach another project. A fork checkout therefore
  cannot write at all until the daemon is rolled.
