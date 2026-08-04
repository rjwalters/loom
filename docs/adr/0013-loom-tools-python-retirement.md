# ADR-0013: Retire the Python `loom-tools` Package — One Rust Binary Plus Bash

## Status

Accepted

Implemented by epic **#4081** ("Eliminate Python from Loom"), Phases 1-4. This
ADR records the decision at the point Phase 4 (#4557) deleted the package.

## Context

### What Loom looked like before

Loom's orchestration logic lived in two places at once:

- `loom-tools/` — a Python package (`loom_tools`, ~31.8k lines measured on `main`
  @ `c4b6f677`) installed with `pip install -e` / `pipx install --editable`,
  exposing ~27 console scripts (`loom-tokens`, `loom-clean`, `loom-status`,
  `loom-agent-spawn`, `loom-forge`, …) plus `python3 -m loom_tools.<module>`
  entry points that `defaults/scripts/*.sh` shelled into.
- `loom-daemon/` — a Rust binary that owned tmux session management, the sweep
  registry, the MCP surface, and the event bus (ADR-0010).

Every shell entry point had to bridge between them: locate the package
(`defaults/scripts/lib/loom-tools.sh`'s `find_loom_tools()`), set
`PYTHONPATH=<repo>/loom-tools/src`, forward `LOOM_PACKAGE_PATH` across a spawn
boundary (#3949), and print pip-install instructions when resolution failed.

### The incident that forced the decision (#4079)

On 2026-07-27 a live daemon host was found with a **stale editable pip install**
of `loom-tools`. Editable installs track module *code* live but freeze their
**console scripts** at install time, so that host's `/opt/homebrew/bin` held 27
entry points from an install months old:

- CLIs deleted releases earlier (`loom-shepherd`, `loom-daemon-loop`) still
  resolvable and still runnable;
- a **Python `loom-daemon` shim** importing the `daemon_v2` brain that ADR-0009
  deleted in v0.10.0 — which **shadowed the Rust `loom-daemon` binary on PATH**;
- and, conversely, *no* `loom-tokens` entry point at all (it was added to
  `pyproject.toml` after that install), so token-ranking refresh had been
  silently broken for every managed workspace.

The failure mode is **structural, not a one-off**: nothing regenerates a frozen
entry point, and the drift is invisible — `loom-daemon --version` reported a
fresh commit while callers resolving `loom-daemon` by name got the stale Python
shim.

It also collided head-on with the machine-level daemon architecture
(#3835/#3926): in a one-daemon-per-machine world, consumer repos carry only
config. They cannot be asked to carry, own, or refresh a pip install.

The operator direction (Tier 3, 2026-07-27, recorded on #4081) was explicit:
**retire `loom-tools` entirely.** End state — one signed, commit-stamped,
self-updating Rust artifact plus bash scripts; **zero Python at runtime, in CI,
or in the install story.**

## Decision

**Delete the Python package. Move its load-bearing functionality into
`loom-daemon` subcommands, keep every shell entry point's name and flags, and
delete the rest outright rather than porting it.**

Three rules shaped the work:

1. **Callers switch, interfaces persist.** `spawn-claude.sh`,
   `probe-tokens.sh`, `merge-pr.sh`, `worktree.sh`, `resolve-model.sh`,
   `checkpoint.sh`, `validate-phase.sh`, `check-usage.sh`, `agent-spawn.sh`,
   `agent-wait.sh` and friends kept their names, flags, output shapes and exit
   codes. Only their *implementation* changed, from `python3 -m loom_tools.X`
   to `loom-daemon <subcommand>` (via `lib/script-helper.sh`, the native
   replacement for `lib/loom-tools.sh`'s `run_loom_tool`).
2. **Port only what has callers.** Modules with zero external references were
   deleted, not translated. Roughly 31.8k lines of Python became a much smaller
   volume of Rust because most of it was dead, duplicated, or a thin
   argparse-over-bash wrapper.
3. **No new Python, ever.** No phase was permitted to add a Python dependency.
   The token rate-limit probe shells to `curl` rather than pulling in an HTTP
   client crate, matching house style.

### The phased migration

| Phase | Scope | Outcome |
|---|---|---|
| **1** (#4082, #4105, #4106, #4108) | Port the `tokens` package (~4.7k lines: 3-tier selection, rate-limit probe, bootstrap, `import-from-monitor`, pin/unpin/unblock) to `loom-daemon tokens`. Pure addition — no callers changed. | Native token pool shipped alongside the Python one, with a conformance suite diffing both implementations' state files byte-for-byte. |
| **2** (#4228) | Cut the token hot path over: `spawn-claude.sh` (select), `probe-tokens.sh` (check), `claude-wrapper.sh` (mark-bad / re-select), the daemon's own ranking-refresh loop and usage collection. Removed `LOOM_PACKAGE_PATH` forwarding. | A consumer-repo workspace with **zero pip installs** completed select → spawn → probe → ranking → status end-to-end. |
| **3** (#4271-#4275, residuals #4415, #4435) | Five file-disjoint families run in parallel: (1) delete the dead modules; (2) worktree/clean (`clean`, `cleanup`, `worktree`, `orphan_recovery`); (3) forge/merge (`auto_merge`, `forge_cli`); (4) agent/session + metrics (`agent_spawn`, `agent_wait`, `agent_metrics`, `status`, `daemon_diagnostic`, `stuck_detection`, `health_monitor`); (5) script helpers (`log_filter`, `model_tiers`, `usage`, `validate_phase`, `checkpoints`, `claim`, `sweep_experiment`). | Every remaining load-bearing module either native or gone. `loom-clean` / `loom-recover-orphans` / `loom-claim` survive as **names only** — auto-generated bash PATH shims exec'ing `loom-daemon <sub>`, regenerated by `provision-daemon.sh` on every provision, so they can never freeze the way a pip console script does. |
| **4** (#4557, this ADR) | Delete `loom-tools/` (minus the carve-out below), remove the Python CI toolchain, purge `PYTHONPATH`/`LOOM_PACKAGE_PATH`/pip plumbing from scripts and docs, fix the build gate, add the stale-entry-point warning, write this ADR. | Zero Python on any load-bearing path. |

Also folded in: epic #4047's Python-side `config_resolver` migrations (#4060,
#4061) were closed as **superseded** — migrating readers in modules this epic
deletes is moot. Their curated semantics carried forward as *porting contracts*
binding Phases 1 and 3 (`--config <path>` as an explicit tier bypass;
`load_config` never raising; forge config resolving from the canonical repo root
via `git rev-parse --git-common-dir`, never worktree CWD). The Rust-reader
cohort (#4058) proceeds independently in #4047.

### The state-format-compatibility contract

The migration was only safe because of one contract, held unbroken from Phase 1
through Phase 4: **while a Python and a Rust implementation could both run on the
same host, all on-disk state stayed byte-compatible.** Concretely, for the token
pool:

- `.ranking` — pipe-delimited lines, same field order and formatting;
- `.bad_tokens` — one reason/timestamp line per entry, with reason-aware `auth`
  persistence and identical TTL semantics; the reason is newline-sanitized so an
  entry is always exactly one line;
- `.failure_counts` — same JSON shape, with auto-unpin at threshold ≥ 5;
- `.allowlist` — same format and precedence in the 3-tier selection algorithm;
- **`mkdir`-based locking**, deliberately not `flock`, because `flock(1)` is
  unavailable on stock macOS. The Rust port kept `mkdir`.

This was not merely asserted — it was *tested*. `loom-tools/tests/tokens/`
(including `test_rust_conformance.py`) ran both implementations against fixture
pools and diffed the resulting files, and was contractually kept alive until this
final phase. That is what made every cutover reversible: a host could roll
between the Python and Rust implementations at any point without a state
migration, so no phase had a flag day.

Phase 4 **closes** that contract: with one implementation left there is nothing
to be compatible *with*, and the conformance suite was deleted along with the
package. The formats themselves are unchanged and remain the durable on-disk
contract; they are now pinned by `loom-daemon`'s own Rust tests.

One conformance fixture deliberately outlives the package:
`loom-tools/tests/fixtures/config_resolver/` (#4039), which pins that the Rust
(`loom-daemon/src/config_resolver.rs`), Bash
(`defaults/scripts/lib/config-resolver.sh`) and Python config resolvers all merge
the same tier tree to the same `expected.json`. It stays because two of those
three consumers are alive and Python-independent.

### The `loom-search` carve-out — retired (#4970)

`loom-tools/pyproject.toml` had exactly two active console scripts:
`loom-tokens` and `loom-search`. `loom-tokens` was superseded by
`loom-daemon tokens` in Phase 1/2 and is simply gone.

**`loom-search` (`loom_tools/semantic_search.py`) was carved out of the Phase 4
deletion** rather than deleted alongside the rest. It backed the opt-in,
off-by-default semantic-search feature documented in
`defaults/docs/semantic-search.md` (#4339, Tier B embeddings #4370): a SQLite
FTS5 + BM25 index over sweep summaries and merged-PR history, with an optional
local ONNX embeddings layer.

Why it was carved out rather than deleted with the rest, at the time:

- **It had no native port.** No epic #4081 phase covered it. It appeared in
  neither the "zero external references (delete, don't port)" inventory, nor
  any Phase 3 family, nor Phase 1's tokens-only scope. No phase decided
  whether to port it, retire the feature, or keep it.
- **Deleting it silently would have been a silent feature removal.** Because
  the feature was opt-in and off by default, **no test would have gone red.**
  CI would have stayed green while a documented, shipped capability
  disappeared.
- **Porting it was explicitly out of scope for Phase 4.** Phase 4 was
  deletion, not another port.

That left an explicit, tracked open question: port `loom-search` to Rust, or
retire the feature. **The operator decided RETIRE**, recorded on
[#4608](https://github.com/rjwalters/loom/issues/4608) (2026-07-31) —
`loom-search` had zero demonstrated usage (never installed, no index, on any
host including the primary operator host), and the underlying need
(searchable fleet memory) is now better served by the telemetry query API
(#4704/#4705/#4726) than a local Python index. That decision was implemented
in [#4970](https://github.com/rjwalters/loom/issues/4970), which deleted
`loom-tools/` in full — including the `loom-search` carve-out described
above — leaving zero Python anywhere in the repo. The one exception is the
`tests/fixtures/config_resolver/` cross-language conformance fixture (#4039),
which was relocated (not deleted) to
`defaults/scripts/tests/fixtures/config_resolver/` because its two surviving
consumers (Rust, Bash) are still alive; see "The state-format-compatibility
contract" above.

`defaults/docs/semantic-search.md` is retained as a tombstone pointing to git
history and the telemetry query API as the successor direction, rather than
being deleted outright, because it is linked from this ADR.

### Stale-entry-point hardening

Because #4079's damage came from entry points that *outlived* their package,
Phase 4 added a defense at the point an operator is already thinking about
binary currency: **`loom-daemon-update.sh` now warns about stale `loom-*` PATH
entries** on every invocation, including `--check`, `--dry-run` and an
up-to-date no-op. It reports any `loom-*` executable on `PATH` that does not
resolve to the `loom-daemon` binary the script just resolved, and separately
warns when `PATH` holds more than one `loom-daemon` (the first shadows the rest).
Only the current auto-generated shims are never flagged; `loom-search` was
allowlisted here too from Phase 4 through #4969, but #4970 dropped that entry
(a Python package it retired needs no allowlist carve-out) — a leftover
`loom-search` binary is now flagged like any other stale entry point.

It is **advisory only** — it deletes nothing, does not touch `PATH`, and never
changes the exit code. An operator's `~/.local/bin` is theirs, and a false
positive must not block an update. `LOOM_SKIP_STALE_ENTRY_POINT_CHECK=1` opts
out.

Complementing it, the `guard-loom-workflow.sh` `pip install -e` worktree block
(#2495) is **retained and strengthened**, not removed with the package: it still
protects the Python repos Loom orchestrates, and it stops new frozen console
scripts being created in the first place.

## Consequences

### Positive

- **One artifact, one version.** `loom-daemon --version` (commit-stamped via
  `build.rs`) is now the whole truth about what is installed. The #4079 class of
  "binary says fresh, callers run stale" is structurally gone for the core path.
- **Provisioning is a single binary copy.** No pip, no pipx, no venv, no
  `PYTHONPATH`, no `LOOM_PACKAGE_PATH` forwarding across spawn boundaries, no
  `find_loom_tools()` resolution ladder. `loom update` rebuilds, provisions,
  verifies the destination's embedded commit, and restarts.
- **Consumer repos carry only config**, as #3835/#3926 intended — no install
  they neither own nor refresh.
- **The build gate no longer needs a Python toolchain.** Its fast tier is
  `cargo build` + a `loom-daemon --version` startup smoke; its full tier is
  `cargo test` + the bash installer suite.
- **Less duplicated logic.** Several behaviors existed as byte-identical twins
  in Python and Rust (model-alias normalization in `model_tiers.py` vs.
  `sweep_registry.rs::read_model_aliases`; `claude_config` in Python vs.
  `terminal.rs`). Collapsing each pair to one implementation removed a whole
  class of drift bug.
- **~31.8k lines of Python deleted**, most of it dead or duplicated rather than
  translated.

### Negative

- **The escape hatch is gone.** Editing a Python module took effect immediately;
  changing daemon behavior now requires a Rust rebuild + reprovision. That cost
  is why `loom-daemon-update.sh` (#3968) exists and why it verifies the
  destination binary's embedded commit (#4053).
- **Rust is a higher bar to contribute to** than Python for logic that used to
  live in a script.
- ~~One Python residue remains (`loom-search`), so "zero Python in the repo"
  is not literally true yet~~ **Resolved by #4970** (2026-08): the operator's
  RETIRE decision on #4608 closed this gap — `loom-tools/` is deleted in
  full and the repo has zero Python anywhere, not merely on the load-bearing
  path.
- **The cross-implementation conformance suite is gone.** It was the safety net
  for the migration; it has no remaining purpose (there is one implementation),
  but the on-disk formats it pinned are now pinned only by `loom-daemon`'s own
  tests.
- **Downstream repos with an old Loom install may still hold frozen `loom-*`
  console scripts.** Nothing can delete those for them. The update script warns,
  the retired `run_loom_tool` shim fails loudly with a migration message instead
  of an opaque `No module named loom_tools`, and the guard hook prevents new
  ones.

## Alternatives Considered

**Keep `loom-tools` and fix the entry points properly** (regenerate console
scripts on every provision; pin a non-editable install). Rejected: it treats a
symptom. Editable installs *structurally* freeze entry points, and any scheme
that keeps a pip install alive still requires every consumer repo — which by
design carries only config — to own and refresh a Python environment. The
drift would return the first time someone used `pip install -e` again (and #2495
shows they do).

**Ship `loom-tools` as a proper published wheel with pinned versions.**
Rejected: it swaps entry-point drift for version-skew drift between the wheel and
the Rust binary, adds a release surface to keep in lockstep, and still requires
Python on every host. It also does nothing about the duplicated-logic problem.

**Rewrite everything in bash, no Rust growth.** Rejected: the token pool alone
(3-tier selection, rate-limit-header parsing, atomic `.ranking` writes, TTL
bookkeeping, locking) is not something to maintain in bash, and the daemon
already owned adjacent state.

**Big-bang rewrite instead of four phases.** Rejected: with a live daemon fleet
running on the pool being replaced, a flag day has no rollback. The phased plan
plus the byte-compatible state contract meant any single cutover could be rolled
back without a state migration — which is exactly why the contract was worth its
cost.

**Delete `loom-search` along with the rest** (the literal reading of Phase 4's
"delete `loom-tools/`"). Rejected: it is a documented, shipped feature with no
native replacement and no test that would have caught its removal, and no phase
ever made the decision to drop it. Silently deleting a live feature is worse than
carrying a small, clearly-quarantined, opt-in residue with an open tracking issue.

**Port `loom-search` to Rust as part of Phase 4.** Rejected: out of scope by the
epic's own terms ("Phase 4 is deletion only, not another port"), and it would
have coupled the retirement to an unscoped feature port. Tracked separately.

## References

- Related GitHub Issues:
  - **#4081** — epic: Eliminate Python from Loom (operator-approved, 2026-07-27)
  - **#4079** — the stale-editable-install incident that motivated the epic
  - Phase 1: #4082, #4105, #4106, #4108 · Phase 2: #4228 (bridge #4080)
  - Phase 3: #4271, #4272, #4273, #4274, #4275 (residuals #4415, #4435)
  - **#4557** — Phase 4: this retirement (carved out the `loom-search` residue)
  - **#4608** — the operator's decision issue that resolved the carve-out's
    open port-or-retire question: RETIRE (2026-07-31)
  - **#4970** — implemented the RETIRE decision: deleted `loom-tools/` in
    full, relocated the #4039 conformance fixture, tombstoned
    `semantic-search.md`
  - #3949 (`LOOM_PACKAGE_PATH`, removed) · #3938 (shared token pool) ·
    #3968 / #4053 (daemon self-update + destination verification) ·
    #3835 / #3926 (machine-level daemon architecture) ·
    #4047 / #4051 / #4058 / #4059 / #4060 / #4061 (config resolution) ·
    #4039 (cross-language config-resolver conformance fixture, relocated by
    #4970 to `defaults/scripts/tests/fixtures/config_resolver/`) ·
    #2495 (`pip install -e` worktree guard) ·
    #4339 / #4370 (`loom-search` and its Tier B embeddings, retired by #4970) ·
    #4259 (tiered build gate) ·
    #4704 / #4705 / #4726 (telemetry query API — the successor direction for
    searchable fleet memory)
- Related ADRs:
  - [ADR-0009](0009-shepherd-deprecation.md) — deleted the Python shepherd and
    `daemon_v2` brains (the *first* Python removal; this ADR finishes the job)
  - [ADR-0010](0010-daemon-rebuild.md) — rebuilt daemon mode as the Rust binary
    this epic consolidated onto
  - [ADR-0012](0012-runtime-adapter-contract.md) — the runtime adapter contract,
    which an adapter now implements with no Python bridge to satisfy
- Documentation:
  - [`docs/migration/v0.10.0-shepherd-deprecation.md`](../migration/v0.10.0-shepherd-deprecation.md)
  - [`defaults/docs/semantic-search.md`](../../defaults/docs/semantic-search.md)
    — tombstone for the retired `loom-search` carve-out (#4970)
  - [`defaults/docs/build-gate.md`](../../defaults/docs/build-gate.md) — the
    now-Python-free quality gate
  - [`defaults/docs/guard-hooks.md`](../../defaults/docs/guard-hooks.md) — the
    retained `pip install -e` worktree guard
