# Loom Defaults

This directory contains default configuration files and templates for Loom workspaces.

## Structure

- `config.json` - Default configuration for new workspaces (version 2 schema)
- `config/` - Additional config data (e.g. `skill-routes.json`)
- `roles/` - Role definitions (`<role>.md` prompt + `<role>.json` metadata)
- `docs/` - Reference docs installed to `.loom/docs/`
- `scripts/` - Helper scripts installed to `.loom/scripts/`
- `hooks/` - Guard hooks installed to `.loom/hooks/`
- `runtimes/` - Runtime adapter manifests
- `optional/` - Opt-in extras (e.g. GitHub workflow templates)
- `.loom/CLAUDE.md` - AI development context template (copied to workspace root as `CLAUDE.md`)
- `.claude/` - Claude Code configuration template (copied to workspace root)
- `.github/` - GitHub labels and issue templates (copied to workspace root)
  - `ISSUE_TEMPLATE/task.yml` - Development task template
  - `ISSUE_TEMPLATE/config.yml` - Issue template configuration
- `.loom-README.md` - README template for `.loom/` directory
- `loom.sh` / `package.json` / `.loom-internal.list` - Install plumbing

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
   - `.claude/` - Claude Code configuration
   - `.github/` - GitHub labels and issue templates
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
- `.claude/` → `<workspace>/.claude/`
- `.github/` → `<workspace>/.github/`
  - `ISSUE_TEMPLATE/task.yml` - Development task template
  - `ISSUE_TEMPLATE/config.yml` - Template configuration

This ensures every Loom-enabled repository has consistent AI context, configuration files, and GitHub issue templates that can be committed to version control.

**When scaffolding runs:**
- Initial workspace setup (`initialize_loom_workspace`)
- Factory reset (`reset_workspace_to_defaults`)
- New project creation (`create_local_project`)

### Dogfooding Note
When using Loom on the Loom repository itself, both versions exist:
- `defaults/.loom/CLAUDE.md` - Source template (committed)
- `CLAUDE.md` - Working instance (committed, gets updated during development)

This is intentional: defaults/ are distribution templates, the root files are the active documentation.

## vs `.loom/`

- **`.loom/`** - Per-workspace configuration (partially gitignored)
  - Ephemeral files ignored: `state.json`, `worktrees/`, `*.log`, `*.sock`
  - Configuration committed: `config.json`, `roles/`, `README.md`
- **`defaults/`** - Committed templates and reference implementation

## Config Schema

### `config.json`

`config.json` uses the version 2 daemon schema: top-level blocks for `forge`,
`runtimes`, `safehouse`, `health_monitoring`, `reflection`, `autonomous`, and
friends, plus the `terminals` array (per-agent role/model). The committed
`defaults/config.json` is the authoritative example of the structure; the full
reference for the `autonomous` block and daemon behavior is
[`.loom/docs/daemon-reference.md`](docs/daemon-reference.md), and `buildGate` /
`runtimes` / guard-hook options are documented in the docs linked from the root
`CLAUDE.md` Configuration section.

### Role Prompts

Role prompts live as markdown files in `roles/` (installed to `.loom/roles/`),
one `<role>.md` prompt plus an optional `<role>.json` metadata file per role:
Builder, Judge, Champion, Curator, Architect, Hermit, Doctor, Guide, Driver,
Auditor, and the `loom.md` daemon-operator surface. See `roles/README.md` for
the catalog and how to add a custom role (`.loom/roles/<name>.md` in a
workspace).
