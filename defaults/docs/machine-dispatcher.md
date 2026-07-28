# Machine-level `loom` dispatcher

Epic #3835 Phase 3a (#4157) + Phase 3b (#4229). The `loom` dispatcher is a
machine-level entry point installed to `~/.local/bin/loom`. It resolves the
machine-level Loom checkout at `~/.local/share/loom` and exec's into it. It is a
sibling of the `~/.local/bin/loom-daemon` binary — one install per machine,
shared across every repo Loom is installed into.

```
loom <command> [options]
```

| Command | What it does |
|---------|--------------|
| `start` | Start the machine-level `loom-daemon` (delegates to `loom-daemon-start.sh`) |
| `stop`  | Stop the machine-level `loom-daemon` (delegates to `loom-daemon-stop.sh`) |
| `restart` | Restart the machine-level `loom-daemon` (drain-and-roll; falls back to stop+start) |
| `status`| Show machine-level + current-repo status (read-only) |
| `sweep <issue>` | Dispatch `/loom:sweep <issue>` for the current repo |
| `update`| Thin delegate to `loom-daemon-update.sh` (no rebuild logic of its own) |

Environment: `LOOM_HOME` overrides the machine-level checkout location (default
`~/.local/share/loom`).

## Checkout resolution — link, not a second clone (AC1)

The installer always runs **from** a Loom source checkout (`$LOOM_ROOT`). Rather
than cloning a *second* copy into `~/.local/share/loom` — which a developer
running Loom *on* the Loom repo could then let drift out of sync — provisioning
establishes the machine checkout as a **symlink**:

```
~/.local/share/loom -> $LOOM_ROOT
```

