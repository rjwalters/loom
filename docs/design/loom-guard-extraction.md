# loom-guard: Extracting the Generic Guard Into a Shared Component (Issue #8570)

This is the design for #8570. It covers how the generic destructive-command
guard becomes a standalone `loom-guard` component that either `/repo` (Repo
Skills) or `/loom` can install, with one active generic engine per invocation.
It answers each bullet of the 2026-10-17 operator ruling in its own section and
ends with an implementation sequence and the gates that stay closed.

**Status: preparation, proposal for review.** This doc and the comparison tool
in `tests/hooks/compare-guard-engines.sh` are the whole change. It creates no
repository, publishes nothing, switches no live guard and edits no vendored
guard pattern. Section 9 lists what is not authorized.

**Evidence convention.** A number below is either (a) the output of a command
shown next to it, against the pinned commits in section 2, or (b) marked
**not measured**. Nothing is carried over from the historical "~311 failed
assertions" report; that figure was untraced and is only re-measured here for
the suites named in section 4.

## Contents

1. [Problem and what already works](#1-problem-and-what-already-works)
2. [Pinned inputs](#2-pinned-inputs)
3. [Component boundary and interface](#3-component-boundary-and-interface)
4. [Behavior inventory and divergence classification](#4-behavior-inventory-and-divergence-classification)
5. [Version, release and provenance contract](#5-version-release-and-provenance-contract)
6. [Install matrix](#6-install-matrix)
7. [tmpfs/ramfs build-directory protection](#7-tmpfsramfs-build-directory-protection)
8. [Implementation sequence and gates](#8-implementation-sequence-and-gates)
9. [Not authorized by the ruling](#9-not-authorized-by-the-ruling)
10. [Remaining uncertainties](#10-remaining-uncertainties)

## 1. Problem and what already works

Two copies of the generic guard exist and have drifted:

- Repo Skills' `hooks/repo/guard-destructive.sh` (canonical, 12,273 lines at
  `v0.24.2`/`b7e03ab`) carries the tmpfs/ramfs predicate (#461, #544).
- Loom's `defaults/hooks/guard-destructive-generic.sh` (12,455 lines) is a
  hand-vendored copy with a DO-NOT-HAND-EDIT banner. It has no tmpfs predicate
  and carries Loom-only categories (write confinement, `--body @path` family,
  search/jq masking) plus later fixes the canonical file lacks.

`defaults/hooks/guard-destructive.sh` is the runtime dispatcher. It `exec`s the
installed Repo Skills guard only if five grep probes pass (`repo#29`,
`worktree-write-confinement`, `--comment|--search`, `--arg|--argjson`,
`gh-comment-body-literal-at`), else the vendored copy, else exits 0. The
invariant to keep is "exactly one generic guard runs, never zero".

The dispatcher works but the arrangement has no shared contract: the probes are
grep heuristics duplicated in the dispatcher and in `resync-installed.sh`
(~line 2176), with the marker strings also referenced by
`scripts/check-guard-scan-contracts.sh` and `test-resync-installed.sh`. Parity
is by re-vendoring at release time, which no script performs.

## 2. Pinned inputs

| Input | Pin |
|---|---|
| Loom (vendored engine, dispatcher, installers) | `98b58ac7c95b5787ac2f6a19c2f97166ec055e0a` (main at the start of this work) |
| Repo Skills (canonical engine) | `b7e03ab7283bbc76bbaebb99b932f38c9a3066c5`, tag `v0.24.3` is the latest tag; `hooks/repo/guard-destructive.sh` is 12,273 lines |
| Comparison tool | `tests/hooks/compare-guard-engines.sh` (this change) |

Repo Skills was read from a local checkout at `~/GitHub/repo` (clean tree,
HEAD above). Installed copies under other repos' `.claude/skills/repo/` are
older (one inspected copy is 5,379 lines) and were **not** compared; they are
what a real host runs today, which is why section 6 treats "installed Repo
Skills version" as an input to the dispatcher decision, not an assumption.

## 3. Component boundary and interface

**Proposal (not created):** a `loom-guard` component owning exactly the
generic engine: the single file `guard-destructive` (bash), its shared helper
sourced today from `scripts/lib/config-resolver.sh`, its regression suites, and
a manifest.

| Concern | Owned by `loom-guard` | Stays in the product |
|---|---|---|
| Generic destructive-command patterns, tiers, masking, segment parsing | yes | |
| Generic toggles: `guards.sqlDdl`, `cloudCli`, `reversibleGh`, `rmScope`, `forceScope`, `cargoCleanScope`, `tmpfsScratch`, decision log | yes | |
| tmpfs/ramfs build-dir predicate and its fixture mount-table hook | yes | |
| Loom workflow guard (`gh pr merge` redirect, daemon workspace mutation, remote-fleet-exec, installed-file writes, cargo target dir) | | `/loom`: `guard-loom-workflow.sh` |
| Edit/Write confinement | | `/loom`: `guard-worktree-paths.sh` |
| Hook wiring, `hook-wiring.sh`, settings.json merge | | each installer |

**Open boundary question (decision needed in review):** the Bash-tool
write-confinement category (`worktree-write-confinement`) and the `--body
@path` family live inside the generic file today but are Loom-motivated. The
conservative proposal is to keep them in the shared engine behind their
existing toggles, because moving them out would split one shell-segment parser
across two files. If the owners prefer a Loom-only overlay hook instead, the
dispatcher probes in section 4 become the contract for what the overlay must
re-provide.

**Public interface** (all already true of the file today, so this is a freeze,
not a redesign):

- stdin: PreToolUse JSON (`tool_input.command`, `cwd`); stdout: the
  `hookSpecificOutput` decision JSON or nothing (allow); exit is always 0.
- Config precedence: env override, then `.loom/config.json`
  (`guards.<name>`), then shipped default. The shared engine must also read the
  Repo Skills spelling (`REPO_GUARD_*`, REPO wins over LOOM, as the tmpfs
  toggle does in canonical).
- Test seam: `REPO_GUARD_MOUNTS_FILE` / `LOOM_GUARD_MOUNTS_FILE` for a fixture
  mount table; `LOOM_GUARD_OVERRIDE` in the test harness for differential runs.
- Machine-readable identity: a `loom-guard-manifest` line (version, source
  commit, capability list). This replaces the grep probes (section 5).

## 4. Behavior inventory and divergence classification

Measured 2026-10-17 with the commands shown. Both engines run the same corpus
in a hermetic repo with a fixture mount table.

```
$ bash tests/hooks/compare-guard-engines.sh --quiet \
      --canonical ~/GitHub/repo/hooks/repo/guard-destructive.sh
vendored : defaults/hooks/guard-destructive-generic.sh sha256=a0bb9cf009b0 repo-head=98b58ac7c95b
canonical: ~/GitHub/repo/hooks/repo/guard-destructive.sh sha256=1d823c92ef4f repo-head=b7e03ab7283b
vendored-gap  deny  allow  deny   tmpfs | CARGO_TARGET_DIR=/dev/shm/t cargo build
vendored-gap  deny  allow  deny   tmpfs | cargo build --target-dir /dev/shm/x
vendored-gap  deny  allow  deny   tmpfs | cargo build --target-dir=/dev/shm/x
vendored-gap  deny  allow  deny   tmpfs | export CARGO_TARGET_DIR=/run/user/1000/t && cargo build
vendored-gap  deny  allow  deny   tmpfs | CARGO_TARGET_DIR=/mnt/ramdisk/t cargo test
vendored-gap  deny  allow  deny   tmpfs | cd /tmp && CARGO_TARGET_DIR=/run/user/1000/a/../t cargo check
# rows=16 flagged=6
```

On a host without the canonical file the tool prints
`canonical: NOT MEASURED`, marks rows `incomplete` and exits 3, so a missing
comparison cannot read as a pass. (Verified: exit 3 with no `--canonical`.)

The existing suites, run against the canonical file through the
`LOOM_GUARD_OVERRIDE` seam (canonical copied temporarily under
`defaults/hooks/` so `config-resolver.sh` resolves; removed afterwards):

| Suite | Vendored failed | Canonical (`b7e03ab`) failed |
|---|---|---|
| dispatcher | 0 (20 pass) | 0 (20 pass) |
| core | 0 | 101 |
| masking | 0 | 48 |
| rm-scope | 0 | 11 |
| stash-scope | 0 | 10 |

Only these five suites were run. The other suites listed in the historical
report (force-scope, write-confinement, matching-precision, cp-mv-continuation,
sed-operands, mkdir-confinement, decision-log, readonly-fastpath,
heredoc-subst, ...) are **not measured** in this change. The core/masking/
rm-scope numbers are close to the historical report (101/47/11), the
stash-scope one equals it (10); that is a coincidence of two measurements, not
a validation of the old total.

Classification of the failures that were read (first assertions of each suite;
the full per-assertion classification is follow-on work, see section 8):

| Divergence | Class | Evidence |
|---|---|---|
| tmpfs/ramfs build dirs allowed by vendored | **protection gap** (vendored lacks it) | six `vendored-gap` rows above |
| `Ask (not deny)` for `gcloud ... delete`, `aws iam delete-*` (#4216) | **tier/reason-text difference**: canonical denies where vendored asks | core suite, first three failures |
| `gh pr comment --body @-`, `gh issue edit --body @-`, `gh api -f body=@-`, `--body "$VAR"` with `VAR=@-` (#9258) | **protection gap** (canonical lacks the vendored rule) | core suite |
| `git read-tree` non-adjacent/prefixed forms (#7923) | **protection gap** (canonical lacks it) | core suite |
| `rm` inside unquoted heredoc `$( )`/backtick spans (#8217, #6519) | **protection gap** (canonical lacks it) | rm-scope suite |
| `stash-push main`/`stash-pop main`/`safe-stash-pop.sh` hint text (#6076, #6501) | **product-specific** (Loom helper names in reason text) | stash-scope suite |
| quoted `--body` citing `git stash pop` stays allowed (#5783); inert prose in `check-duplicate.sh` args (#5235) | **false-positive fix** present in vendored, absent in canonical | stash-scope suite |

Reading: the divergence is bidirectional. Canonical has the tmpfs predicate;
the vendored copy has a set of later fixes canonical lacks. This is why neither
a wholesale re-vendor (regresses the second set) nor a six-function port
(addresses only the first and forks the trust boundary) is the plan. The shared
component has to be the **union**, proven by running both suites against it.

Preserved denial floors, opt-outs and failure behavior to carry over unchanged
(read from `defaults/docs/guard-hooks.md`, "The Ungated Denial Floor", and the
dispatcher header): the ungated floor no toggle disables; `guards.*` toggles
with `LOOM_*` env overrides; fail-open on internal error (`trap 'exit 0' ERR`);
bash 3.2 portability. Supported runtimes: bash on Linux and macOS;
`/proc/mounts` exists only on Linux (section 7).

## 5. Version, release and provenance contract

One contract replaces the grep probes:

1. The engine file carries a single machine-readable header line, e.g.
   `# loom-guard-manifest: version=1.4.0 source=<sha> caps=tmpfs,write-confinement,body-literal-at,search-mask`.
2. A consumer (dispatcher, `resync-installed.sh`, `check-guards-installed.sh`,
   `/repo` installer) calls one shared predicate, `loom-guard-compatible
   <engine> <required-caps> <min-version>`, instead of its own grep chain. Today
   the dispatcher and `resync-installed.sh` (~2176) must be edited in lockstep,
   and the marker strings are also pinned by `check-guard-scan-contracts.sh`;
   the predicate removes that.
3. Each product **pins** a version range and records the pin where it already
   records install metadata (`.loom/install-metadata.json` for Loom). A
   product installs the pinned artifact; it never takes "latest".
4. Release provenance: tag, source commit and checksum of the artifact, stated
   in the manifest and verified at install. **Proposed home:** a new repository
   owned and published under the owners' own decision (section 9); until it
   exists the single source remains Repo Skills for the engine and Loom for its
   overlay, exactly as today.
5. Compatibility is stated by capability, not by version arithmetic, matching
   the dispatcher's existing "presence, no semver" rule. A missing required
   capability routes to the known-working fallback.

## 6. Install matrix

Every cell is a specification derived from reading `guard-destructive.sh`,
`resync-installed.sh` (~2140-2260), `provision-hooks.sh` and
`check-guards-installed.sh`. **None was exercised by running an installer in
this change** (not measured); the follow-on fixture work in section 8 is what
turns them into receipts.

Invariant asserted in every cell: after the operation, the dispatcher resolves
exactly one engine, and `check-guards-installed.sh` reports no BROKEN hook.

| Scenario | Install | Update | Uninstall | Rollback |
|---|---|---|---|---|
| `/loom` only (no Repo Skills) | wire dispatcher; engine = bundled `loom-guard` fallback | resync replaces fallback only if manifest is newer within the pin | `loom-daemon` deprovision removes wiring keyed on `/defaults/hooks/<name>` (existing #4200 ownership marker) | keep previous engine file until the new one passes the capability predicate |
| `/repo` only (no Loom) | Repo Skills installs the same artifact at its own path | Repo Skills update, same pin rule | Repo Skills removes its entry | previous artifact retained until verified |
| Both, `/repo` installed first, then `/loom` | Loom detects a compatible engine via the predicate and does **not** install a second copy | each product updates only the artifact it installed; the predicate arbitrates | removing one product leaves the other's engine in place | dispatcher falls back to the bundled copy if the other product's artifact is removed or fails the predicate |
| Both, `/loom` first, then `/repo` | Repo Skills' coexistence-aware installer defers to Loom's wired entry (already its behavior per the dispatcher header) | same | same | same |

Path variants:

- **Full install** (machine checkout at `${LOOM_HOME:-~/.local/share/loom}`):
  dispatcher runs from the checkout, `LOOM_PROJECT_ROOT` locates the repo's
  `.claude/skills/repo/`.
- **Quick install / project-level fallback (#4401):** `.loom/hooks/` copies; same
  predicate, resolved from `SCRIPT_DIR/../..`.
- **Transition (per-repo copy still present, #4200 dedup):** the project copy
  wins until stripped, so each guard still runs once.

Constraints carried over, not changed:

- A **git-tracked** `.loom/hooks/guard-destructive-generic.sh` is never
  deleted because one host has Repo Skills (#4403, #4566). Loom itself is that
  case.
- User opt-outs and `guards.*` configuration live in `.loom/config.json` and
  env, not in the engine file, so replacing the engine preserves them.
- If no compatible artifact is available: the bundled fallback runs; if that is
  also unreadable the dispatcher exits 0 (existing fail-open), and
  `check-guards-installed.sh` reports it, so zero-engine is visible rather than
  silent.

## 7. tmpfs/ramfs build-directory protection

Required shared behavior, taken from canonical (`b7e03ab`) rather than
re-specified:

- Effective path: same-command `CARGO_TARGET_DIR=`, `--target-dir`
  (space and `=` forms), `TMPDIR=`, `export`/`env`/`sudo` prefixes, ambient
  `CARGO_TARGET_DIR`, and the resolved `build.target-dir`; `..` normalized
  before classification.
- Classification: longest-prefix match against the mount table, by filesystem
  type (`tmpfs`/`ramfs`), never a hardcoded `/dev/shm` prefix. Ties go to the
  later entry (overmount). Octal-escaped mountpoints decoded.
- Unreadable/absent mount table (macOS) or no covering entry: no opinion, never
  a deny.
- Tier: deny, with a message naming a sanctioned on-disk location. Stay silent
  when the on-disk default would itself be on tmpfs (no sanctioned alternative
  to name).
- Toggle `guards.tmpfsScratch` (`REPO_GUARD_TMPFS_SCRATCH`, `LOOM_GUARD_TMPFS_SCRATCH`).
- The deny text must point at `defaults/docs/troubleshooting.md`, "Sanctioned
  on-disk location for a private/disposable target dir" (the Curator verified
  that `build-gate.md` has no such section).

Regression evidence: the `tmpfs` rows in
`tests/hooks/fixtures/guard-engine-compat-corpus.tsv` (six deny rows, three
allow rows) against a fixture mount table. They are expected to pass on the
shared engine and currently show six `vendored-gap` rows. Unreadable-table and
`/tmp`-as-tmpfs cases are **not** in the corpus yet; adding them is part of
step 3 below, since they need the engine to accept a second mount table.

The earlier #8512 reclaim (`loom_daemon::tmpfs_reclaim`) is the other half of
this protection and is independent of the guard.

## 8. Implementation sequence and gates

| Step | Work | Gate before the next step |
|---|---|---|
| 1 (this change) | Design doc, comparison tool, corpus, pointer in `guard-hooks.md` | Review + CI; normal merge policy |
| 2 | Extend corpus from the suites: full per-assertion divergence table for all 18 `test-guard-destructive-*` suites against a pinned canonical, classified per section 4 | Table complete; any suite not run is listed as not measured |
| 3 | Add unreadable-mount-table and tmpfs-`/tmp` rows; make the corpus runnable in CI against a pinned Repo Skills tarball if one is provided (otherwise CI keeps exit-3 semantics and does not run it) | Owner decision on how CI obtains the pinned artifact |
| 4 | Draft the union engine in a scratch location: canonical's tmpfs predicate plus the vendored-only fixes, produced upstream-first, not by editing the vendored file here | Union engine passes all vendored suites **and** all canonical suites with zero net regressions |
| 5 | Propose manifest line and `loom-guard-compatible` predicate; replace the grep-probe sites behind a feature flag defaulting to the current probes | Dispatcher suite (20 cases) green with both mechanisms |
| 6 | Installer matrix fixtures: exercise the six rows of section 6 in temp repos with stub `.claude/skills/repo/` trees | Each cell has a receipt; "exactly one engine" asserted |
| 7 | Release/publish, fleet install, switching a live guard | Separate authorization (section 9) |

Technical prerequisite at step 3/7, recorded rather than worked around: there
is no `loom-guard` repository, package or release, and this change does not
create one. Steps 4 to 7 need an owner-designated home.

## 9. Not authorized by the ruling

This preparation does not authorize, and this change does not do:

- creating a repository, choosing owners or visibility for `loom-guard`;
- publishing a package or release;
- installing or switching any live guard, or editing host/runtime config;
- fleet rollout or deployment, spend, or access to a new scope;
- merging the PR;
- a temporary hand-port of the six tmpfs helpers into the vendored guard, or
  any hand edit of `guard-destructive-generic.sh`;
- weakening any guard gate or the test baseline.

## 10. Remaining uncertainties

- Not measured: 13 of the 18 guard suites against canonical; every installer
  cell in section 6; behavior of older installed Repo Skills copies (one is
  5,379 lines) through the dispatcher probes.
- Whether the write-confinement and `--body @path` families belong in the
  shared engine or a Loom overlay (section 3).
- How CI would obtain a pinned canonical engine (section 8, step 3).
- The union engine's maintenance cost is unmeasured; the failing-assertion
  counts show the size of the reconciliation, not its effort.
- "Preparation complete" here means a reviewable design and a measuring tool.
  It is **not** "extracted component published" and **not** "fleet protection
  deployed": the vendored fallback on standalone-Loom hosts still lacks the
  tmpfs predicate until a later, separately authorized step lands.
