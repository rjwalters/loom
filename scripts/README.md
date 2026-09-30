# Loom Repository Scripts

Shell tooling for developing and maintaining the Loom repository itself: CI
structural guards (`check-*.sh`), installer/uninstaller and their tests,
release/version helpers, and daemon dev-mode helpers. Scripts shipped *to
consumer installs* live under `defaults/scripts/` (installed as `.loom/scripts/`),
not here. Per the shell language policy
(`defaults/docs/shell-language-policy.md`), new executable logic belongs in a
`loom-daemon` subcommand; every script here is accounted for in
`shell-allowlist.txt`.

## Index

### CI structural guards

`check-structural.sh` runs every gate below (except advisory ones) locally,
the same set CI's `Structural Checks` job runs.

| Script | Guards |
|--------|--------|
| `check-structural.sh` | Runs all structural gates before anything expensive |
| `check-claude-md-budget.sh` | `CLAUDE.md` line budget |
| `check-markdown-token-budget.sh` | Agent-facing markdown token ratchet |
| `check-role-prompt-budget.sh` | Whole per-role prompt prefix ratchet |
| `check-file-size-budget.sh` | Oversized source file ratchet |
| `check-shell-allowlist.sh` | Every tracked `.sh` is in `shell-allowlist.txt` |
| `check-pipefail-early-exit.sh` | `pipefail` + early-exit SIGPIPE ratchet |
| `check-dangling-links.sh` | Relative markdown links resolve |
| `check-doc-anchors.sh` | Markdown `#anchor` links resolve |
| `check-doc-tocs.sh` | Generated TOCs in large docs (`--fix` regenerates) |
| `check-docs-defaults-parity.sh` | `.loom/docs` vs `defaults/docs` parity |
| `check-hooks-defaults-parity.sh` | `defaults/hooks`/`scripts` vs installed copies |
| `check-agents-md-sync.sh` | `defaults/.loom/AGENTS.md` is not stale |
| `check-gitignore-convergence.sh` | Loom-managed `.gitignore` block is current |
| `check-retired-list-drift.sh` | Deleted `defaults/` payloads are in `.loom-retired.list` |
| `check-daemon-subcommand-versions.sh` | Scripts declare the minimum `loom-daemon` version they need |
| `check-guard-scan-contracts.sh` | Consumer-tier contract on guard scan strings |
| `check-guard-destructive-drift.sh` | Advisory drift check on the vendored destructive-command guard |
| `check-vendored-private-refs.sh` | No private repo/host names in scanned trees |

Baselines/allowlists: `*-baseline.txt`, `role-prompt-budget.txt`, `shell-allowlist.txt`.

### Install / release

| Path | Purpose |
|------|---------|
| `install-loom.sh`, `uninstall-loom.sh` | Install/remove Loom in a target repo |
| `install/` | Installer helpers (label sync, hooks/skills provisioning, branch protection, migration) |
| `loom/` | Loom CLI entry-point helpers |
| `setup-mcp.sh` | Demoted MCP bundle-rebuild / legacy-migration tool |
| `version.sh` | Keep all version-bearing files in sync (release path only; never in a PR) |
| `changelog.sh` | Generate/verify Keep-a-Changelog entries |
| `test-installer.sh`, `test-install-local-mode.sh`, `test-migrate-consumer.sh`, `test-changelog.sh`, `test-daemon-liveness.sh` | Tests for the above |

### Development / maintenance

| Path | Purpose |
|------|---------|
| `dev-daemon.sh`, `start-daemon.sh`, `stop-daemon.sh`, `restart-daemon.sh`, `daemon-headless.sh`, `daemon-build.sh` | Daemon dev-mode helpers (detailed below) |
| `worktree.sh` | Worktree helper |
| `cargo-target-dir.sh` | Cargo target directory resolution |
| `cleanup-branches.sh`, `clean-tmux.sh` | Stale branch / tmux session cleanup |
| `archive-logs.sh` | Archive task outputs and daemon logs with retention |

## Daemon dev scripts

Details for the daemon development helpers:

### dev-daemon.sh
**Interactive development mode** - Starts daemon and provides live monitoring dashboard.

- **PID file**: `.loom/.daemon.pid`
- **Log file**: `.loom/.daemon.log`
- **Interactive**: Keeps terminal active with colored log streaming
- **Metrics**: Shows uptime, connections, terminals, errors, warnings
- **Auto-cleanup**: Stops daemon on Ctrl+C

Features:
- Color-coded log output (errors=red, warnings=yellow, info=green)
- Real-time activity monitoring
- Connection tracking
- Error/warning counters
- Uptime display

Usage:
```bash
./scripts/dev-daemon.sh
# or
pnpm run daemon:dev
```

**Recommended for development**: Use this in one terminal while issuing slash commands to Claude Code in another.

### start-daemon.sh
Starts the daemon in the background silently and stores its PID.

