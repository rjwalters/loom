# ADR-0008: tmux + Rust Daemon Architecture

## Status

Accepted

## Context

Loom needs to manage multiple persistent terminal sessions that:
- Survive app restarts
- Run background processes (Claude Code agents)
- Support input/output streams
- Work with git worktrees
- Integrate with macOS desktop app

Several architectural approaches were considered for terminal management:

## Decision

Use a **two-tier architecture**:

1. **Tauri Frontend** (TypeScript): UI, state management, user interaction
2. **Rust Daemon** (`loom-daemon`): Terminal lifecycle management
3. **tmux**: Persistent terminal multiplexer

**Architecture**:
```
Tauri App (UI)
    ↕ IPC (JSON over Unix socket)
Rust Daemon (loom-daemon)
    ↕ Commands
tmux Sessions (loom-terminal-1, loom-terminal-2, ...)
    ↕ stdin/stdout
Terminal Processes (bash, Claude Code)
```

**Key Components**:

**1. Rust Daemon** (`loom-daemon/src/`):
- Listens on Unix socket: `~/.loom/loom.sock`
- Manages tmux session lifecycle
- Handles IPC requests (create, destroy, send input, get output)
- Auto-cleanup: Removes worktrees when terminals destroyed

**2. tmux Sessions**:
- Socket: `-L loom` (separate from user's default tmux)
- Naming: `loom-terminal-{id}` (e.g., `loom-terminal-1`)
- Persistent: Survive app restarts
- Output capture: `pipe-pane -o` to `/tmp/loom-terminal-{id}.out`

**3. IPC Protocol** (JSON, internally-tagged):
```json
{"type": "CreateTerminal", "payload": {"id": "terminal-1", "working_directory": "/path"}}
{"type": "DestroyTerminal", "payload": {"id": "terminal-1"}}
{"type": "SendInput", "payload": {"id": "terminal-1", "input": "echo hello\n"}}
```

## Consequences

### Positive

- **Persistence**: tmux sessions survive app crashes/restarts
- **Isolation**: Separate tmux socket prevents interference with user's tmux
- **Performance**: Native Rust daemon is fast and efficient
- **Simplicity**: tmux handles terminal emulation complexity
- **Debugging**: Can attach to sessions with `tmux -L loom attach -t loom-terminal-1`
- **Cleanup**: Daemon auto-removes worktrees on terminal destruction
- **Testable**: Daemon has comprehensive integration tests (Issue #13)

### Negative

- **Complexity**: Multi-process architecture (app + daemon + tmux)
- **Dependencies**: Requires tmux installed on system
- **Platform-specific**: tmux not natively available on Windows
- **IPC overhead**: JSON serialization for all commands
- **Process management**: Must handle daemon lifecycle (start, stop, crash recovery)

## Alternatives Considered

### 1. Embedded Terminal Emulator (conpty, pty.js)

**Pros**:
- Full control over terminal emulation
- No tmux dependency
- Cross-platform (Windows support)

**Rejected because**:
- Complex to implement correctly (ANSI codes, window resizing, etc.)
- No persistence (process dies with app)
- Would need to reimplement tmux features
- More code to maintain and debug

### 2. Node.js Backend

**Pros**:
- More familiar to web developers
- Rich ecosystem (`node-pty`, `xterm.js`)
- Easier to prototype

**Rejected because**:
- Slower than Rust
- Larger memory footprint
- JavaScript async model more complex for IPC
- Contradicts Tauri philosophy (Rust backend)

### 3. Docker Containers

**Pros**:
- Strong isolation
- Reproducible environments
- Easy cleanup

**Rejected because**:
- Heavy overhead (Docker daemon, images)
- Slower startup times
- Overkill for terminal management
- Requires Docker installation

### 4. Built-in Tauri Shell

**Pros**:
- Simplest approach
- Built into Tauri

**Rejected because**:
- No persistence
- Limited control
- Can't survive app restarts
- No multiplexing

## Implementation Details

**Daemon Lifecycle** (`loom-daemon/src/main.rs`):
```rust
#[tokio::main]
async fn main() -> Result<()> {
    let socket_path = dirs::home_dir()
        .unwrap()
        .join(".loom/loom.sock");

    let listener = UnixListener::bind(&socket_path)?;

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(handle_client(stream));
    }
}
```

**Terminal Creation** (`loom-daemon/src/terminal.rs`):
```rust
pub fn create_terminal(id: &str, working_directory: Option<String>) -> Result<()> {
    let session_name = format!("loom-{}", id);
    let output_file = format!("/tmp/loom-{}.out", id);

    // Create tmux session
    Command::new("tmux")
        .args(&["-L", "loom", "new-session", "-d", "-s", &session_name])
        .output()?;

    // Set working directory
    if let Some(dir) = working_directory {
        Command::new("tmux")
            .args(&["-L", "loom", "send-keys", "-t", &session_name,
                   &format!("cd '{}'", dir), "C-m"])
            .output()?;
    }

    // Capture output to file
    Command::new("tmux")
        .args(&["-L", "loom", "pipe-pane", "-t", &session_name,
               "-o", &format!("cat > {}", output_file)])
        .output()?;

    Ok(())
}
```

**Worktree Auto-Cleanup** (`loom-daemon/src/terminal.rs:87-102`):
```rust
pub fn destroy_terminal(id: &str) -> Result<()> {
    let session_name = format!("loom-{}", id);

    // Check if terminal is in a Loom worktree
    if let Some(working_dir) = get_working_directory(&session_name)? {
        if working_dir.contains("/.loom/worktrees/") {
            // Remove git worktree
            Command::new("git")
                .args(&["worktree", "remove", &working_dir, "--force"])
                .output()
                .ok();
        }
    }

    // Kill tmux session
    Command::new("tmux")
        .args(&["-L", "loom", "kill-session", "-t", &session_name])
        .output()?;

    Ok(())
}
```

## Testing

Comprehensive integration tests (`loom-daemon/tests/integration_basic.rs`):
- Basic IPC (Ping/Pong)
- Terminal lifecycle (create, list, destroy)
- Working directory support
- Input handling
- Multiple concurrent clients
- Error conditions

Run with:
```bash
pnpm daemon:test
```

## References

- Implementation: `loom-daemon/src/` (Rust daemon)
- Related: ADR-0004 (Worktree paths)
- Related: ADR-0006 (Label-based workflow)
- Related: Issue #3 (Daemon architecture)
- Related: Issue #13 (Daemon integration tests)
- tmux manual: https://www.man7.org/linux/man-pages/man1/tmux.1.html
- Unix domain sockets: https://en.wikipedia.org/wiki/Unix_domain_socket
