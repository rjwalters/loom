# Remote build host (heavy cargo steps leave the laptop)

**Why:** local `cargo check --all-targets` / `cargo test` runs are the slow part
of daemon work (minutes to hours on a warm tree, hours cold or on a loaded
machine). Since ADR-0021/#8764 the standard practice is: **edit and grep
locally; compile, test, and clippy on the remote box.** No exceptions for
"just one quick check" — a saturated laptop is how verification becomes
unverifiable.

## The box

Provisioned from this repository via the Repo Skills `/repo:remote` tool
(`.claude/skills/repo/scripts/repo-remote.sh`; see `infra/aws/joseph-remote-compute.md`
in the 2am repo, 2am#999, for the identity and its rules):

- instance: `m7i.2xlarge` (8 vCPU / 32 GB), us-east-1, 100 GB gp3
- ssh alias: `repo-remote-i-would-like-to-have-github-we` (in `~/.ssh/config`)
- auto-stops after 120 idle minutes; ~$0.36/hr while running
- pinned instance id lives in the (gitignored) repo `.env` — re-runs of
  `up` reuse the same box and its warm `target/`

```
.claude/skills/repo/scripts/repo-remote.sh status      # is it up?
.claude/skills/repo/scripts/repo-remote.sh up --yes    # start it (reuses pinned instance)
.claude/skills/repo/scripts/repo-remote.sh down --yes  # stop when done for the day
```

Rules that carry over from 2am#999: ≤8 vCPU is self-serve (ask before more —
the us-east-1 128-vCPU quota is shared with the fleet); stop the box when the
work is done; never touch anything tagged `Fleet=loom`.

## Working tree sync

The box's copy lives at `~/loom`, rsynced from the worktree (`.git` excluded —
the box keeps its own build state):

```
rsync -a --exclude=target --exclude=.git --exclude=node_modules \
  -e "ssh -o BatchMode=yes" . repo-remote-i-would-like-to-have-github-we:loom/
```

One-way, local → box. Do not edit on the box anything you will not rsync again
from the worktree; the worktree is canonical.

## The canonical verification commands

```
BOX="ssh -o BatchMode=yes repo-remote-i-would-like-to-have-github-we"
$BOX 'export PATH=$HOME/.cargo/bin:$PATH && cd loom && cargo check -p loom-daemon --all-targets 2>&1 | tail -3'
$BOX 'export PATH=$HOME/.cargo/bin:$PATH && cd loom && cargo test -p loom-daemon 2>&1 | grep -E "test result:|failures:" | head'
$BOX 'export PATH=$HOME/.cargo/bin:$PATH && cd loom && cargo clippy -p loom-daemon --all-targets 2>&1 | tail -5'
```

Pinned toolchain is 1.96.0 (`rust-toolchain.toml`); rustup on the box installs
it on first use.

## Environment deltas already bit us (known-good floor for the box)

These are not theoretical. On the first verification pass (2026-07-23), the
box's bare Ubuntu 22.04 AMI produced three false failures that never appear on
macOS or GitHub CI. The box image carries these fixes; a re-provisioned box
must re-apply them before its first run is trusted:

1. **`jq` must be installed** — `runtime_admission`'s conformance test runs the
   shipped shell checker, which shells out to `jq`.
   `sudo apt-get install -y jq`.
2. **git must be ≥ 2.38** — `worktree_ops::landed.rs`'s tree-equality proof
   uses `git merge-tree --write-tree` (git 2.38+). On older git the probe
   fails closed to `Landed::Unknown`, which flips two `worktree_ops::aggressive`
   tests from the expected `UnreachableHead` skip to a `LandedUnknown` skip
   (same worktree kept, different bucket — the code is correct and
   conservative; the test's expectation is the CI-git one).
   `sudo add-apt-repository -y ppa:git-core/ppa && sudo apt-get update && sudo apt-get install -y git`.

If a daemon test fails on the box but passes on CI (or the reverse), check
these two first before reading the code.

## The box as a Superset host (multi-host subagents)

Registered in the `noc0` org under the standalone CLI (`curl -fsSL
https://superset.sh/cli/install.sh | sh` at `~/superset/bin/superset` on the
box; `superset start --daemon --org noc0`):

- host id: `1496d6ff9038f2ee6952e83f9b3f55e1` (reports as
  `ip-172-31-33-246` — its private-IP hostname; rename in the app's host
  settings if that's unworkable)
- the daemon holds credentials in `~/.superset/config.json` on the box
- workspaces/terminals for subagents that should build here: target this host
  when creating them; each gets its own worktree on the same warm box
- **lifecycle caveat:** the host service does not survive a stop/terminate
  cycle — after any `repo-remote up`, re-run `superset start --daemon --org
  noc0` (one line, seconds) before expecting the host back in
  `superset hosts list`

## What stays local

Edits, `rg`, design/ADR work, `git` metadata (commit, log, diff review),
`create-issue.sh`, and the 2am worker's `node --test` (fast). The division
exists so the laptop stays responsive while a 10-minute test compile happens
off-machine.