# `loom-worker` container mount contract

This is the **normative** filesystem contract for anything that runs a Loom
worker (or a Loom-daemon-managed session) inside a container built `FROM`
[`ghcr.io/rjwalters/loom-worker`](README.md). It replaces the informal
"Bootstrap seams" table that used to be the only documentation of these
mounts — that table now links here instead of duplicating this content (see
[README.md § Bootstrap seams](README.md#bootstrap-seams-mounts-not-baked-content)).

Every later phase of epic #6896 (session containers: persistent Codex auth,
mandatory worker containment, the `run-job` remote-execution seam) mounts
filesystems under this contract. Getting it wrong is not a performance bug —
it silently corrupts git worktrees (§1) or reintroduces a known rebuild-storm
incident (§4).

## 1. Path parity (the load-bearing rule)

**The host workspace root MUST be mounted at the identical absolute path
inside the container.**

```bash
# CORRECT — host path and container path are byte-identical
docker run --rm \
  -v /home/loom/workspaces:/home/loom/workspaces \
  -w /home/loom/workspaces/loom \
  ghcr.io/rjwalters/loom-worker:<version> ...

# WRONG — remaps the host path to something else
docker run --rm \
  -v /home/loom/workspaces:/workspace \
  ghcr.io/rjwalters/loom-worker:<version> ...
```

### Why this is load-bearing

Git worktrees are a bidirectional graph of **absolute-path** pointers, not
relative ones:

- `.loom/worktrees/issue-N/.git` is a *file* (not a directory) containing
  `gitdir: <absolute-path-to-main-repo>/.git/worktrees/issue-N`.
- The main repo's `.git/worktrees/issue-N/gitdir` file points back:
  `<absolute-path-to-worktree>/.git`.
- `.git/worktrees/issue-N/commondir` and the object-store references inside
  it are likewise absolute.

If the container sees the workspace at a different absolute path than the
host wrote into these pointer files, every git operation inside the
container that dereferences a cross-repo pointer (`git status`, `git commit`,
`git worktree list`, anything touching the shared object store) fails or —
worse — silently resolves to the wrong path. This is exactly what
`test-mount-contract.sh` (§ below) proves with a positive/negative pair.

Path parity has a second, independent benefit: it makes host-authored
absolute paths — `-v` flags in a remote-job spec (Phase 4's `run-job` seam),
file references a script writes into a comment or log, a `CARGO_TARGET_DIR`
env var — resolve identically whether they're interpreted on the host or
inside the container. No translation layer is needed anywhere in the
dispatch chain.

### Relationship to the base image's `WORKDIR /workspace`

`docker/worker/README.md`'s `FROM` contract documents `WORKDIR /workspace` as
the image's **default** — that default is unaffected by this section and
remains valid for **standalone use** (a one-off `docker run` against an
ad-hoc checkout with no worktree fan-out, e.g. a developer smoke-testing the
image locally per README.md § "Building and testing locally").

**Loom-managed dispatch — anything driven by `spawn-worker.sh`, the daemon's
containerized-dispatch mode (epic #6896 Phase 3), or the `run-job` seam
(Phase 4) — MUST use a parity mount and override `-w`/`WORKDIR` to the parity
path, not `/workspace`.** The two conventions do not conflict: `/workspace`
is what you get if you don't opt into worktree-aware dispatch; parity mounts
are what worktree-aware dispatch requires.

## 2. Secrets mounts

Restated from the base image's `FROM` contract: **zero secrets are ever baked
into a `loom-worker` (or downstream session) image.** Every credential
arrives at `docker run` / `docker exec` time, via one of the mechanisms
below.

| Path | Contents | Mount mode | Notes |
|---|---|---|---|
| `/home/loom/.loom/tokens` (parity-mounted host path) | The multi-account Claude OAuth token pool | **Read-only** (`:ro`) | A worker container reads a token via the existing rotation logic in `spawn-claude.sh`; it never writes back into the pool. |
| `$CODEX_HOME` (the account's profile directory) | Mutable `auth.json` + refresh-chain state | **Read-write** | Codex's `auth.json` is refreshed in place — this is the one credential mount that is *not* read-only. It MUST be a per-account **volume** (or a parity-mounted per-account host directory), never repo-local, and never shared across two containers/processes at once (a session-container ownership rule — Phase 2's job — prevents two writers clobbering the same refresh chain). See [`guardrail-parity-codex.md`](../../.loom/docs/guardrail-parity-codex.md) for the full `CODEX_HOME` layout. |
| `gh`/forge auth | A PAT, `GH_TOKEN`/`GITHUB_TOKEN` env var, or a read-only bind of `~/.config/gh` | Env var (preferred) or **read-only** bind mount | Never bake a token into an image layer or `ENV` instruction — both are visible via `docker history`, exactly what `test-image.sh`'s secret scan checks for. |

**Never baked into images, restated**: no token, PAT, `auth.json`, or
`accounts.env` is ever `COPY`'d, `ADD`'d, generated, or referenced by a
Dockerfile `RUN`/`ENV` at build time — for the base image or any `FROM
loom-worker` downstream layer (session image, repo-specific overlay). This is
the same guarantee `test-image.sh` already enforces for the base image via a
`docker history` scan; a downstream layer inherits the obligation even though
nothing currently automates checking it there.

## 3. uid/gid mapping

The image's default user is `loom`, **uid/gid `1000`** (`docker/worker/README.md`'s
`FROM` contract table). This is not configurable per-container without
rebuilding — it is fixed in the Dockerfile.

**Consequence for host file ownership**: any parity-mounted host path (the
workspace root, a `CODEX_HOME` volume, a repo-local cache directory) that the
container writes to must be **readable and writable by uid/gid 1000 on the
host**, because a bind mount does not remap ownership — the container process
is uid 1000 regardless of which host user's files it's touching.

- **Linux fleet hosts**: provision worker workspaces (and `.loom/tokens`,
  `CODEX_HOME` volumes, build caches) as uid/gid **1000** at creation time.
  This is normally automatic — the conventional single-worker-account fleet
  host setup already uses uid 1000 for its primary non-root user — but a host
  provisioned with a different first-user uid (common on some cloud images,
  which default the first non-root account to uid 1001+) needs an explicit
  `chown -R 1000:1000` (or `usermod`-based uid remap of the host account) once
  before containerized dispatch is enabled there.
- **Failure mode when uid/gid does not match**: the container process gets
  `EACCES`/`EPERM` on every write beneath the mismatched path — worktree
  creation, commits, and build-cache writes all fail with permission denied,
  while *reads* of world-readable files may still appear to work, which can
  make the failure look intermittent rather than a flat mismatch. This is a
  container-level failure that host-side `git`/build tooling never
  encounters, since there uid mapping is a no-op (same process, same uid).
  There is deliberately no rootless/`--userns-remap` workaround documented
  here — see the base image's shape decision (`docker/worker/README.md` §
  "Shape decision"): the container is a sweep-execution environment provided
  and provisioned by Loom's own fleet tooling, not a general-purpose
  multi-tenant runtime, so "provision the host directory at uid 1000" is the
  supported answer, not a remap layer.

## 4. Build-cache placement

**Language build caches — `CARGO_TARGET_DIR` (Rust), and the equivalent for
any other toolchain a downstream image adds — MUST live under a
parity-mounted, host-visible, container-visible path, shared across every
worktree of a given repo on that host.** They must never default to a
path that exists only inside one ephemeral container's writable layer.

### Why: the #6013/#6014 finding

`.loom/hooks/post-worktree.sh`'s binary-reuse fast path (the fix for #2291's
cargo-lock contention) copies an already-built binary from the main
workspace instead of rebuilding for every new worktree. #6013 found that this
fast path silently breaks whenever `CARGO_TARGET_DIR` is redirected somewhere
the hook's hardcoded lookup doesn't check — the fast path falls through to a
full `cargo build --release` for *every* worktree creation, and under
concurrent worktree creation (#6014's compounding lock-contention finding)
that turned into a multi-hour, fleet-wide rebuild storm from a single
redirected-target-dir host.

Containers make exactly this misconfiguration the *default* unless the mount
contract states otherwise: an ephemeral per-sweep container (epic #6896
Phase 3) that doesn't mount a build-cache path gets a **fresh, empty**
`target/` inside its own writable layer on every `docker run` — worse than a
redirected-but-still-persistent host cache, because now *every single sweep*
pays a full rebuild, not just hosts with an unusual `~/.cargo/config.toml`.

### The rule

- Resolve the build-cache location the same way the rest of the repo already
  does — `scripts/cargo-target-dir.sh` (env → `cargo metadata` → `<repo>/target`
  fallback) — and mount **that resolved path**, not an assumed default, into
  the container at the identical absolute path (path parity, §1, applies to
  caches too).
- The mounted path MUST be **shared across every worktree of the repo on that
  host**, matching `post-worktree.sh`'s existing binary-reuse assumption — one
  cache directory per repo-on-host, not one per worktree and not one per
  container.
- Do **not** point `CARGO_TARGET_DIR` at a path that lives only inside the
  container's own filesystem (no host mount backing it) — that silently
  reintroduces the #6013 failure mode, just via containment instead of a
  `~/.cargo/config.toml` redirect.
- The same rule generalizes to any other language cache a downstream image
  adds (`node_modules`/pnpm store, `.venv`, Go module cache, …): host-visible,
  container-visible, shared per-repo-per-host, never trapped inside one
  container's ephemeral layer.

## 5. Worktree-correctness test

`test-mount-contract.sh` (sibling to `test-image.sh`, wired into the same
`ci.yml` leg) proves §1 concretely against a scratch git repo:

- **Positive case**: with a parity mount, a container can run `git status`
  cleanly inside a `git worktree add`-created worktree, commit there, and the
  host sees the resulting commit in the worktree's history.
- **Negative case**: the identical worktree, mounted at a *different*
  (non-parity) container path, fails — proving the positive case is actually
  exercising path parity and not passing by accident.

The test runs wherever docker is available (including CI's `worker-image-smoke`
leg) and **skips cleanly (exit 0, not a failure)** on a docker-less host —
consistent with `test-image.sh`'s own CI wiring.

## Related

- [`README.md`](README.md) — the base image's shape decision and `FROM`
  contract; § "Bootstrap seams" now points here instead of duplicating this
  content.
- [`test-image.sh`](test-image.sh) — the base image's existing smoke test
  (secrets scan, non-root user, core toolchain).
- [`test-mount-contract.sh`](test-mount-contract.sh) — the worktree-correctness
  test this contract requires (§5).
- #6013 / #6014 — the `CARGO_TARGET_DIR` rebuild-storm finding §4 addresses.
- Epic **#6896** — session containers (this contract is Phase 1's
  filesystem-contract deliverable; every later phase mounts under it).
- [`.loom/docs/runtime-adapters.md`](../../.loom/docs/runtime-adapters.md) —
  the broader multi-runtime dispatch contract this mount contract's consumers
  (containerized `spawn-worker.sh` dispatch, Codex session containers) plug
  into.
