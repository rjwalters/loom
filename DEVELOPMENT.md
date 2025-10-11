# Development Guide

This guide covers the development workflow, tooling, and best practices for contributing to Loom.

## Prerequisites

- **Node.js** 20+ and npm
- **Rust** 1.60+ (stable)
- **tmux** (for terminal management)
- **Git**

## Getting Started

1. Clone the repository:
   ```bash
   git clone https://github.com/rjwalters/loom.git
   cd loom
   ```

2. Install dependencies:
   ```bash
   npm install
   ```

3. Run the development server:
   ```bash
   npm run tauri:dev
   ```

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

#### Frontend Linting and Formatting
```bash
# Check for linting issues
npm run lint

# Fix linting issues automatically
npm run lint:fix

# Check formatting (no changes)
npm run format

# Format code
npm run format:write
```

#### Backend Linting and Formatting
```bash
# Check Rust formatting
npm run format:rust

# Format Rust code
npm run format:rust:write

# Run clippy linter
npm run clippy

# Fix clippy issues automatically
npm run clippy:fix
```

#### Comprehensive Checks
```bash
# Run all checks (lint, format, compile, build)
npm run check:all

# Check workspace compilation
npm run check

# Check daemon compilation
npm run daemon:check
```

### Git Hooks

Pre-commit hooks are automatically set up via [husky](https://typicode.github.io/husky/) and [lint-staged](https://github.com/lint-staged/lint-staged).

When you commit:
1. **TypeScript/JavaScript files** are automatically formatted and linted with Biome
2. **Rust files** are automatically formatted with rustfmt and linted with clippy

If there are errors that can't be auto-fixed, the commit will be blocked.

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
   npm run check:all
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
npm run daemon:dev

# Build daemon for production
npm run daemon:build

# Check daemon compilation
npm run daemon:check
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
1. Try auto-fixing: `npm run lint:fix` (frontend) or `npm run clippy:fix` (Rust)
2. Check the error messages for manual fixes needed
3. If a rule is too strict, discuss with the team before disabling it

### Build Errors

If builds fail:
1. Clean and rebuild: `rm -rf node_modules dist target && npm install && npm run build`
2. Check that all dependencies are installed
3. Verify Rust version: `rustc --version` (should be 1.60+)
4. Verify Node version: `node --version` (should be 20+)

### Git Hook Issues

If pre-commit hooks fail:
1. Check the error output for specific issues
2. Try manual fixes: `npm run lint:fix && npm run format:rust:write`
3. If hooks are misconfigured, reinstall: `rm -rf .husky && npx husky init`

## Additional Resources

- [Tauri Documentation](https://tauri.app/)
- [Biome Documentation](https://biomejs.dev/)
- [Rust Book](https://doc.rust-lang.org/book/)
- [TypeScript Documentation](https://www.typescriptlang.org/docs/)
- [TailwindCSS Documentation](https://tailwindcss.com/docs)
