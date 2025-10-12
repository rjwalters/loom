# Loom Defaults

This directory contains default configuration files for Loom workspaces.

## Structure

- `config.json` - Default configuration for new workspaces
- `roles/` - System prompt templates for different terminal roles

## Purpose

When a workspace's `.loom/config.json` doesn't exist, Loom uses these defaults.
These files are committed to git to serve as:
- Examples of config structure
- Documentation of available settings
- Default values for new workspaces

## vs `.loom/`

- **`.loom/`** - Gitignored, created in each workspace (including this repo when dogfooding)
- **`defaults/`** - Committed to git, reference implementation

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