A symlink cannot diverge, so the hard AC1 constraint ("a developer running Loom
on the Loom repo must not end up with two divergent copies") holds by
construction. If `~/.local/share/loom` already exists as a **real directory**
(an operator's pre-existing standalone clone), provisioning leaves it untouched
— that is the supported "fresh clone" resolution. The `loom` dispatcher resolves
the checkout at runtime and works with either shape.

## The name collision with `./.loom/bin/loom`, and how it is resolved (AC3)

There are two different Loom surfaces that both answer to the name `loom`:

| Surface | What it is | Verbs |
|---------|-----------|-------|
| `~/.local/bin/loom` (this dispatcher) | Machine-level runtime driver | `start stop restart status sweep update` |
| `./.loom/bin/loom` | Per-repo **tmux agent-pool** manager | `start status health stop attach send scale logs` |

Three verbs collide by name — `start`, `stop`, `status` — and mean *different
things* in each surface.

**Why there is no PATH-shadowing.** `.loom/bin` is **never** added to `PATH`
anywhere in the tree, and every in-repo invocation of the pool manager is
path-qualified (`./.loom/bin/loom …`). A path-qualified call never resolves
through `PATH`, and a bare `loom …` on `PATH` never resolves to `./.loom/bin/loom`.
So the two invocation forms are **disjoint by construction** — adding
`~/.local/bin/loom` cannot shadow the pool manager, and the pool manager cannot
shadow the dispatcher. This is the `#4079` failure mode (a stale entry shadowing
another on `PATH`) *not* recurring; it is asserted by a regression test, not
patched with a compatibility shim.

**The residual, human-facing risk** is narrower: an operator typing a bare
`loom start` *while inside a consumer repo* might mean the tmux pool but get the
machine dispatcher. The dispatcher resolves this by **detecting** a nearby
`./.loom/bin/loom` (walking up from `$PWD`) and, for the three colliding verbs:

- **`start` / `stop`** (they mutate process state): the dispatcher **refuses**
  and prints a disambiguation naming *both* surfaces, then exits non-zero — it
  never silently runs the wrong one. Force the machine surface with
  `loom start --machine`, or run the pool with `./.loom/bin/loom start`.
  `restart` (#4229) gets the **same guard**, even though the per-repo pool
  manager has no `restart` verb of its own — guard consistency across the
  three process-mutating verbs is cheaper than explaining why `restart` is the
  odd one out.
- **`status`** (read-only, and required by AC7 to produce output from inside a
  repo): the dispatcher prints machine-level status and a clearly-labelled line
  pointing at the per-repo pool manager (`… run: ./.loom/bin/loom status`). It
  is never silent about the other surface.

## `status` output across contexts (AC7)

`loom status` reports a `repo:` line that distinguishes the three contexts:

- **consumer repo root** → `repo: consumer-repo (root: …)`
- **git worktree** (under `.loom/worktrees/…`) → `repo: git-worktree (main checkout: …)`
- **non-repo directory** (e.g. `/tmp`) → `repo: non-repo (no .loom/ found from cwd)`

## Config resolution (AC5)

`loom status` resolves configuration through the Phase 2 tier resolver
(`defaults/scripts/lib/config-resolver.sh`: private defaults → `.loom/config.json`
→ `.loom-project/project.json` → `.loom-local/local.json`) rather than reading
`.loom/config.json` directly, so a value overridden in `.loom-local/local.json`
wins. In a **non-repo** directory only the private-defaults tier contributes
(graceful degradation, not an error). When `jq` is unavailable the dispatcher
says so explicitly — it does **not** present a `jq`-less host as "no config".

## `update` is a thin verb (scope guard, Finding 3)

`loom update` only resolves the machine checkout and delegates to the **existing**
`loom-daemon-update.sh` (built by #3968, extended by the shipped #4055 self-update
loop). It implements **no** rebuild / reprovision / restart logic itself, so it
neither pre-empts #4017 (auto-rebuild-when-stale) nor duplicates #4055.

## Machine mode: LOOM_MACHINE_CHECKOUT hand-off (Phase 3b, #4229)

Phase 3a shipped `start`/`stop`/`update` as delegates, but each of the three
lifecycle scripts (`loom-daemon-start.sh`, `-stop.sh`, `-update.sh`) still
resolved its own operating root by walking up from `$PWD`
(`find_repo_root()`), independent of what this dispatcher had already
resolved. That produced two concrete gaps, closed here:

1. **`loom update` failed outside a Loom source checkout.** From a consumer
   repo, `find_repo_root()` found the consumer repo (no
   `loom-daemon/Cargo.toml` there) and refused; from a non-repo directory it
   found nothing at all and refused with "Not in a Loom workspace" — even
   though this dispatcher had *already* resolved and validated the machine
   checkout.
2. **`loom start`/`stop` bound machine-global daemon state to whichever repo
   they were invoked from.** The launchd label (`com.rjwalters.loom-daemon`)
   is a machine-wide singleton, but the rendered plist's `WorkingDirectory`
   and the `.daemon.pid`/`.daemon.flags` files were `$REPO_ROOT`-relative — so
   `loom start` from repo A and `loom update` from repo B could read/write two
   different pid/flags files against the same launchd job.

**The fix**: every verb that delegates into the checkout (`start`, `stop`,
`update`, `restart`) exports `LOOM_MACHINE_CHECKOUT=<resolved checkout>` before
exec'ing/invoking its lifecycle-script delegate. Each lifecycle script now
checks this variable *first*, ahead of its `$PWD`-based `find_repo_root()`
fallback:

- **Set** (machine mode — always true for a dispatcher-driven invocation): the
  checkout is used as the operating root (plist `WorkingDirectory`, the
  `loom-daemon/Cargo.toml` rebuild target for `update`) **regardless of
  `$PWD`**, and runtime artifacts — `.daemon.pid`, `.daemon.flags`, the
  startup log — resolve under `$HOME/.loom` (the pid/flags decision below),
  not under the checkout or the invoking directory.
- **Unset** (direct invocation of a lifecycle script, no dispatcher — the
  pre-#4229 dev workflow): every script behaves **byte-for-byte** as before,
  `$PWD`-based `find_repo_root()` included. Machine mode is strictly additive.

### The pid/flags relocation decision

`#4042` already established that a `.loom/.daemon.pid` file is an unreliable
running-state source under launchd (`KeepAlive:{SuccessfulExit:true}` assigns
a fresh pid on every supervised relaunch) — which argued for dropping pid
files entirely under launchd and treating `launchctl print` as the sole
source of truth. This unit takes the **narrower, lower-risk option** instead:
relocate `.daemon.pid`/`.daemon.flags`/the startup log from
`$REPO_ROOT/.loom/` to `$HOME/.loom/` in machine mode, rather than removing
pid-file tracking altogether. `$HOME/.loom/` is not new state — it is the
**existing** machine-level state home (socket, token pool, `activity.db`,
`daemon.log` already live there); this only adds a few more files to a
directory that was already the machine-level source of truth, and the
pid-file/nohup fallback tier every lifecycle script's own ownership-detection
logic already has (see `loom-daemon-update.sh`'s `DAEMON_MANAGER` resolution)
keeps working unchanged. No existing state (socket, tokens, `activity.db`,
logs) moves. Dropping pid files entirely under launchd remains available as a
future, more invasive follow-up if the pid-file tier ever proves more
confusing than useful in machine mode.

### `restart` verb (Gap 3)

`loom restart` mirrors `start`/`stop` — same collision guard — and prefers a
**drain-and-roll** restart: it first tries the daemon's own supervised restart
IPC (`loom-daemon restart`, #4077), which never tears down in-flight sweep
children (unlike a `launchctl bootout`). If that is unavailable (not
launchd-managed) or refused (not currently running, or a pre-#4077 binary), it
falls back to a plain stop-then-start via the same checkout-resolved
lifecycle-script delegates.

### Supervision (reboot/crash) — macOS done, Linux deferred

Reboot/crash supervision itself (as opposed to the workdir/pid-file relocation
above) is already implemented and documented for macOS via launchd —
`RunAtLoad` (#3972), `KeepAlive:{SuccessfulExit:true}` restart-only relaunch
(#4054), and a `StartInterval` autonomy-loss watchdog (#4011), all resolved
through the `gui/<uid>` ↦ `user/<uid>` domain fallback (#4130). See
[`daemon-reference.md`](daemon-reference.md) → Operability for the full
writeup. **Linux has no equivalent** — the non-Darwin path is a plain `nohup …
&` with no reboot/crash recovery and no watchdog. This is tracked as a named
follow-up, #4260, rather than designed inline here.

## Uninstall semantics

A per-repo `uninstall-loom.sh` removes only the per-repo `./.loom/bin/loom` pool
manager. The machine-level `~/.local/bin/loom` dispatcher and the
`~/.local/share/loom` checkout link are **not** removed by a per-repo uninstall —
same semantics as the shared `~/.local/bin/loom-daemon` binary, which outlives any
single repo's uninstall.
