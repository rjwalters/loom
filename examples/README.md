# Loom Configuration Templates

This directory contains example Loom workspace configurations to help you get started quickly.

## Available Templates

### 🚀 [Quickstart](quickstart/) - Minimal Setup (3 Terminals)
**Perfect for**: Getting started, small projects, learning Loom basics

**Terminals**:
- Shell - Manual terminal for exploring and debugging
- Builder - General development worker (implements features and fixes bugs)
- Judge - Code review specialist (reviews PRs)

**Use when**: You want a simple setup to learn Loom or work on small projects.

### 🏗️ [Full-Stack](full-stack/) - Complete Setup (8 Terminals)
**Perfect for**: Production use, complex projects, full AI-powered development

**Terminals**:
- Architect - Creates feature proposals and architectural improvements
- Curator - Enhances issues with implementation details
- Judge - Reviews pull requests thoroughly
- Builder 1, 2, 3 - Parallel feature implementation
- Doctor - Addresses review feedback and polishes PRs
- Hermit - Identifies opportunities to simplify and remove bloat

**Use when**: You want the complete Loom experience with specialized agents for every part of the development workflow.

## How to Use These Templates

These templates model **manual orchestration mode**: long-lived terminals, each
bound to one role, polling on a `targetInterval`. For most work — especially your
first run — prefer `/loom:sweep <issue>` instead, which drives a single issue
through the full Curator → Builder → Judge → Doctor → Merge lifecycle in one
command without you provisioning any terminals at all:

```bash
/loom:sweep 123
```

The daemon's `roleRunner` (`autonomous.roleRunner.enabled=true` in
`.loom/config.json`) is the equivalent of these long-lived terminals for
continuous/autonomous operation — it runs the periodic support roles (Champion,
Curator, Judge, Doctor, Auditor, Guide, Hermit) on a schedule using the same
rotated token pool as sweeps, so you don't need to keep a terminal open per role.
See [`.loom/docs/daemon-reference.md`](../.loom/docs/daemon-reference.md) for the
full role runner reference.

If you still want the manual, terminal-per-role shape these templates provide
(e.g. to watch each role's output live, or for a repo not running the daemon):

### Option 1: Copy to Existing Project

```bash
# Navigate to your project
cd /path/to/your/project

# Copy the template you want (quickstart or full-stack)
cp -r /path/to/loom/examples/quickstart/.loom .

# Start Loom and select your project as workspace
# Start each terminal to begin its role's polling loop
```

### Option 2: Start a New Project

```bash
# Create and initialize new project
mkdir my-new-project
cd my-new-project
git init

# Copy template
cp -r /path/to/loom/examples/full-stack/.loom .

# Open Loom, select this directory, and start each terminal
```

## What Gets Committed

When you use these templates, you should **commit the `.loom/` directory** to version control:

✅ **Commit these** (shared with team):
```
.loom/
└── config.json          # Terminal configurations
```

That is all either template actually ships — `.loom/config.json` and nothing
else. A real Loom install (`./install.sh` / `loom-daemon init`) additionally
populates `.loom/roles/`, `.loom/scripts/`, `.loom/hooks/`, `.loom/docs/`,
`.loom/runtimes/`, `.loom/config/`, and `.loom/README.md`; those are
installer-managed, not part of these templates.

❌ **Don't commit these** (automatically gitignored):
```
.loom/
├── .daemon.pid         # Dev script PID file
├── .daemon.log         # Dev script logs
├── daemon.sock         # IPC socket
├── state.json          # Current terminal state
├── activity.db         # Activity tracking database
└── worktrees/          # Git worktrees (one per issue)
```

Note: Production daemon logs are written to `~/.loom/daemon.log` (home directory).

Only the [`quickstart/`](quickstart/) template ships a `.gitignore` with these
patterns; [`full-stack/`](full-stack/) contains just `.loom/config.json`. Copying
either template does **not** give you the ignore rules — a real Loom install
writes them into your repo's own `.gitignore` (the `>>> loom-managed` block), so
run the installer, or copy `quickstart/.gitignore`'s patterns by hand.

## Customizing Templates

After copying a template, you can:

1. **Add custom roles** - Create `.loom/roles/my-role.md`
2. **Modify config** - Edit `.loom/config.json` to change terminal settings
3. **Adjust intervals** - Change autonomous operation timing
4. **Change themes** - Update terminal color themes

See [../.loom/README.md](../.loom/README.md) for detailed customization guide.

## Label Workflow

Both templates use GitHub labels to coordinate work between agents:

- `loom:issue` (blue) - Issue approved for work, ready for Builder
- `loom:building` (blue) - Builder is actively implementing
- `loom:curating` (amber) - Curator is enhancing issue
- `loom:treating` (amber) - Doctor is fixing bug/PR feedback
- `loom:review-requested` (green) - PR ready for review
- `loom:reviewing` (amber) - Under active review
- `loom:pr` (blue) - PR approved, ready to merge

See [../docs/workflows.md](../docs/workflows.md) for complete workflow documentation.

## Troubleshooting

### Terminals not launching?
- Ensure you're in a git repository (`git init` if needed)
- Check Claude Code is installed and in PATH
- Review console logs for errors

### Builders not finding issues?
- Install GitHub CLI: `brew install gh` (macOS) or equivalent
- Authenticate: `gh auth login`
- Create issues with `loom:issue` label

### Configuration not loading?
- Ensure `.loom/config.json` has valid JSON
- Check file permissions (should be readable)
- Try factory reset: **File** → **Factory Reset Workspace**

## Next Steps

- Read [../docs/workflows.md](../docs/workflows.md) for advanced multi-agent patterns
- Explore [../defaults/roles/README.md](../defaults/roles/README.md) to create custom roles
- Check [../CLAUDE.md](../CLAUDE.md) for development context

Happy coding with Loom! 🧵✨
