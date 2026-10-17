# loom-guard: Extracting the Generic Guard Into a Shared Component (Issue #8570)

This is the design for #8570. It covers how the generic destructive-command
guard becomes a standalone `loom-guard` component that either `/repo` (Repo
Skills) or `/loom` can install, with one active generic engine per invocation.
It answers each bullet of the 2026-10-17 operator ruling in its own section and
ends with an implementation sequence and the gates that stay closed.

**Status: preparation, proposal for review.** This doc, the comparison tool
`tests/hooks/compare-guard-engines.sh` and the install-matrix fixture
`tests/hooks/test-guard-install-matrix.sh` are the whole change. It creates no
repository, publishes nothing, switches no live guard and edits no vendored
guard pattern. Section 9 lists what is not authorized.

**Evidence convention.** A number below is either (a) the output of a command
shown next to it, against the pinned commits in section 2, or (b) marked
**not measured**. Nothing is carried over from the historical "~311 failed
assertions" report; that figure was untraced. Section 4 re-measures all 18 guard suites at the
pinned commits instead; the result is not comparable to the old figure.

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
| Loom (step 2/3/6 measurements, #8570 residual) | `3170d473084b` (main when the per-suite table, corpus rows and install-matrix receipts were produced) |
| Comparison tool | `tests/hooks/compare-guard-engines.sh` |
| Install-matrix fixture | `tests/hooks/test-guard-install-matrix.sh` |

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
vendored : defaults/hooks/guard-destructive-generic.sh sha256=a0bb9cf009b0 repo-head=3170d473084b
canonical: ~/GitHub/repo/hooks/repo/guard-destructive.sh sha256=1d823c92ef4f repo-head=b7e03ab7283b
vendored-gap  deny  allow  deny   tmpfs | CARGO_TARGET_DIR=/dev/shm/t cargo build
vendored-gap  deny  allow  deny   tmpfs | cargo build --target-dir /dev/shm/x
vendored-gap  deny  allow  deny   tmpfs | cargo build --target-dir=/dev/shm/x
vendored-gap  deny  allow  deny   tmpfs | export CARGO_TARGET_DIR=/run/user/1000/t && cargo build
vendored-gap  deny  allow  deny   tmpfs | CARGO_TARGET_DIR=/mnt/ramdisk/t cargo test
vendored-gap  deny  allow  deny   tmpfs | cd /tmp && CARGO_TARGET_DIR=/run/user/1000/a/../t cargo check
vendored-gap  deny  allow  deny   tmpfs-tmp | CARGO_TARGET_DIR=/tmp/t cargo build
vendored-gap  deny  allow  deny   tmpfs-tmp | cargo build --target-dir /tmp/x
# rows=22 flagged=8
```

The corpus has 22 rows. The 14 rows not listed are `ok`: both engines gave the
required verdict. They include the two new `tmpfs-unreadable` rows (a mount
table path that does not exist: both engines stay **silent** and allow, the
required "unmeasurable must not deny" behavior) and two `tmpfs-tmp` rows that
guard against a path-prefix implementation: in a fixture where `/tmp` is tmpfs
and `/dev/shm` is not a mount, `CARGO_TARGET_DIR=/dev/shm/t` is allowed and
`CARGO_TARGET_DIR=/tmp/t` and `--target-dir /tmp/x` are denied, so canonical
classifies by mount type (section 7). The `tmpfs-tmp` rows run with the acting
cwd under `/var/tmp` (disk in every fixture table). An earlier attempt that
left the cwd under the host's `$TMPDIR` saw both engines **allow** `/tmp/t`:
canonical deliberately stays silent when the acting cwd is on the same RAM
mount as the target (the "already in RAM" exemption documented at its
`tmpfs_scratch_*` block), so the corpus must control the cwd or it measures the
exemption instead of the predicate.

On a host without the canonical file the tool prints
`canonical: NOT MEASURED`, marks rows `incomplete` and exits 3, so a missing
comparison cannot read as a pass. (Verified: exit 3 with no `--canonical`.)

Per-suite results, all 18 `tests/hooks/test-guard-destructive-*.sh` suites, run
on 2026-10-17 against Repo Skills `b7e03ab7283bbc76bbaebb99b932f38c9a3066c5`
(`v0.24.3`, `hooks/repo/guard-destructive.sh`, 12,273 lines) through the harness
seam `LOOM_GUARD_OVERRIDE` (`tests/hooks/lib/guard-destructive-harness.sh`).
Method: `git archive HEAD` of Loom `3170d473084b` into a scratch directory, the
canonical file copied in next to the vendored guard so `config-resolver.sh`
resolves, then for each suite one run with and one run without the override,
sequentially, with `LOOM_FORCE_SCOPE` and `LOOM_GUARD_DECISION_LOG` unset (the
sweep environment exports both; see the builder role's environment note). The
vendored column is the baseline at the same commit.

| Suite | Vendored (pass/fail) | Canonical (pass/fail) | First divergences read | Class |
|---|---|---|---|---|
| cargo-and-perf | 27 / 0 | 26 / 1 | `Cargo clean deny names the category opt-out (#7795)` | reason-text / config difference |
| category-opt-ins | 110 / 0 | 102 / 8 | SQL exec-gate cases (`sed -i` source edit containing `DROP TABLE`, `git grep`, pipeline into `sqlite3`) | candidate false-positive fixes carried only by the vendored copy (not individually verified) |
| cp-mv-continuation | 48 / 0 | 33 / 15 | multi-source continuation-line `cp`/`mv` into an in-worktree destination expected to allow | candidate false-positive fix in vendored only (not individually verified) |
| decision-log | 28 / 0 | 23 / 5 | `#4216` retiered patterns log `ask`; `#7882` log location for the `defaults/hooks/` layout; `#10434` ask text names `LOOM_GUARD_CLOUD` | tier/reason-text difference; product-specific (Loom config names, layout) |
| force-scope | 146 / 0 | 111 / 35 | `cd '<worktree>' && reset --hard <own branch>` allowed under `forceScope protected` (#5372); `$VAR` resolution (#6152) | product-specific policy (Loom `LOOM_FORCE_SCOPE`) plus candidate protection logic present only in vendored |
| heredoc-subst | 119 / 0 | aborted rc=127 after 91 pass / 26 fail | see note below the table | not comparable: white-box suite |
| matching-precision | 229 / 0 | 200 / 29 | `#4216` retier (`az group delete`, `gcloud ... delete` ask vs deny); `#5838` `check-duplicate.sh` quoting a catastrophic phrase | tier difference; candidate false-positive fix (vendored only) |
| mkdir-confinement | 19 / 0 | 11 / 8 | `mkdir -p ../../../pwned-dir` into the main checkout expected to deny | candidate protection gap in canonical (Loom-only category, not individually verified) |
| mktemp-canon | 43 / 0 | 43 / 0 | none | identical |
| readonly-fastpath | 68 / 0 | 55 / 13 | `find ... -delete`/`-exec`/`-fls` must not be fast-pathed (#3772); `grep <ddl> \| less` stdin sink (#5263) | candidate protection gap in canonical (the first three are fast-path escapes; not individually verified) |
| sed-operands | 33 / 0 | 22 / 11 | `sed -i` with multiple `-e`, `--expression`/`--file`, attached `-es/a/b/` forms (#11074) | candidate protection gap in canonical (not individually verified) |
| session-scratch | 43 / 0 | 43 / 0 | none | identical |
| write-confinement | 437 / 0 | 400 / 37 | `tee`/`sudo tee`/`cp` with a trailing heredoc operator misread as an extra write target (#5232) | candidate false-positive fix in vendored only (not individually verified) |
| core | 358 / 0 | 257 / 101 | see the classification table below | mixed |
| masking | 136 / 0 | 88 / 48 | see the classification table below | mixed |
| rm-scope | 210 / 0 | 199 / 11 | see the classification table below | protection gap in canonical (heredoc `$( )` spans) |
| stash-scope | 138 / 0 | 128 / 10 | see the classification table below | product-specific plus false-positive fix |
| dispatcher | 20 passed / 0 failed | not applicable | the suite tests the dispatcher's own probes, so it has no engine override | n/a |

Of the 18 suites, 14 completed against canonical with failures, 2 were
identical (`mktemp-canon`, `session-scratch`), 1 (`heredoc-subst`) did not run to
completion, and 1 (`dispatcher`) has no engine override. The canonical failures sum to
`1+8+15+5+35+29+8+13+11+37+101+48+11+10 = 332` assertions in the 14 suites that
completed with failures, plus the 26 failures `heredoc-subst` reached before it
aborted. This is a measurement at one pinned commit pair, **not** a validation
of the historical "~311" figure, and no failure was traced to a source line.
"Class" cells that say "candidate ... not individually verified" are a reading
of the first failing assertions only; the per-assertion classification of the
remaining failures is the input to design step 4, not something this change
claims.

`heredoc-subst` note: the suite `eval`s the vendored functions
`heredoc_filesink_candidate_targets` and `heredoc_filesink_targets_are_inert`
out of the guard file (line ~340 of the suite). Canonical does not define them,
so the suite exits 127 at that point. That is a structural difference (the
suite tests internals, not behavior). The vendored run executes 119 assertions
and the canonical run reached 117 before aborting, so the last assertions of
that suite are **not measured** against canonical.

Classification of the failures that were read in the first measurement
(`core`, `masking`, `rm-scope`, `stash-scope`; first assertions of each suite):

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

The cells below are specifications derived from reading `guard-destructive.sh`,
`resync-installed.sh` (~2140-2260), `provision-hooks.sh` and
`check-guards-installed.sh`. The *current* dispatcher, the *current*
user-scope and project-level wiring functions, and `check-guards-installed.sh`
were then exercised by `tests/hooks/test-guard-install-matrix.sh` (receipts
below). What the fixture does **not** run is any *future* behavior (the manifest
line and `loom-guard-compatible` predicate of section 5 do not exist yet) and any
installer owned by the other product.

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

### Receipts (`tests/hooks/test-guard-install-matrix.sh`, Loom `3170d473084b`)

Command: `bash tests/hooks/test-guard-install-matrix.sh`; result `42 passed, 0 failed`
on 2026-10-17. Sandbox: temporary `$HOME` and `$LOOM_HOME` (`$LOOM_HOME/defaults`
symlinks the worktree's `defaults/`), so no real `~/.claude` or `~/.local` is
touched. **Real:** `provision_loom_hooks`, `deprovision_loom_hooks`,
`ensure_project_hook_wiring` from `scripts/install/provision-hooks.sh`, the
user-scope wrapper command executed verbatim from the written `settings.json`,
the real dispatcher and vendored engine, the real `check-guards-installed.sh`.
**Simulated:** the Repo Skills installer (a stub tree carrying all five probe
markers, logging each invocation and denying with a sentinel) and
`loom-daemon init`'s per-repo copy (`cp defaults/hooks/*.sh .loom/hooks/`).
The stub is not Repo Skills; "canonical" in this table means "a guard that
passes all five dispatcher probes".

| Cell | Install | Update | Uninstall | Rollback | One engine after each op | `check-guards-installed` |
|---|---|---|---|---|---|---|
| 1. `/loom` only, full (user-scope) | measured: 13 hooks wired, vendored decides | measured: re-provision idempotent (13 entries) | measured: all Loom entries removed, user-owned settings key preserved, no engine wired afterwards | measured: compatible stub appears then is removed, dispatcher returns to vendored | yes (every step) | 0 |
| 2. `/loom` only, quick (per-repo copy) | measured: project wiring written, vendored decides | measured: re-copy keeps one engine | measured: copies removed, project settings still wire them (see findings) | not applicable (no second engine) | yes | 0 |
| 3. `/repo` only | measured (stub): stub decides alone; no Loom wiring exists | not measured (no real Repo Skills installer) | measured (stub): removal leaves no engine, owned by `/repo` | not measured | yes | 0 (non-Loom repo is a no-op) |
| 4. both, `/repo` then `/loom` | measured: dispatcher picks the compatible guard, one engine | measured: a build lacking probe (c) or probe (d) falls back to vendored, a compatible build returns to canonical | measured: removing `/repo` returns to vendored, Loom dispatcher byte-identical | same as uninstall | yes | 0 |
| 5. both, `/loom` then `/repo` | measured: vendored, then canonical once the stub appears; Loom's `settings.json` unchanged by that step | measured: re-copy with `/repo` present keeps canonical | measured: removing `/repo` returns to vendored | same as uninstall | yes | 0 |
| 6. transition (copy + user-scope wiring) | measured: user-scope wrapper emits no decision while the per-repo copy exists, project entry decides alone | n/a | measured: stripping the copy hands over to the user-scope wrapper, one engine | measured: with and without the stub | yes (0 or 1 decision each time, never 2) | 0 |

Findings that change the earlier text:

1. **A missing or non-executable vendored engine is not reported.** With
   `.loom/hooks/guard-destructive-generic.sh` deleted, the dispatcher fails open
   (no decision, exit 0) and `check-guards-installed.sh` still exits 0; with the
   file merely non-executable it also exits 0. The script only inspects hooks
   named in `settings.json`, and the engine is a sibling file the dispatcher
   `exec`s, not a wired hook. The earlier statement that zero-engine "is visible
   rather than silent" was **wrong at this commit**. The shared component's
   install contract therefore needs the engine (or its manifest) to be an
   explicit check target; that is a design requirement (section 8, step 5), not
   something this change implements.
2. **A missing wired project copy is `DEGRADED`, not `BROKEN`, when a machine
   checkout exists.** With `.loom/hooks/guard-destructive.sh` removed but
   `settings.json` still wiring it, the check exits 0 while `$LOOM_HOME` holds the
   hook, and exits 2 only when it does not (both measured). That matches the
   script's documented intent; the matrix invariant is "no BROKEN", which is
   weaker than "the intended engine is installed".
3. `PROJECT_HOOKS_WIRED` is a count of asserted entries, not a boolean.

Not measured: the real `/repo` installer in any order; `resync-installed.sh`
(it resolves against the primary checkout and is unsafe to run from a
worktree); `loom-daemon init`'s real copy and `loom-daemon` deprovision; older
installed Repo Skills copies (e.g. the 5,379-line one) through the probes;
macOS; and the quick-versus-full install paths of the *proposed* manifest
mechanism.

Constraints carried over, not changed:

- A **git-tracked** `.loom/hooks/guard-destructive-generic.sh` is never
  deleted because one host has Repo Skills (#4403, #4566). Loom itself is that
  case.
- User opt-outs and `guards.*` configuration live in `.loom/config.json` and
  env, not in the engine file, so replacing the engine preserves them.
- If no compatible artifact is available: the bundled fallback runs; if that is
  also unreadable the dispatcher exits 0 (existing fail-open). Measured: at
  this commit `check-guards-installed.sh` does **not** report that state (finding
  1 above), so zero-engine is currently silent, and the shared component must
  add the check.

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
| 1 (done, #12602) | Design doc, comparison tool, corpus, pointer in `guard-hooks.md` | Review + CI; normal merge policy |
| 2 (suite-level done) | Per-suite divergence table for all 18 `test-guard-destructive-*` suites against canonical `b7e03ab` (section 4). **Per-assertion classification is still open** and is the input to step 4 | Table complete; any suite not run is listed as not measured (`heredoc-subst` tail) |
| 3 (rows done) | Unreadable-mount-table and tmpfs-`/tmp` rows added (section 4); make the corpus runnable in CI against a pinned Repo Skills tarball if one is provided (otherwise CI keeps exit-3 semantics and does not run it) | Owner decision on how CI obtains the pinned artifact |
| 4 | Draft the union engine in a scratch location: canonical's tmpfs predicate plus the vendored-only fixes, produced upstream-first, not by editing the vendored file here | Union engine passes all vendored suites **and** all canonical suites with zero net regressions |
| 5 | Propose manifest line and `loom-guard-compatible` predicate; replace the grep-probe sites behind a feature flag defaulting to the current probes | Dispatcher suite (20 cases) green with both mechanisms |
| 6 (done for current code) | `tests/hooks/test-guard-install-matrix.sh` exercises the six cells of section 6 in temp repos with stub `.claude/skills/repo/` trees; wired into CI because it needs no Repo Skills checkout | Each cell has a receipt; "exactly one engine" asserted; real `/repo` installer cells remain not measured |
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

- Not measured: the assertions of `heredoc-subst` after its abort against
  canonical (the suite `eval`s vendored-only functions); every cell that needs
  the real `/repo` installer, `resync-installed.sh`, `loom-daemon init` or
  macOS (section 6); behavior of older installed Repo Skills copies (one is
  5,379 lines) through the dispatcher probes.
- Measured but not traced: 332 canonical failures across 14 suites plus 26 in
  `heredoc-subst` (section 4). Only the first assertions of each suite were
  read, so the classes in that table marked "candidate" are hypotheses. No
  claim is made that the historical "~311" figure is current or wrong.
- `check-guards-installed.sh` does not check the engine file (section 6,
  finding 1). Whether the fix belongs in that script or in the shared
  component's manifest check is undecided.
- Whether the write-confinement and `--body @path` families belong in the
  shared engine or a Loom overlay (section 3). `mkdir-confinement`,
  `write-confinement` and `force-scope` show the largest Loom-specific
  surface, which argues for the overlay but is not measured as a boundary.
- How CI would obtain a pinned canonical engine (section 8, step 3). Until an
  owner decides, `compare-guard-engines.sh` keeps exit-3 (incomplete) semantics
  in CI and the 18-suite measurement is a manual, receipted run.
- The union engine's maintenance cost is unmeasured; the failing-assertion
  counts show the size of the reconciliation, not its effort.
- "Preparation complete" here means a reviewable design and measuring tools.
  It is **not** "extracted component published" and **not** "fleet protection
  deployed": the vendored fallback on standalone-Loom hosts still lacks the
  tmpfs predicate until a later, separately authorized step lands. Steps 4, 5
  and 7 of section 8 need an owner-designated home and authorization.
