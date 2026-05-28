# Loom Development Guide

This guide describes how to set up the Loom source repo for development and contribute changes.

## Prerequisites

- **Rust** (stable channel)
- **Node.js** (v18+) and **pnpm** (for `mcp-loom`)
- **Python 3.10+** with **uv** (for `loom-tools`)
- **tmux**
- A POSIX shell (bash/zsh)

## Quick Start

1. Clone the repo:
   ```bash
   git clone https://github.com/rjwalters/loom
   cd loom
   ```

2. Build the daemon:
   ```bash
   cargo build --workspace --release
   ```

3. Run the daemon in dev mode:
   ```bash
   ./scripts/dev-daemon.sh
   ```

   For detailed daemon workflow information, see [dev-workflow.md](dev-workflow.md).

## Code Quality Tools

### Linting and Formatting

Loom uses a comprehensive set of tools to maintain code quality:

#### Rust
- **[rustfmt](https://rust-lang.github.io/rustfmt/)** - Official Rust formatter
- **[clippy](https://github.com/rust-lang/rust-clippy)** - Official Rust linter
- Configuration: `rustfmt.toml` and `.cargo/config.toml`

#### TypeScript (mcp-loom)
- `tsc --noEmit` for typecheck
- `esbuild` for bundling

#### Python (loom-tools)
- `pytest` for tests; `ruff` / `mypy` if configured per package

### Available Commands

#### Daemon Management
```bash
# Start daemon in interactive monitor (recommended)
pnpm run daemon:dev

# Run daemon in foreground (cargo run)
pnpm run daemon:preview

# Start in background
pnpm run daemon:headless

# Stop
pnpm run daemon:stop

# Build release binary
pnpm run daemon:build
```

For detailed workflow information, see [dev-workflow.md](dev-workflow.md).

#### Rust Linting and Formatting
```bash
# Check formatting
pnpm run format:rust

# Apply formatting
pnpm run format:rust:write

# Run clippy
pnpm run clippy

# Auto-fix clippy issues
pnpm run clippy:fix
```

#### Combined Checks

```bash
# Run everything locally (matches CI)
pnpm run check:all

# CI variant (with --locked for reproducible builds)
pnpm run check:ci
```

## Continuous Integration

GitHub Actions runs the following on every push and PR:

- **Rust Formatting** - rustfmt checks
- **Rust Linting** - clippy checks with warnings as errors
- **Rust Build** - Cargo workspace check
- **mcp-loom Build** - TypeScript compile + bundle
- **Installer Integration Tests** - macOS installer smoke tests

See `.github/workflows/ci.yml` for the full CI configuration.

## Project Structure

```
loom/
├── loom-daemon/           # Rust daemon (orchestration, IPC, tmux mgmt)
│   └── src/
│       ├── main.rs        # Daemon entry point
│       ├── ipc.rs         # Unix socket server
│       └── ...
├── loom-api/              # Shared Rust types and IPC protocol
├── mcp-loom/              # Unified MCP server (TypeScript/Node)
├── loom-tools/            # Python tools (loom-shepherd, loom-clean, etc.)
├── defaults/              # Default configuration templates installed into target repos
├── scripts/               # Installation, daemon, and maintenance scripts
├── .github/workflows/     # GitHub Actions CI
├── .vscode/               # VSCode settings and extensions
└── rustfmt.toml           # Rustfmt configuration
```

## Development Workflow

### Making Changes

1. Create a feature branch:
   ```bash
   git checkout -b feature/issue-N
   ```

2. Make your changes

3. Run checks locally:
   ```bash
   pnpm run check:all
   ```

4. Commit your changes (pre-commit hooks will run automatically)

5. Push and create a PR with `loom:review-requested`:
   ```bash
   git push -u origin feature/issue-N
   gh pr create --label loom:review-requested --body "Closes #N"
   ```

### Working with the Daemon

The daemon manages tmux terminal sessions and orchestrates support roles.

```bash
# Run daemon in development mode (with logging)
pnpm run daemon:dev

# Build daemon for production
pnpm run daemon:build
```

## VSCode Setup

Install the recommended extensions when prompted. The workspace includes:

- **rust-analyzer** - Rust language server

Settings in `.vscode/settings.json` enable format-on-save.

## Troubleshooting

### Linting Errors

If you see linting errors:
1. Try auto-fixing: `pnpm run clippy:fix`
2. Check the error messages for manual fixes needed
3. If a rule is too strict, discuss with the team before disabling it

### Build Errors

If builds fail:
1. Clean and rebuild: `rm -rf target && cargo build --workspace`
2. Check that all dependencies are installed
3. Verify Rust version: `rustc --version` (should be 1.70+)

### Git Hook Issues

If pre-commit hooks fail:
1. Check the error output for specific issues
2. Try manual fixes: `pnpm run format:rust:write && pnpm run clippy:fix`
3. If hooks are misconfigured, reconfigure: `git config core.hooksPath .githooks`

## Additional Resources

- [Rust Book](https://doc.rust-lang.org/book/)
- [tmux Manual](https://www.man7.org/linux/man-pages/man1/tmux.1.html)