- **PID file**: `.loom/.daemon.pid`
- **Log file**: `.loom/.daemon.log`
- **Idempotent**: Won't start if already running
- **Verification**: Checks that process started successfully

Usage:
```bash
./scripts/start-daemon.sh
```

### stop-daemon.sh
Stops the daemon gracefully (or force kills if needed).

- Reads PID from `.loom/.daemon.pid`
- Sends SIGTERM first (graceful)
- Waits up to 5 seconds for process to die
- Force kills with SIGKILL if still running
- Cleans up PID file
- Fallback: Searches for process by name if PID file missing

Usage:
```bash
./scripts/stop-daemon.sh
# or
pnpm run daemon:stop
```

### restart-daemon.sh
Restarts the daemon (stop + wait + start).

Equivalent to:
```bash
./scripts/stop-daemon.sh
sleep 1
./scripts/start-daemon.sh
```

Usage:
```bash
./scripts/restart-daemon.sh
```

## Integration with pnpm

These scripts are used by the pnpm commands in `package.json`:

| Command | Description | Use Case |
|---------|-------------|----------|
| `pnpm run daemon:dev` | **Interactive dev mode** (recommended) | Two-terminal development workflow |
| `pnpm run daemon:headless` | Start daemon in background (silent) | Scripting/automation |
| `pnpm run daemon:stop` | Stop daemon | Manual control |
| `pnpm run daemon:preview` | Run daemon in foreground (cargo run) | Low-level debugging |
| `pnpm run daemon:build` | Build release daemon binary | Release packaging |

## Files Created

These scripts create files under `.loom/` (all gitignored):

- `.loom/.daemon.pid` - Process ID of running daemon
- `.loom/.daemon.log` - Daemon stdout/stderr output

## Troubleshooting

### Daemon won't start
Check the log file:
```bash
cat .loom/.daemon.log
```

Common issues:
- Port/socket already in use
- Cargo build failed
- Permissions issue

### Daemon won't stop
Force kill manually:
```bash
# Find PID
ps aux | grep loom-daemon

# Kill it
kill -9 <PID>

# Clean up
rm -f .loom/.daemon.pid
```

### Stale PID file
If `.loom/.daemon.pid` exists but daemon isn't running:
```bash
rm .loom/.daemon.pid
pnpm run daemon:dev
```

The stop script handles this automatically by checking if the PID is still alive.

## Maintenance Scripts

### clean.sh / loom-clean
**Unified cleanup for stale worktrees, branches, and build artifacts**

Cleans up:
- Stale git worktrees (for closed/merged issues)
- Merged local feature branches
- Loom tmux sessions
- Build artifacts (`target/`, `node_modules/`) with `--deep`

Flags:
- `--force` — Non-interactive mode (auto-confirm all prompts)
- `--deep` — Include build artifacts (target/, node_modules/)
- `--dry-run` — Show what would be cleaned without making changes
- `--safe` — Only remove worktrees with merged PRs
- `--aggressive` — Enumerate **all** `git worktree` entries (not just
  `.loom/worktrees/issue-*`) and remove vestigial ones reachable from
  `origin/main`. Respects open PRs, active spawn-loop tasks, the
  `.loom-managed` sentinel, and uncommitted changes; `--safe` narrows it
  further. Preview with `--dry-run` before running it for real.
- `--aggressive-min-age <seconds>` — Minimum worktree age before `--aggressive`
  will remove it (default: 24h)

Full flag list: `./clean.sh --help` (delegates to `loom-daemon clean --help`).

Usage:
```bash
./clean.sh                  # Interactive standard cleanup
./clean.sh --force          # Non-interactive cleanup
./clean.sh --deep           # Include build artifacts
./clean.sh --dry-run        # Preview only
./clean.sh --safe --force   # Safe mode, non-interactive
./clean.sh --aggressive --dry-run   # Preview vestigial-worktree cleanup
loom-clean                  # Direct invocation (PATH shim to `loom-daemon clean`, no venv required)
```

### cleanup-branches.sh
**Removes feature branches for closed issues**

Automatically cleans up stale feature branches by:
1. Scanning all `feature/issue-*` branches
2. Checking GitHub issue status (`gh issue view`)
3. Deleting branches for `CLOSED` issues
4. Preserving branches for `OPEN` issues

Usage:
```bash
# Preview what would be deleted
./scripts/cleanup-branches.sh --dry-run

# Actually delete stale branches
./scripts/cleanup-branches.sh
```

Features:
- Color-coded output (green=deleted, blue=kept, yellow=error)
- Summary statistics
- Safe: Only deletes confirmed closed issues
- Fast: Checks all branches in one run

**When to use**: After merging several PRs to keep branch list clean.

## Development Notes

- Scripts use `set -e` to exit on error
- PID is verified after starting (1 second grace period)
- Graceful shutdown with 5 second timeout
- Process detection by name as fallback
- RUST_LOG=info for informative logging
