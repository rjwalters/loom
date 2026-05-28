# Code Quality Tools (Issue #8)

## Linting & Formatting Setup

**Frontend (Biome)**:
- Fast, comprehensive linter and formatter for TypeScript/JavaScript
- Configured in `biome.json` with schema version 2.2.5
- VCS integration enabled for git-aware linting
- Rules: Recommended + custom overrides for project style
- Commands: `npm run lint`, `npm run format`

**Backend (rustfmt + clippy)**:
- `rustfmt.toml`: Format configuration (100 char width, 4 space indent)
- `.cargo/config.toml`: Clippy lint levels
  - Deny: all, correctness, suspicious, complexity
  - Warn: pedantic, perf, style, unwrap_used, expect_used
- Commands: `npm run format:rust`, `npm run clippy`

**Git Hooks (.githooks/pre-commit)**:
- Pre-commit hook auto-formats staged files
- TS/JS files: Biome formatting + linting
- Rust files: rustfmt formatting
- Configured in `.githooks/pre-commit` (plain shell script, zero npm dependencies)

**CI/CD (GitHub Actions)**:
- Workflow: `.github/workflows/ci.yml`
- Jobs run in parallel: rust format, rust clippy, builds, mcp-loom build, installer tests
- All warnings treated as errors (`-D warnings` for clippy)
- Dependency caching for faster builds

**VSCode Integration**:
- Settings: `.vscode/settings.json`
- Extensions: `.vscode/extensions.json`
- Format on save enabled for all languages
- rust-analyzer for Rust

## Development Workflow

1. **Make changes** - Edit code with format-on-save
2. **Pre-commit hook** - Auto-formats on commit
3. **Push** - Triggers CI checks
4. **CI validates** - All linting/formatting/builds must pass
5. **Manual check** - Run `npm run check:all` to verify locally

## IMPORTANT: Always Use pnpm Scripts for CI Matching

**Always use pnpm scripts** defined in `package.json` instead of running cargo/biome commands directly. This ensures your local checks match CI exactly.

**Available Scripts**:
```bash
pnpm format:rust       # Rust formatting check
pnpm format:rust:write # Rust formatting fix
pnpm clippy            # Clippy with exact CI flags
pnpm clippy:fix        # Clippy auto-fix
pnpm check             # Cargo check
pnpm check:all         # Run everything (full CI simulation)
```

**Why This Matters**:
- CI uses: `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- Direct `cargo clippy` might miss flags like `--all-targets` or `--all-features`
- pnpm scripts guarantee the exact same command CI runs
- Prevents "passes locally but fails in CI" issues

**Before Opening a PR**:
```bash
pnpm check:all  # This runs the full CI suite locally
```

If this passes, CI should pass too.

**Package Manager Preference**: Always use `pnpm` (not `npm`) as the package manager for this project.

## Clippy Configuration Details

The `.cargo/config.toml` enforces strict linting:

```toml
rustflags = [
    "-D", "clippy::all",           # Deny all warnings
    "-D", "clippy::correctness",   # Deny correctness issues
    "-D", "clippy::suspicious",    # Deny suspicious patterns
    "-D", "clippy::complexity",    # Deny unnecessary complexity
    "-W", "clippy::pedantic",      # Warn on pedantic issues
    "-W", "clippy::unwrap_used",   # Warn on .unwrap()
    "-W", "clippy::expect_used",   # Warn on .expect()
]
```

**When to use `#[allow(clippy::expect_used)]`**:
- Mutex locks (poisoning is panic-level, not recoverable)
- Main function startup (binary cannot continue without a usable runtime)
- Other truly exceptional scenarios

**Handling expect/unwrap warnings**:
- Prefer proper error handling with `Result` and `?` operator
- Use `expect()` with descriptive messages only when panic is acceptable
- Add `#[allow]` attribute with explanatory comment when necessary

## Self-Modification Workflow

When agents modify the Loom codebase itself, the daemon binary continues running with the old image — file changes do not auto-restart it. To pick up daemon changes, restart the daemon explicitly:

```bash
pnpm daemon:stop
pnpm daemon:build
pnpm daemon:dev
```

For changes to `mcp-loom`, rebuild and restart any Claude Code session using it:

```bash
cd mcp-loom && npm run build
```

For shell scripts, slash commands, role definitions, and Python tools, changes take effect on the next invocation — no restart required.
