# Development Guide

This guide covers the development workflow, tooling, and best practices for contributing to Loom.

## Prerequisites

- **Node.js** 20+ and pnpm
- **Rust** 1.60+ (stable)
- **tmux** (for terminal management)
- **Git**

## Documentation Overview

This guide covers code quality, tooling, and development practices. For:
- **Day-to-day development workflow**: See [DEV_WORKFLOW.md](dev-workflow.md)
- **Project vision and architecture**: See [README.md](../../README.md)
- **Agent workflows**: See [WORKFLOWS.md](../workflows.md)

## Getting Started

1. Clone the repository:
   ```bash
   git clone https://github.com/rjwalters/loom.git
   cd loom
   ```

2. Install dependencies:
   ```bash
   pnpm install
   ```

3. Run the development environment:
   ```bash
   pnpm run app:dev
   ```

   This starts both the daemon and Tauri dev server in one command. For detailed workflow information, see [DEV_WORKFLOW.md](dev-workflow.md).

## Code Quality Tools

### Linting and Formatting

Loom uses a comprehensive set of tools to maintain code quality:

#### Frontend (TypeScript/JavaScript)
- **[Biome](https://biomejs.dev/)** - Fast formatter and linter for TypeScript/JavaScript
- Configuration: `biome.json`

#### Backend (Rust)
- **[rustfmt](https://rust-lang.github.io/rustfmt/)** - Official Rust formatter
- **[clippy](https://github.com/rust-lang/rust-clippy)** - Official Rust linter
- Configuration: `rustfmt.toml` and `.cargo/config.toml`

### Available Commands

#### Application Development Commands
```bash
# Start daemon + Tauri dev in one command
pnpm run app:dev

# Restart daemon when it gets into bad state
pnpm run app:dev:restart

# Stop the background daemon
pnpm run app:stop
```

#### Daemon Management
```bash
# Start daemon in background
pnpm run daemon:start

# Stop daemon
pnpm run daemon:stop

# Restart daemon
pnpm run daemon:restart

# Run daemon in foreground (for debugging)
pnpm run daemon:dev
```

For detailed workflow information, see [DEV_WORKFLOW.md](dev-workflow.md).

#### Frontend Linting and Formatting
```bash
# Check for linting issues
pnpm run lint

# Fix linting issues automatically
pnpm run lint:fix

# Check formatting (no changes)
pnpm run format

# Format code
pnpm run format:write
```

#### Backend Linting and Formatting
```bash
# Check Rust formatting
pnpm run format:rust

# Format Rust code
pnpm run format:rust:write

# Run clippy linter
pnpm run clippy

# Fix clippy issues automatically
pnpm run clippy:fix
```

#### Comprehensive Checks
```bash
# Run all checks (lint, format, compile, build)
pnpm run check:all

# Check workspace compilation
pnpm run check

# Check daemon compilation
pnpm run daemon:check
```

### Git Hooks

Pre-commit hooks are automatically set up via [husky](https://typicode.github.io/husky/) and [lint-staged](https://github.com/lint-staged/lint-staged).

When you commit:
1. **TypeScript/JavaScript files** are automatically formatted and linted with Biome
2. **Rust files** are automatically formatted with rustfmt and linted with clippy

If there are errors that can't be auto-fixed, the commit will be blocked.

### Testing

Loom has comprehensive testing at multiple levels:

#### Unit Tests
```bash
# Run all workspace tests
cargo test --workspace

# Run with verbose output
cargo test --workspace -- --nocapture
```

#### Integration Tests (Daemon)
```bash
# Run daemon integration tests
pnpm run daemon:test

# Run with verbose output
pnpm run daemon:test:verbose

# Run specific test
cargo test --test integration_basic test_ping_pong -- --nocapture
```

#### Script Integration Tests
```bash
# Test daemon management scripts
pnpm run daemon:test:scripts
```

**Requirements**: Tests require `tmux` installed (`brew install tmux` on macOS)

See [scripts/README.md](scripts/README.md) for details on daemon management script testing.

### CI/CD

GitHub Actions runs the following checks on every PR and push to `main`:

- **Frontend Linting** - Biome checks
- **Frontend Formatting** - Biome format checks
- **Frontend Build** - TypeScript compilation and Vite build
- **Rust Formatting** - rustfmt checks
- **Rust Linting** - clippy checks with warnings as errors
- **Rust Build** - Cargo workspace check

See `.github/workflows/ci.yml` for the full CI configuration.

## Project Structure

```
loom/
├── src/                    # Frontend TypeScript source
│   ├── lib/               # Reusable modules (state, config, ui, theme)
│   ├── main.ts            # Application entry point
│   └── style.css          # TailwindCSS styles
├── src-tauri/             # Tauri Rust backend
│   └── src/
│       ├── main.rs        # Tauri commands and application
│       └── daemon_client.rs  # Daemon IPC client
├── loom-daemon/           # Standalone Rust daemon
│   └── src/
│       ├── main.rs        # Daemon entry point
│       ├── types.rs       # Shared types for IPC
│       ├── terminal.rs    # tmux terminal management
│       └── ipc.rs         # Unix socket server
├── .github/workflows/     # GitHub Actions CI
├── .vscode/               # VSCode settings and extensions
├── biome.json             # Biome configuration
├── rustfmt.toml           # Rustfmt configuration
└── .cargo/config.toml     # Cargo and clippy configuration
```

## Development Workflow

### Making Changes

1. Create a feature branch:
   ```bash
   git checkout -b feature/your-feature-name
   ```

2. Make your changes

3. Run checks locally:
   ```bash
   pnpm run check:all
   ```

4. Commit your changes (pre-commit hooks will run automatically)

5. Push and create a PR:
   ```bash
   git push -u origin feature/your-feature-name
   ```

### Working with the Daemon

The daemon manages tmux terminal sessions independently of the GUI.

```bash
# Run daemon in development mode (with logging)
pnpm run daemon:dev

# Build daemon for production
pnpm run daemon:build

# Check daemon compilation
pnpm run daemon:check
```

## VSCode Setup

Install the recommended extensions when prompted. The workspace includes:

- **rust-analyzer** - Rust language server
- **Biome** - TypeScript/JavaScript formatter and linter
- **Tauri** - Tauri development tools

Settings in `.vscode/settings.json` enable format-on-save for all languages.

## Troubleshooting

### Linting Errors

If you see linting errors:
1. Try auto-fixing: `pnpm run lint:fix` (frontend) or `pnpm run clippy:fix` (Rust)
2. Check the error messages for manual fixes needed
3. If a rule is too strict, discuss with the team before disabling it

### Build Errors

If builds fail:
1. Clean and rebuild: `rm -rf node_modules dist target && pnpm install && pnpm run build`
2. Check that all dependencies are installed
3. Verify Rust version: `rustc --version` (should be 1.60+)
4. Verify Node version: `node --version` (should be 20+)

### Git Hook Issues

If pre-commit hooks fail:
1. Check the error output for specific issues
2. Try manual fixes: `pnpm run lint:fix && pnpm run format:rust:write`
3. If hooks are misconfigured, reinstall: `rm -rf .husky && npx husky init`

## Additional Resources

- [Tauri Documentation](https://tauri.app/)
- [Biome Documentation](https://biomejs.dev/)
- [Rust Book](https://doc.rust-lang.org/book/)
- [TypeScript Documentation](https://www.typescriptlang.org/docs/)
- [TailwindCSS Documentation](https://tailwindcss.com/docs)
