# Loom API Reference

This document provides a comprehensive reference for Loom's APIs, including daemon CLI commands, daemon IPC protocol, and MCP server APIs.

## Table of Contents

- [Daemon CLI Commands](#daemon-cli-commands)
- [Daemon IPC Protocol](#daemon-ipc-protocol)
- [MCP Server APIs](#mcp-server-apis)

## Daemon CLI Commands

The `loom-daemon` binary provides command-line interface for headless operations.

### `loom-daemon init`

Initialize a Loom workspace in a git repository.

**Synopsis:**
```bash
loom-daemon init [OPTIONS] [PATH]
```

**Arguments:**

| Argument | Type | Description | Default |
|----------|------|-------------|---------|
| `PATH` | String | Target directory to initialize | Current directory (`.`) |

**Options:**

| Flag | Type | Description |
|------|------|-------------|
| `--force` | Boolean | Overwrite existing `.loom` directory |
| `--dry-run` | Boolean | Preview changes without applying |
| `--defaults <PATH>` | String | Custom defaults directory path |

**Description:**

Initializes a Loom workspace by:
1. Validating the target is a git repository
2. Copying `.loom/` configuration from defaults
3. Installing repository scaffolding (CLAUDE.md, .claude/, .github/)
4. Updating `.gitignore` with Loom ephemeral patterns

**Examples:**

```bash
# Initialize current directory
loom-daemon init

# Initialize specific repository
loom-daemon init /path/to/repo

# Preview changes
loom-daemon init --dry-run

# Force reinitialization
loom-daemon init --force

# Use custom defaults
loom-daemon init --defaults ./org-defaults

# Combine flags
loom-daemon init --force --defaults ./custom /path/to/repo
```

**Exit Codes:**

| Code | Meaning |
|------|---------|
| `0` | Success - workspace initialized |
| `1` | Error - see stderr for details |

**Common Errors:**

| Error Message | Cause | Solution |
|---------------|-------|----------|
| "Not a git repository (no .git directory found)" | Target lacks `.git` | Run `git init` first |
| "Workspace already initialized (.loom directory exists)" | `.loom/` exists | Use `--force` or skip if already set up |
| "Failed to create .loom directory: Permission denied" | Insufficient permissions | Check ownership with `ls -la` |
| "Defaults directory not found" | Cannot locate defaults | Specify with `--defaults /path` |

**Implementation:**

- **File:** `loom-daemon/src/init.rs`
- **Function:** `initialize_workspace(workspace_path, defaults_path, force)`
- **CLI Parser:** `loom-daemon/src/main.rs` (Clap argument parsing)

**See Also:**
- [CLI Reference Guide](../guides/cli-reference.md) - Complete command documentation
- [Getting Started Guide](../guides/getting-started.md) - Installation walkthrough
- [CI/CD Setup Guide](../guides/ci-cd-setup.md) - Pipeline integration

## Daemon IPC Protocol

The Loom daemon uses a Unix socket at `~/.loom/loom-daemon.sock` with JSON messages.

### Message Format

All messages are JSON objects with a `type` field:

```json
{
  "type": "MessageType",
  "payload": { ... }
}
```

### Request Messages

#### CreateTerminal

Creates a new terminal session.

```json
{
  "type": "CreateTerminal",
  "payload": {
    "id": "terminal-1",
    "working_directory": "/Users/username/projects/my-repo"
  }
}
```

**Response:**
```json
{
  "type": "TerminalCreated",
  "payload": {
    "id": "terminal-1",
    "tmux_session": "loom-terminal-1"
  }
}
```

#### SendInput

Sends input to a terminal.

```json
{
  "type": "SendInput",
  "payload": {
    "id": "terminal-1",
    "input": "ls -la\n"
  }
}
```

**Response:**
```json
{
  "type": "InputSent",
  "payload": {
    "id": "terminal-1"
  }
}
```

#### ReadOutput

Reads terminal output.

```json
{
  "type": "ReadOutput",
  "payload": {
    "id": "terminal-1"
  }
}
```

**Response:**
```json
{
  "type": "Output",
  "payload": {
    "id": "terminal-1",
    "output": "file1\nfile2\n"
  }
}
```

#### KillTerminal

Terminates a terminal session.

```json
{
  "type": "KillTerminal",
  "payload": {
    "id": "terminal-1"
  }
}
```

**Response:**
```json
{
  "type": "TerminalKilled",
  "payload": {
    "id": "terminal-1"
  }
}
```

#### ListTerminals

Lists all active terminals.

```json
{
  "type": "ListTerminals"
}
```

**Response:**
```json
{
  "type": "TerminalList",
  "payload": {
    "terminals": [
      {
        "id": "terminal-1",
        "tmux_session": "loom-terminal-1",
        "working_directory": "/path/to/workspace"
      }
    ]
  }
}
```

#### Ping

Health check.

```json
{
  "type": "Ping"
}
```

**Response:**
```json
{
  "type": "Pong"
}
```

### Error Responses

```json
{
  "type": "Error",
  "payload": {
    "message": "Terminal not found: terminal-99"
  }
}
```

## MCP Server APIs

Loom provides a unified MCP server (`mcp-loom`) for AI-powered testing and debugging. See [MCP Documentation](../mcp/README.md) for complete API reference.

### Quick Reference

**UI Tools:**
- `read_state_file` - Application state
- `read_config_file` - Terminal configurations
- `trigger_force_start` - Start engine
- `trigger_factory_reset` - Reset to defaults
- `get_heartbeat` - Health status

**Log Tools:**
- `tail_daemon_log` - Daemon logs
- `list_terminal_logs` - List terminal logs
- `tail_terminal_log` - Terminal-specific logs

**Terminal Tools:**
- `list_terminals` - All terminals
- `get_selected_terminal` - Primary terminal
- `get_terminal_output` - Terminal output
- `send_terminal_input` - Send input

See the [MCP Documentation](../mcp/) for detailed usage examples.

## Configuration File Formats

### .loom/config.json

Workspace configuration persisted across app restarts.

```json
{
  "nextAgentNumber": 4,
  "agents": [
    {
      "id": "terminal-1",
      "name": "Worker 1",
      "status": "idle",
      "isPrimary": true,
      "role": "builder",
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

### .loom/state.json

Runtime state (not persisted to config).

```json
{
  "terminals": {
    "terminal-1": {
      "id": "terminal-1",
      "tmuxSession": "loom-terminal-1",
      "workingDirectory": "/path/to/workspace",
      "health": "healthy",
      "lastHealthCheck": 1234567890
    }
  }
}
```

### Role Metadata (.json)

Optional metadata for role files.

```json
{
  "name": "Worker Bot",
  "description": "General development worker",
  "defaultInterval": 0,
  "defaultIntervalPrompt": "Continue working on open tasks",
  "autonomousRecommended": false,
  "suggestedWorkerType": "claude"
}
```

## See Also

- [Testing Guide](../guides/testing.md) - MCP testing and debugging
- [Code Quality Guide](../guides/code-quality.md) - Development workflows
