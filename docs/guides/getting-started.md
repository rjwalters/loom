# Getting Started with Loom

This comprehensive guide walks you through installing and setting up Loom, whether you're an end user wanting to use Loom in your repository or a contributor building Loom itself.

## Table of Contents

- [Before You Install](#before-you-install)
- [Prerequisites](#prerequisites)
- [Installation Options](#installation-options)
  - [Option 1: Download Binary (Easiest)](#option-1-download-binary-easiest)
  - [Option 2: Build from Source](#option-2-build-from-source)
  - [Option 3: Interactive Install Script](#option-3-interactive-install-script)
- [First-Time Setup](#first-time-setup)
- [Verifying Your Setup](#verifying-your-setup)
- [Next Steps](#next-steps)
- [Troubleshooting](#troubleshooting)

## Before You Install

### What Loom Does

Loom transforms your repository into an AI-orchestrated workspace where agents coordinate through GitHub issues, PRs, and labels. Each terminal can embody a specialized role (Worker, Curator, Architect, Reviewer) working autonomously or on-demand.

### What Gets Installed

Running `./install.sh /path/to/your/repo` (which invokes `loom-daemon init`)
creates these files in your repository:

**Configuration (Commit these)**:
- `.loom/config.json` - Terminal settings and role assignments
- `.loom/roles/` - Custom agent role definitions (optional)

**Documentation (Commit these)**:
- `CLAUDE.md` - AI context document for Claude Code (11KB template)
- `AGENTS.md` - AI context document for OpenAI Codex and other AGENTS.md-aware runtimes (generated from `CLAUDE.md`)

**Tooling (Commit these)**:
- `.claude/commands/loom/` - Claude Code slash commands for each role
- `.codex/` - Codex configuration (if available)
- `.github/labels.yml` - Workflow label definitions

**Gitignored (Local only)**:
- `.loom/state.json` - Runtime terminal state
- `.loom/worktrees/` - Git worktrees for isolated work
- `.loom/*.log` - Application log files

### What Gets Modified

- **`.gitignore`** - Adds patterns for `.loom/state.json`, `.loom/worktrees/`, `~/.loom/console.log`, etc.

That's it! Loom is non-invasive and everything important can be committed to version control so your team shares the same agent configuration.

## Prerequisites

### For End Users (Using Loom)

Minimal requirements to use Loom:

1. **macOS or Linux** (the daemon ships both a launchd and a systemd-user
   service wrapper — `defaults/scripts/lib/launchd-domain.sh` and
   `defaults/scripts/lib/systemd-user.sh`)
2. **Git repository** (any existing project)
3. **tmux** (not shipped with macOS — install it)
   ```bash
   # Verify tmux is installed
   tmux -V

   # Install if needed
   brew install tmux          # macOS
   sudo apt install tmux      # Debian/Ubuntu
   ```
4. **Claude Code** (optional, for AI agents)
   ```bash
   # Verify Claude Code is installed
   claude --version

   # See https://claude.com/claude-code for installation
   ```

That's all you need to use Loom!

### For Contributors (Developing Loom)

Additional requirements to build and contribute to Loom:

1. **Rust** (for daemon and api compilation)
   ```bash
   # Install Rust via rustup (recommended)
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

   # Verify installation
   rustc --version
   cargo --version
   ```

2. **System Dependencies**

   **macOS:**
   ```bash
   xcode-select --install
   ```

   **Linux (Ubuntu/Debian):**
   ```bash
   sudo apt update
   sudo apt install build-essential pkg-config libssl-dev
   ```

3. **Node.js** (v18 or later, for mcp-loom)
   ```bash
   # Install via nvm (recommended)
   nvm install 18

   # Verify installation
   node --version  # Should be v18+
   ```

4. **pnpm** (package manager) — **pin it to your Node major; do not let it float**

   | Node.js | Known-good pnpm |
   |---------|-----------------|
   | 18.x / 20.x | **10.x** (e.g. `10.15.1`) — last line that runs on Node < 22.13 |
   | 22.13+ / 24+ | 11.x |

   ```bash
   npm install -g pnpm@10.15.1     # Node 18/20
   # ...or, with corepack:
   corepack prepare pnpm@10.15.1 --activate

   # Verify installation — this must PRINT A VERSION, not an error
   pnpm --version
   ```

   > **Why pin?** `corepack enable pnpm` resolves the *latest* pnpm (currently
   > the 11.x line), which hard-requires Node >= 22.13. On a Node 18/20 host
   > that leaves a `pnpm` shim that exists but cannot run — every invocation
   > dies with `ERR_UNKNOWN_BUILTIN_MODULE: No such built-in module:
   > node:sqlite`. A fleet straddling two Node majors hits this on some hosts
   > and not others. `install.sh` now actually runs `pnpm --version` during its
   > dependency check and reports this explicitly rather than letting it
   > surface later inside `pnpm daemon:build`.
   >
   > **Do not run `corepack prepare` under `sudo`.** The shim and the resolved
   > version live in different places: `sudo corepack enable pnpm` installs the
   > shim at `/usr/bin/pnpm` (root), while `corepack prepare pnpm@X --activate`
   > resolves the version **per user** under `~/.cache/node/corepack/`. Running
   > `prepare` under sudo pins root's cache while your own shell keeps
   > resolving the broken version — so the fix looks like it silently failed.
   > Run `enable` with sudo if needed, but `prepare` as the invoking user.

5. **GitHub CLI** (optional, for agent workflows)
   ```bash
   # macOS
   brew install gh

   # Linux - see https://cli.github.com/

   # Authenticate
   gh auth login
   ```

To verify all prerequisites:

```bash
# Check Rust (contributors only)
rustc --version && cargo --version

# Check Node.js (contributors only)
node --version

# Check pnpm (contributors only) - must print a version, not a Node error;
# see the pnpm/Node pairing table above if it fails
pnpm --version

# Check tmux (all users)
tmux -V

# Check Claude Code (optional)
claude --version

# Check GitHub CLI (optional)
gh --version
```

## Installation Options

Choose the installation method that best fits your needs:

### Option 1: Download Binary (Easiest)

Perfect for end users who want to use Loom without building from source.

```bash
# Download latest release
curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon -o loom-daemon
chmod +x loom-daemon

# Initialize your repository
./loom-daemon init /path/to/your/repo
```

**What this does:**
- Downloads the pre-built daemon binary
- Makes it executable
- Initializes your repository with Loom configuration

**Next:** See [First-Time Setup](#first-time-setup) to explore what was created.

### Option 2: Build from Source

For contributors or users who want the latest development version.

```bash
# Clone Loom repository
git clone https://github.com/rjwalters/loom
cd loom

# Install dependencies
pnpm install

# Build daemon
pnpm daemon:build

# Initialize your repository
./target/release/loom-daemon init /path/to/your/repo
```

**What this does:**
- Clones the Loom source code
- Installs Node.js dependencies via pnpm
- Builds the Rust daemon from source
- Initializes your repository

**Next:** See [DEVELOPMENT.md](development.md) for development workflow.

### Option 3: Interactive Install Script (Recommended)

Uses the interactive install script for guided installation with two workflows.

```bash
# Clone Loom first (if you haven't)
git clone https://github.com/rjwalters/loom
cd loom

# Run interactive installer (will prompt for target repo if not provided)
./install.sh

# Or specify target repository directly
./install.sh /path/to/your/repo
```

**What this provides:**
- Interactive prompts for repository path
- Shows exactly what will be installed before proceeding
- Two installation methods:
  - **Quick Install (Option 1)**: Direct installation with `loom-daemon init`
  - **Full Install (Option 2)**: Automated workflow with GitHub issue, worktree, and PR
- Git repository validation
- GitHub authentication checks (for Full Install)
- Confirmation prompts at each step
- Clear error messages and recovery suggestions

**When to use Quick Install:**
- Personal projects or quick testing
- Solo development
- No need for GitHub issue tracking
- Want minimal setup

**When to use Full Install:**
- Team projects requiring review
- Want installation tracked in GitHub issue and PR
- Prefer git worktree isolation for clean separation
- Need labels automatically synced to repository
- Want to review changes before merging

The Full Install workflow:
1. Creates a GitHub issue to track the installation
2. Creates a git worktree for isolated work
3. Runs `loom-daemon init` in the worktree
4. Syncs GitHub labels from `.github/labels.yml`
5. Creates a pull request with all changes
6. Automatic cleanup if any step fails

**Advanced:** For programmatic installation without prompts, use:
```bash
# Automated full workflow (no interactive prompts)
./scripts/install-loom.sh /path/to/your/repo
```

This runs the complete Full Install workflow automatically.

**Next:** Review the output to understand what was created.

### Initialization Flags

The `loom-daemon init` command supports several flags for customization:

```bash
# Initialize current directory
loom-daemon init

# Initialize specific repository
loom-daemon init /path/to/your/repo

# Preview changes without applying them
loom-daemon init --dry-run

# Overwrite existing .loom directory
loom-daemon init --force

# Use custom defaults directory
loom-daemon init --defaults ./custom-defaults
```

## First-Time Setup

After installing Loom, you'll find the following files in your repository:

### Workspace Configuration (`.loom/`)

```
.loom/
├── config.json       # Terminal configurations, roles, agent counter
├── roles/            # Custom role definitions (initially empty)
└── README.md         # Documentation about .loom directory
```

**What to do:**
1. Review `config.json` to understand default terminal setup
2. Leave `roles/` empty unless you want custom role definitions
3. Read `.loom/README.md` for configuration guidance

### AI Context Documentation

```
CLAUDE.md             # Technical context for Claude Code agents
AGENTS.md             # Technical context for OpenAI Codex and other AGENTS.md-aware agents
```

**What to do:**
1. Review `CLAUDE.md` (Claude Code) and/or `AGENTS.md` (Codex) to understand the codebase structure and patterns
2. Update `CLAUDE.md` with project-specific context as you build. `AGENTS.md` is generated from `CLAUDE.md`'s `agents-md:include` ranges (via `defaults/scripts/generate-agents-md.sh`), so it stays in sync automatically — do not hand-edit it.

### Claude Code Configuration

```
.claude/
├── commands/
│   └── loom/         # Loom slash commands for Claude Code
└── README.md         # Documentation
```

**What to do:**
1. Explore available slash commands in `.claude/commands/loom/`
2. Add custom slash commands for your project
3. See [Claude Code docs](https://docs.claude.com/en/docs/claude-code) for details

### GitHub Configuration

```
.github/
├── labels.yml        # Label definitions for workflow coordination
└── workflows/        # CI/CD workflow templates
```

**What to do:**
1. Review label definitions in `labels.yml`
2. Sync labels to the forge: `.loom/scripts/sync-labels.sh`
3. Customize labels for your project's workflow

### Gitignore Updates

Loom automatically updates `.gitignore` with ephemeral patterns:

```gitignore
# Loom - AI Development Orchestration
.loom/state.json
.loom/worktrees/
.loom/*.log
.loom/*.sock
```

**What to commit:**
- ✅ `.loom/config.json` - Share terminal roles across team
- ✅ `.loom/roles/` - Custom role definitions
- ✅ `CLAUDE.md` - AI context documentation (Claude Code)
- ✅ `AGENTS.md` - AI context documentation (OpenAI Codex)
- ✅ `.claude/` - Slash commands and config
- ✅ `.github/` - Labels and workflows

**What to gitignore:**
- ❌ `.loom/state.json` - Runtime state (session IDs, ephemeral data)
- ❌ `.loom/worktrees/` - Git worktrees (temporary workspaces)
- ❌ `.loom/*.log` - Log files
- ❌ `.loom/*.sock` - Unix socket files

### Local (uncommitted) install mode (`--local`)

By default the installer commits the Loom implementation (`.loom/`,
`.claude/commands/loom/`, `.claude/agents/loom-*.md`, hooks) into the consumer
repo. If you would rather use Loom in a repo **without** publishing/duplicating
that implementation in the repo's history, run the installer in local mode:

```bash
./scripts/install-loom.sh --local /path/to/your/repo            # print untrack commands
./scripts/install-loom.sh --local --untrack /path/to/your/repo  # also run them
```

`--local` (alias `--gitignore`) does **not** run the full copy-into-worktree +
PR install. Instead it appends a Loom-managed, marker-delimited block to the
target repo's `.gitignore` (idempotent — re-running refreshes it in place):

```gitignore
# >>> loom-local install (do not edit) >>>
# Loom implementation files — installed via 'install-loom.sh --local', not committed to this repo (#3836)
/.loom/
/.claude/commands/loom/
/.claude/agents/loom-*.md
/.loom-local/
# <<< loom-local install <<<
```

Because `.gitignore` alone will not stop tracking files that are already
committed, local mode also detects any of those paths that git currently tracks
and prints the exact `git rm -r --cached …` commands to untrack them. Pass
`--untrack` to run those commands automatically (they stage deletions from the
index; the files stay on disk — commit to finalize).

> **Note:** local mode intentionally leaves genuinely project-specific config
> tracked — `.github/labels.yml` is not under any ignored path. `.loom/config.json`
> lives under the ignored `/.loom/` tree; relocating shared project config under a
> future `.loom-project/` is left to a follow-up increment.

## Verifying Your Setup

After installation, verify everything is working correctly:

### 1. Check File Structure

```bash
# Verify .loom directory structure
tree .loom

# Expected output:
# .loom
# ├── README.md
# ├── config.json
# └── roles
#     ├── architect.md
#     ├── auditor.md
#     ├── builder.md
#     ├── champion.md
#     ├── curator.md
#     ├── doctor.md
#     ├── driver.md
#     ├── guide.md
#     ├── hermit.md
#     └── judge.md
```

### 2. Check Configuration

```bash
# View config file (should have default terminals)
cat .loom/config.json

# Expected: JSON with nextAgentNumber and terminals array
```

### 3. Verify Gitignore

```bash
# Check gitignore was updated
grep -A 4 "Loom - AI Development Orchestration" .gitignore

# Expected:
# # Loom - AI Development Orchestration
# .loom/state.json
# .loom/worktrees/
# .loom/*.log
# .loom/*.sock
```

### 4. Test Daemon (Optional)

```bash
# Start the daemon. The binary has no start/stop subcommands — those
# wrappers live under .loom/scripts/cli/
./.loom/scripts/cli/loom-daemon-start.sh

# Check health
loom-daemon health

# Stop the daemon
./.loom/scripts/cli/loom-daemon-stop.sh
```

## Next Steps

Now that Loom is installed and configured, you can:

### 1. Create the Workflow Labels (required)

Labels are the entire coordination substrate — agents claim work and hand it off
by relabeling. **A Quick Install ships `.github/labels.yml` but does not create
the labels on the forge** (that happens only on a Full Install), so a
`--quick` install has no working workflow until you run:

```bash
# `gh` has no `label sync` subcommand — use the shipped script, which
# handles both GitHub and Gitea
.loom/scripts/sync-labels.sh

# Verify — compares the live label set against .github/labels.yml on either
# forge, so it catches a partial sync. Exits 0 (in sync), 3 (drift found),
# 1 (forge/lookup error).
.loom/scripts/sync-labels.sh --check
```

`gh label list | grep "loom:"` also works, but only on GitHub, and it shows
only that *some* `loom:` label exists — not that the expected set is complete.

See [WORKFLOWS.md](../workflows.md) for what each label means.

### 2. Protect the Default Branch

```bash
# from your Loom checkout
./scripts/install/setup-branch-protection.sh /path/to/your/repo
./scripts/install/setup-repository-settings.sh /path/to/your/repo
```

Both scripts handle GitHub (rulesets API) and Gitea (branch protection API),
and need admin rights on the target repo. On Gitea, rules without an equivalent
— linear history, role-based bypass — are skipped with a warning.

This creates a ruleset requiring linear history and a pull request with **0
approvals** — the 0-approval part is what lets Champion auto-merge an approved
PR without a human in the loop.

**0 required approvals is an unattended-automation policy, not a general
branch-protection recommendation, and it does not mean "unreviewed".** Review
still happens; it is recorded as Judge's `loom:pr` label rather than as a
GitHub formal approval. The two gates are independent — GitHub's
required-approval count knows nothing about Loom's labels, so any non-zero
count deadlocks Champion (no agent can satisfy it; GitHub's API blocks
self-review). Keep a non-zero count if your repo needs a person on every merge,
and drive merges by hand with `./.loom/scripts/merge-pr.sh <PR>`.

### 3. Your First Sweep

`/loom:sweep` is the main entry point: it runs the whole
Curator → Builder → Judge → Doctor → Merge lifecycle on one issue, checkpointed
under `.loom/sweep-checkpoint/` so a crash resumes rather than restarts.

Write a real issue, then from Claude Code in your repo:

```bash
cd /path/to/your/repo
claude
```

```
/loom:sweep 42
```

**Make the first one small and bounded**: one issue, scoped so you can read
the whole diff. Before you start, confirm `gh auth status` succeeds and that
you know the project's real test command — a sweep runs it, and a first run
that fails on a missing toolchain teaches you nothing about Loom.

#### What happens when the Judge asks for changes

`/loom:sweep` runs this loop for you, but it is worth recognizing, because a
PR sitting in it is not finished work:

| PR label | Who acts next | What they do |
|----------|---------------|--------------|
| `loom:review-requested` | **Judge** | Reviews the diff, then applies exactly one verdict |
| `loom:changes-requested` | **Doctor** | Addresses the feedback on the PR branch, then relabels back to `loom:review-requested` |
| `loom:pr` | **Champion** | Auto-merges (this is the only terminal state) |

So Doctor — not Builder — owns a rejected PR, and the cycle is
Judge → Doctor → **Judge again** → merge. Doctor never applies `loom:pr`
itself; re-review is mandatory. **Doctor is conditional**: it runs only if the
Judge requested changes. On an approval the PR goes straight to merge, which is
why the lifecycle is written Curator → Builder → Judge → *Doctor (if needed)* →
Merge.

Verdicts are scoped to the tree they were rendered against (#5686): once a PR's
head SHA moves, a stale `loom:changes-requested` or `loom:pr` is cleared and the
PR returns to `loom:review-requested` for a fresh evaluation. A verdict label
left over from an older commit is never trusted.

#### Scaling up

Once you trust it:

```
/loom:sweep 42 43 44     # parallel builder waves
/loom:sweep all          # the whole open backlog — aggressive; see below
```

`/loom:sweep all` is the deliberately fast/sloppy "build everything" path: it
takes **every** open issue regardless of label, promotes uncurated ones, and
reclaims stale claims. It prompts for confirmation with the resolved candidate
set before spawning anything — read that list. It is not an onboarding command.

You can also drive a single stage by hand — `/loom:builder`, `/loom:judge`,
`/loom:curator`, `/loom:doctor` — but the full lifecycle must run in order;
a PR labeled `loom:review-requested` is only the Builder stage, not finished work.

### 4. Start the Daemon (continuous mode)

```bash
./.loom/scripts/cli/loom-daemon-start.sh
./.loom/scripts/cli/loom-status.sh
```

By default the daemon is **not a work generator**. It runs only the sweeps you
hand it. There are three distinct ways work reaches it — know which one you are
using:

| | Where it runs | Work comes from |
|---|---|---|
| `/loom:sweep 42` | Your current Claude session, in the foreground | You, one command at a time |
| `loom-daemon dispatch 42` | The daemon, in the background | You, enqueued |
| `autonomous.workFinder` | The daemon, in the background | The daemon finds its own |

To enqueue one issue and watch it, with the daemon running:

```bash
# Enqueue — prints the sweep id it accepted
loom-daemon dispatch 42

# Confirm it was accepted and watch progress
loom-daemon status

# Back out if you change your mind (never hand-kill the pids)
loom-daemon cancel --issue 42
```

Inside a Claude session with the Loom MCP server registered, the same three
operations are the `mcp__loom__dispatch_sweep`, `get_sweep_status`, and
`cancel_sweep` tools. Those are MCP tool names, not shell commands — you ask
Claude for them, you do not type them in a terminal.

To let the daemon generate its own work, opt in through the `autonomous` block
in `.loom/config.json`:

```json
"autonomous": {
  "roleRunner": { "enabled": true, "roles": ["curator", "champion", "judge", "doctor", "guide"] },
  "workFinder": { "enabled": true, "maxConcurrent": 3 }
}
```

Enable `roleRunner` first (it runs the periodic support roles), then
`workFinder` (it finds its own work) once the support roles behave.

**Editing this block while the daemon is running does not uniformly take
effect.** The `autonomous.roleRunner.*` sub-block is re-read on the next tick,
so those edits are live. But `workFinder.enabled` and `workFinder.maxConcurrent`
are resolved once during bring-up and frozen for the life of the process — a
config edit alone changes nothing, so **restart the daemon** after touching them
(#5963). The `work_finder: enabled (…)` startup log line names the resolved
value and which layer supplied it, so you can confirm the edit landed.

`maxConcurrent` is a per-machine, workload-dependent ceiling; the shipped
default is `3`. Raise it only from evidence (`loom-daemon calibrate`,
`loom-daemon status`) — ~10 is reasonable on an 8-core API-bound worker, while
2–3 is right on the same hardware running heavier sweeps. Full reference:
[`.loom/docs/daemon-reference.md`](../../.loom/docs/daemon-reference.md).

### 5. Provision a Token Pool (before long runs)

Optional, and not needed for your first sweeps. A single account is fine until
its weekly limit becomes the thing that stalls the pipeline — on an exhausted
pool `spawn-claude.sh` exits `78` (`EX_CONFIG`). Before a long unattended run,
rotate across several accounts so one account's limit cannot stall everything:

```bash
loom-daemon tokens bootstrap
loom-daemon tokens check --ranking
```

Full reference: [`.loom/docs/token-pool.md`](../../.loom/docs/token-pool.md).

### 6. Customize Roles

Create custom role definitions for your project:

```bash
# Create custom role
cat > .loom/roles/my-role.md <<'EOF'
# My Custom Role

You are a specialist in the {{workspace}} repository.

## Your Role

[Define the role's purpose and responsibilities]

## Your Workflow

[Define the workflow steps]
EOF

# Create metadata (optional)
cat > .loom/roles/my-role.json <<'EOF'
{
  "name": "My Custom Role",
  "description": "Brief description",
  "defaultInterval": 600000,
  "defaultIntervalPrompt": "Continue working",
  "autonomousRecommended": true,
  "suggestedWorkerType": "claude"
}
EOF
```

See [defaults/roles/README.md](../../defaults/roles/README.md) for role creation guidance.

### 7. Learn the Workflows

Read the comprehensive workflow documentation:

- [WORKFLOWS.md](../workflows.md) - Agent coordination patterns
- [Agent Archetypes](../philosophy/agent-archetypes.md) - Role philosophy
- [Git Workflow](git-workflow.md) - Branch strategy and PR process

## Troubleshooting

### Issue: "Not a git repository" Error

**Symptom:**
```
Error: Not a git repository (no .git directory found): /path/to/dir
```

**Solution:**
```bash
# Initialize git repository first
git init

# Or navigate to an existing git repository
cd /path/to/your/git/repo
loom-daemon init
```

### Issue: ".loom directory already exists"

**Symptom:**
```
Error: Workspace already initialized (.loom directory exists). Use --force to overwrite.
```

**Solution:**

**Option 1: Keep existing configuration**
```bash
# If .loom is already set up, you're done!
# No need to re-initialize
```

**Option 2: Reset to defaults**
```bash
# Overwrite with fresh defaults
loom-daemon init --force

# Or manually remove and re-initialize
rm -rf .loom
loom-daemon init
```

### Issue: "Permission denied" Errors

**Symptom:**
```
Error: Failed to create .loom directory: Permission denied
```

**Solution:**
```bash
# Check directory permissions
ls -la

# Ensure you own the directory
sudo chown -R $(whoami) /path/to/repo

# Or run with appropriate permissions
cd /path/to/repo  # as the owner
loom-daemon init
```

### Issue: "Defaults directory not found"

**Symptom:**
```
Error: Defaults directory not found. Tried paths: ...
```

**Solution:**

**For CLI users:**
```bash
# Specify defaults directory explicitly
loom-daemon init --defaults /path/to/loom/defaults
```

**For developers:**
```bash
# Ensure you're in the Loom repository root
cd /path/to/loom
loom-daemon init /path/to/target/repo
```

### Issue: Corrupted Scaffolding Files

**Symptom:**
- `.loom/config.json` is invalid JSON
- Role files are empty or corrupted
- `CLAUDE.md` is malformed

**Solution:**
```bash
# Reset to factory defaults
loom-daemon init --force

# Or manually repair specific files
cp defaults/config.json .loom/config.json
cp defaults/.loom/CLAUDE.md ./.loom/CLAUDE.md
```

### Issue: Labels Not Syncing to GitHub

**Symptom:**
```
Error: gh: command not found
```

**Solution:**
```bash
# Install GitHub CLI
brew install gh

# Authenticate
gh auth login

# Sync labels
.loom/scripts/sync-labels.sh
```

### Need More Help?

- **Documentation**: Check [docs/guides/](.) for detailed guides
- **Troubleshooting**: See [troubleshooting.md](troubleshooting.md)
- **Issues**: Report bugs at [GitHub Issues](https://github.com/rjwalters/loom/issues)
- **MCP Tools**: Use MCP servers for debugging (see [testing.md](testing.md))

## Summary

You've successfully installed Loom and are ready to start orchestrating AI agents!

**Key Takeaways:**
- ✅ Loom works within git repositories
- ✅ `/loom:sweep <issue>` runs the full lifecycle; the daemon runs it continuously
- ✅ Configuration lives in `.loom/` (partially gitignored)
- ✅ Agents coordinate via GitHub labels
- ✅ Customize roles for your project's needs

**Next:**
- Read [WORKFLOWS.md](../workflows.md) to understand agent coordination
- Review [Git Workflow](git-workflow.md) for development patterns
- Explore [Agent Archetypes](../philosophy/agent-archetypes.md) for role philosophy

Happy orchestrating! 🎭
