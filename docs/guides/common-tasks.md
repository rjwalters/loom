# Common Tasks

## Setting Up Loom in a Repository

### Headless Initialization

Initialize a Loom workspace without the GUI application:

```bash
# Navigate to your repository
cd /path/to/your/repo

# Initialize Loom (creates .loom/, CLAUDE.md, etc.)
loom-daemon init
```

**What gets created:**
- `.loom/config.json` - Terminal configurations and role assignments
- `.loom/roles/` - Default role definitions (builder.md, judge.md, etc.)
- `CLAUDE.md` - AI context documentation for the repository
- `.claude/` - Claude Code slash commands
- `.github/labels.yml` - GitHub label definitions
- `.gitignore` updates - Ephemeral pattern additions

**See also:**
- [Getting Started Guide](getting-started.md) - Complete installation walkthrough
- [CLI Reference](cli-reference.md) - Full `loom-daemon init` documentation

### Customizing Defaults for Your Team

Create organization-specific defaults:

```bash
# 1. Create a defaults repository
mkdir my-org-loom-defaults
cd my-org-loom-defaults

# 2. Copy Loom's defaults as a starting point
cp -r /path/to/loom/defaults/* .

# 3. Customize for your organization
# - Edit config.json (default terminal setup)
# - Modify roles/ (custom role definitions)
# - Update CLAUDE.md template
# - Add org-specific .github/ workflows

# 4. Commit and push
git init
git add .
git commit -m "Initial org defaults"
git remote add origin https://github.com/my-org/loom-defaults.git
git push -u origin main

# 5. Use in projects
cd /path/to/project
loom-daemon init --defaults /path/to/my-org-loom-defaults
```

### Post-Init Configuration

After initializing a workspace:

#### 1. Sync GitHub Labels

```bash
# Sync labels defined in .github/labels.yml
gh label sync -f .github/labels.yml

# Verify
gh label list | grep "loom:"
```

#### 2. Customize Terminal Roles

Edit `.loom/config.json` to configure default terminals:

```json
{
  "nextAgentNumber": 8,
  "terminals": [
    {
      "id": "1",
      "name": "Builder",
      "role": "claude-code-builder",
      "roleConfig": {
        "workerType": "claude",
        "roleFile": "builder.md",
        "targetInterval": 0,
        "intervalPrompt": ""
      }
    }
  ]
}
```

Or use the GUI to configure terminals visually.

#### 3. Create Custom Roles

Add project-specific roles to `.loom/roles/`:

```bash
# Create a custom role definition
cat > .loom/roles/documenter.md <<'EOF'
# Documenter

You are a documentation specialist for the {{workspace}} repository.

## Your Role

Maintain comprehensive, up-to-date documentation...
EOF

# Create metadata (optional)
cat > .loom/roles/documenter.json <<'EOF'
{
  "name": "Documenter",
  "description": "Documentation maintenance specialist",
  "defaultInterval": 0,
  "defaultIntervalPrompt": "",
  "autonomousRecommended": false,
  "suggestedWorkerType": "claude"
}
EOF
```

#### 4. Update CLAUDE.md

Customize the AI context documentation for your project:

```markdown
# My Project - AI Development Context

## Project Overview

[Describe your project's purpose and architecture]

## Technology Stack

[List technologies used]

## Development Workflow

[Explain how to work on this project]

...
```

See [defaults/CLAUDE.md](../../defaults/CLAUDE.md) for the complete template.

### Troubleshooting Initialization

#### Issue: "Not a git repository"

**Solution:**
```bash
# Initialize git first
git init
loom-daemon init
```

#### Issue: ".loom already exists"

**Solution:**
```bash
# Option 1: Keep existing config (no action needed)
# Already initialized!

# Option 2: Reset to defaults
loom-daemon init --force

# Option 3: Manual cleanup
rm -rf .loom CLAUDE.md .claude
loom-daemon init
```

#### Issue: "Permission denied"

**Solution:**
```bash
# Check ownership
ls -la

# Fix permissions
chmod u+w .
loom-daemon init
```

#### Issue: Initialization succeeds but files are missing

**Cause:** Partial initialization from previous failure

**Solution:**
```bash
# Force complete reinitialization
loom-daemon init --force
```

**See also:**
- [Troubleshooting Guide](troubleshooting.md#initialization-issues) - Complete debugging guide
- [Getting Started](getting-started.md#troubleshooting) - Common setup issues

## Adding a New Agent Terminal Property

1. Update interface in `src/lib/state.ts`:
   ```typescript
   export interface Terminal {
     id: string;
     name: string;
     status: TerminalStatus;
     isPrimary: boolean;
     workingDirectory?: string; // NEW
   }
   ```

2. Update UI rendering in `src/lib/ui.ts`:
   ```typescript
   // Display new property
   <span>${escapeHtml(terminal.workingDirectory || 'N/A')}</span>
   ```

3. TypeScript will catch any missing properties at compile time

## Adding a New Daemon IPC Endpoint

1. Add a new variant to the request enum in `loom-api/src/protocol.rs`
2. Implement the handler in `loom-daemon/src/`
3. Add tests in `loom-daemon/tests/` (mirror the patterns in `integration_basic.rs`)
4. If exposing via MCP, add the wrapping tool in `mcp-loom/src/tools/`
