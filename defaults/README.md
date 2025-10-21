# Loom Defaults

This directory contains default configuration files and templates for Loom workspaces.

## Structure

- `config.json` - Default configuration for new workspaces
- `roles/` - System prompt templates for different terminal roles
- `CLAUDE.md` - AI development context template (copied to workspace root)
- `AGENTS.md` - Repository guidelines template (copied to workspace root)
- `.claude/` - Claude Code configuration template (copied to workspace root)
- `.codex/` - Codex configuration template (copied to workspace root)
- `.github/` - GitHub workflow and issue templates (copied to workspace root)
  - `workflows/label-external-issues.yml` - Auto-label external contributions
  - `ISSUE_TEMPLATE/task.yml` - Development task template
  - `ISSUE_TEMPLATE/config.yml` - Issue template configuration
- `.loom-README.md` - README template for `.loom/` directory

## Purpose

### Configuration Defaults
When a workspace's `.loom/config.json` doesn't exist, Loom uses these defaults.
These files are committed to git to serve as:
- Examples of config structure
- Documentation of available settings
- Default values for new workspaces

### CLI Initialization

The `loom-daemon init` command uses this directory to initialize workspaces:

```bash
# Initialize with default configuration
loom-daemon init /path/to/repo

# Initialize with custom defaults
loom-daemon init --defaults /path/to/custom-defaults /path/to/repo
```

**What happens during initialization:**
1. Validates target is a git repository
2. Copies this entire `defaults/` directory to `.loom/`
3. Copies scaffolding files to workspace root:
   - `CLAUDE.md` - AI development context
   - `AGENTS.md` - Agent workflow guide
   - `.claude/` - Claude Code configuration
   - `.github/` - GitHub labels and workflows
4. Updates `.gitignore` with Loom ephemeral patterns

**Default Resolution** (when using `--defaults`):
1. Development mode: Uses provided path relative to current directory
2. Git worktree mode: Searches git repository root
3. Production mode: Uses bundled resources in app bundle

**Custom Organizational Defaults:**

Organizations can maintain their own defaults repository:

```bash
# 1. Create defaults repository
mkdir my-org-loom-defaults
cp -r defaults/* my-org-loom-defaults/

# 2. Customize for your organization
# - Edit config.json (default terminals)
# - Modify roles/ (custom role definitions)
# - Update CLAUDE.md template
# - Add org-specific workflows

# 3. Use in projects
loom-daemon init --defaults /path/to/my-org-loom-defaults /path/to/project
```

**See also:**
- [Getting Started Guide](../docs/guides/getting-started.md) - Installation walkthrough
- [CLI Reference](../docs/guides/cli-reference.md) - Complete `loom-daemon init` documentation
- [CI/CD Setup](../docs/guides/ci-cd-setup.md) - Pipeline integration examples

### Repository Scaffolding
During workspace initialization, Loom automatically copies scaffolding files to the workspace root **if they don't already exist**:
- `CLAUDE.md` → `<workspace>/CLAUDE.md`
- `AGENTS.md` → `<workspace>/AGENTS.md`
- `.claude/` → `<workspace>/.claude/`
- `.codex/` → `<workspace>/.codex/`
- `.github/` → `<workspace>/.github/`
  - `workflows/label-external-issues.yml` - Auto-labels external issues
  - `ISSUE_TEMPLATE/task.yml` - Development task template
  - `ISSUE_TEMPLATE/config.yml` - Template configuration

This ensures every Loom-enabled repository has consistent AI context, configuration files, and GitHub workflow automation that can be committed to version control.

**When scaffolding runs:**
- Initial workspace setup (`initialize_loom_workspace`)
- Factory reset (`reset_workspace_to_defaults`)
- New project creation (`create_local_project`)

### Dogfooding Note
When using Loom on the Loom repository itself, both versions exist:
- `defaults/CLAUDE.md` - Source template (committed)
- `CLAUDE.md` - Working instance (committed, gets updated during development)

This is intentional: defaults/ are distribution templates, the root files are the active documentation.

## vs `.loom/`

- **`.loom/`** - Per-workspace configuration (partially gitignored)
  - Ephemeral files ignored: `state.json`, `worktrees/`, `*.log`, `*.sock`
  - Configuration committed: `config.json`, `roles/`, `README.md`
- **`defaults/`** - Committed templates and reference implementation

## Config Schema

### `config.json`

```json
{
  "nextAgentNumber": 4,
  "agents": [
    {
      "id": "1",
      "name": "Shell",
      "status": "idle",
      "isPrimary": true
    },
    {
      "id": "2",
      "name": "Worker 1",
      "status": "idle",
      "isPrimary": false,
      "role": "claude-code-worker",
      "roleConfig": {
        "workerType": "claude",
        "roleFile": "worker.md",
        "targetInterval": 300000,
        "intervalPrompt": "Continue working on open tasks"
      }
    }
  ]
}
```

#### Top-level Fields

- `nextAgentNumber` (number): Counter for naming new agents (Worker 1, Worker 2, etc.)
  - Increments with each new agent
  - Persists across app restarts
  - Independent per workspace

- `agents` (array): List of terminal configurations
  - Each agent represents a terminal with its configuration
  - IDs are unique identifiers (can be UUIDs or simple numbers)
  - One agent should have `isPrimary: true` (the default selected terminal)

#### Agent Fields

**Required:**
- `id` (string): Unique identifier for the terminal
- `name` (string): Display name shown in UI
- `status` (string): Current terminal status - "idle" | "busy" | "needs_input" | "error" | "stopped"
- `isPrimary` (boolean): Whether this is the default terminal shown on workspace load

**Optional (for worker terminals):**
- `role` (string): Worker type - `undefined` (plain shell) | "claude-code-worker" | "codex-worker"
- `roleConfig` (object): Configuration specific to the worker role

#### Role Configuration (`roleConfig`)

When `role` is set to "claude-code-worker" or "codex-worker", the following fields are available:

- `workerType` (string): AI provider - "claude" | "codex"
  - "claude": Uses Claude Code (Anthropic)
  - "codex": Uses OpenAI Codex

- `roleFile` (string): Filename of the system prompt in `.loom/roles/`
  - Example: "worker.md", "issues.md", "reviewer.md"
  - Prompt files support `{{workspace}}` template variable (replaced with workspace path)
  - See `roles/` directory for available prompt templates

- `targetInterval` (number): Milliseconds between autonomous worker invocations
  - `0`: Autonomous mode disabled (manual interaction only)
  - `300000`: Worker runs every 5 minutes (recommended default)
  - Must be > 0 to enable autonomous operation

- `intervalPrompt` (string): Message sent to worker at each interval
  - Only used when `targetInterval > 0`
  - Example: "Continue working on open tasks"
  - Can be a simple nudge or specific instruction

### System Prompts

System prompts are stored as markdown files in `.loom/roles/`:

- **`default.md`** - Plain shell environment
- **`worker.md`** - General development worker
- **`issues.md`** - GitHub issue creation specialist
- **`reviewer.md`** - Code review specialist
- **`architect.md`** - System architecture and design
- **`curator.md`** - Issue maintenance and enhancement

You can create custom prompt files by adding `.md` files to `.loom/roles/` in your workspace. All prompt files will automatically appear in the Terminal Settings dropdown.
