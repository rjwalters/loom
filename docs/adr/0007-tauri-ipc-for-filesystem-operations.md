# ADR-0007: Tauri IPC for Filesystem Operations

## Status

Accepted

## Context

Loom's frontend (web browser context) needs to perform filesystem operations:
- Validate git repository paths
- Read/write `.loom/config.json` and `.loom/state.json`
- List role files from `defaults/roles/` and `.loom/roles/`
- Expand tilde paths (`~/path` → `/Users/username/path`)

Web browsers have **no filesystem access** for security reasons. Tauri provides two options:
1. **Tauri FS API**: Limited, sandboxed filesystem access from frontend
2. **Tauri IPC Commands**: Rust backend commands invoked from frontend

## Decision

Use **Tauri IPC commands** (Rust backend) for all filesystem operations instead of the Tauri FS API.

**Commands implemented**:
- `validate_git_repo(path: String) -> Result<bool, String>`
- `list_role_files(workspacePath: String) -> Result<Vec<String>, String>`
- `read_role_file(workspacePath: String, filename: String) -> Result<String, String>`
- `read_role_metadata(workspacePath: String, filename: String) -> Result<Option<String>, String>`

**Frontend invocation pattern**:
```typescript
import { invoke } from '@tauri-apps/api/tauri';

const isValid = await invoke<boolean>('validate_git_repo', { path });
```

## Consequences

### Positive

- **Full filesystem access**: Rust backend has unrestricted filesystem access
- **Type-safe IPC**: Automatic serialization/deserialization with type checking
- **Better error handling**: Rust `Result` types map cleanly to TypeScript
- **Security**: Backend can validate paths before allowing access
- **Testable**: Backend commands can be unit tested independently
- **Performance**: Native Rust code faster than browser APIs

### Negative

- **Complexity**: Requires Rust code for simple file operations
- **Serialization overhead**: Data must be JSON serialized across IPC boundary
- **Async everywhere**: All filesystem operations become async
- **No streaming**: Large files must be read entirely into memory
- **More boilerplate**: Each operation requires Rust command + TS invocation

## Alternatives Considered

### 1. Tauri FS API (Frontend)

**Example**:
```typescript
import { readTextFile } from '@tauri-apps/api/fs';
const content = await readTextFile('.loom/config.json');
```

**Pros**:
- Simple API
- No Rust code required
- Built-in path resolution

**Rejected because**:
- Sandboxed (limited filesystem access)
- No custom validation logic
- Can't implement complex operations (e.g., recursive role file search)
- Error messages less informative

### 2. Node.js Backend

**Pros**:
- More familiar to web developers
- Rich ecosystem (`fs-extra`, `glob`, etc.)
- Easier to prototype

**Rejected because**:
- Contradicts Tauri architecture (Rust backend)
- Larger bundle size
- Slower than native Rust
- Related: ADR-0008 (chose Rust daemon)

### 3. WebView Filesystem Access APIs

**Pros**:
- Native browser APIs (File System Access API)
- No backend required

**Rejected because**:
- Not available in Tauri webview
- Limited browser support
- Security prompts on every access
- Can't bypass user confirmation

## Implementation Pattern

**1. Define Rust Command** (`src-tauri/src/main.rs`):
```rust
#[tauri::command]
fn validate_git_repo(path: String) -> Result<bool, String> {
    let path = shellexpand::tilde(&path).to_string();
    let repo_path = Path::new(&path);

    if !repo_path.exists() {
        return Err(format!("Path does not exist: {}", path));
    }

    if !repo_path.is_dir() {
        return Err(format!("Path is not a directory: {}", path));
    }

    let git_path = repo_path.join(".git");
    if !git_path.exists() {
        return Err(format!("Not a git repository: {}", path));
    }

    Ok(true)
}
```

**2. Register Command**:
```rust
tauri::Builder::default()
    .invoke_handler(tauri::generate_handler![
        validate_git_repo,
        list_role_files,
        read_role_file,
        read_role_metadata,
    ])
    .run(tauri::generate_context!())
```

**3. Call from Frontend**:
```typescript
try {
  const isValid = await invoke<boolean>('validate_git_repo', {
    path: userInputPath
  });

  if (isValid) {
    state.setWorkspace(userInputPath);
  }
} catch (error) {
  console.error('Validation failed:', error);
  showError(error.toString());
}
```

## Validation Strategy

Backend commands perform **defensive validation**:

1. **Path expansion**: `~/path` → `/Users/username/path`
2. **Existence check**: Path must exist on filesystem
3. **Type check**: Directory vs file validation
4. **Security check**: Prevent path traversal (`../../../etc/passwd`)
5. **Domain check**: Verify `.git` directory exists for repo validation

This validation **cannot be done in the frontend** due to browser sandbox restrictions.

## References

- Implementation: `src-tauri/src/main.rs` (command definitions)
- Usage: `src/main.ts` (workspace validation), `src/lib/terminal-settings-modal.ts` (role files)
- Related: ADR-0003 (Config/State file split)
- Tauri IPC: https://tauri.app/v1/guides/features/command
- Tauri FS API: https://tauri.app/v1/api/js/fs
