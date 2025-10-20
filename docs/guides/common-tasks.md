# Common Tasks

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
