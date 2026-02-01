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

## Adding a New State Method

1. Add method to `AppState` class in `src/lib/state.ts`
2. Call `this.notify()` after state changes
3. UI will automatically re-render

## Adding a New UI Section

1. Add HTML structure to `index.html`
2. Create render function in `src/lib/ui.ts`
3. Call from `render()` in `src/main.ts`
4. Add event listeners in `setupEventListeners()`

## Adding a New Tauri Command

Tauri commands are organized into domain-specific modules in `src-tauri/src/commands/`.

### Choose the Right Module

| Module | Purpose | Example Commands |
|--------|---------|------------------|
| `terminal.rs` | Terminal management | `create_terminal`, `send_terminal_input` |
| `workspace.rs` | Workspace operations | `validate_git_repo`, `initialize_loom_workspace` |
| `config.rs` | Config/state file I/O | `read_config`, `write_state` |
| `github.rs` | GitHub integration | `check_label_exists`, `reset_github_labels` |
| `project.rs` | Project creation | `create_local_project` |
| `daemon.rs` | Daemon health checks | `check_daemon_health` |
| `filesystem.rs` | File I/O operations | `read_text_file`, `write_file` |
| `system.rs` | System dependency checks | `check_system_dependencies` |
| `ui.rs` | UI events and triggers | `emit_event`, `trigger_start` |

### Steps

1. **Add command to appropriate module** (e.g., `src-tauri/src/commands/workspace.rs`):
   ```rust
   #[tauri::command]
   pub fn my_command(param: String) -> Result<ReturnType, String> {
       // Implementation
       Ok(result)
   }
   ```
   Note: Command functions must be `pub` to be re-exported.

2. **Re-export from `commands/mod.rs`**:
   ```rust
   // In src-tauri/src/commands/mod.rs
   pub mod workspace;

   // Re-export individual commands
   pub use workspace::my_command;
   ```
   Or use wildcard re-export if the module has many related commands:
   ```rust
   pub use workspace::*;
   ```

3. **Register in `src-tauri/src/main.rs` invoke_handler**:
   ```rust
   .invoke_handler(tauri::generate_handler![
       // Legacy commands
       get_cli_workspace,
       // System commands
       check_system_dependencies,
       // ... existing commands ...
       my_command,  // Add your command here
   ])
   ```

4. **Call from TypeScript**:
   ```typescript
   import { invoke } from '@tauri-apps/api/tauri';

   const result = await invoke<ReturnType>('my_command', { param: value });
   ```

5. **Add required APIs** to `src-tauri/tauri.conf.json` allowlist if needed
6. **Update Cargo.toml** if new Tauri features required

### Creating a New Command Module

If you need a new domain area not covered by existing modules:

1. Create `src-tauri/src/commands/my_module.rs`
2. Add `pub mod my_module;` to `src-tauri/src/commands/mod.rs`
3. Re-export commands: `pub use my_module::*;`
4. Import in `main.rs`: Commands are available via the wildcard import `use commands::*;`
