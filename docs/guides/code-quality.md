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
- Jobs run in parallel: frontend lint/format, rust format, rust clippy, builds
- All warnings treated as errors (`-D warnings` for clippy)
- Dependency caching for faster builds
- Frontend build artifacts downloaded before Tauri compilation

**VSCode Integration**:
- Settings: `.vscode/settings.json`
- Extensions: `.vscode/extensions.json`
- Format on save enabled for all languages
- Biome for TS/JS, rust-analyzer for Rust

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
pnpm lint              # Biome linting
pnpm format            # Biome formatting
pnpm format:rust       # Rust formatting check
pnpm format:rust:write # Rust formatting fix
pnpm clippy            # Clippy with exact CI flags
pnpm clippy:fix        # Clippy auto-fix
pnpm check             # Cargo check
pnpm build             # TypeScript + Vite build
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

## Development Workflow

Use the appropriate script based on your scenario:

- **`pnpm app:dev`**: Normal development with hot reload (fastest iteration)
  - Use when: Making frequent frontend changes
  - Caveat: Hot reload sometimes misses changes (see "Stale Code Issue" below)

- **`pnpm app:preview`**: Complete rebuild + launch (recommended for testing)
  - Use when: After pulling new code, switching branches, or hot reload misses changes
  - Always rebuilds both frontend AND Tauri binary before launching
  - This is the "safe" option that guarantees fresh code

- **`pnpm app:build`**: Production build
  - Use when: Creating release builds

**Stale Code Issue**: If you pull new code or switch branches, run `pnpm app:preview` to ensure you're running the latest code. The `tauri dev` command caches the built frontend and hot reload doesn't always catch everything, leading to wasted debugging time.

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
- Main function startup (Tauri failure is fatal)
- Other truly exceptional scenarios

**Handling expect/unwrap warnings**:
- Prefer proper error handling with `Result` and `?` operator
- Use `expect()` with descriptive messages only when panic is acceptable
- Add `#[allow]` attribute with explanatory comment when necessary

## Self-Modification Problem

**CRITICAL**: Loom cannot develop itself using `app:dev` mode due to hot reload causing restart loops.

### The Problem

When running Loom in development mode (`pnpm app:dev`), Vite watches for file changes and triggers hot module replacement (HMR). If agent terminals within Loom are working on the Loom codebase itself:

1. Agent edits source file (e.g., `src/lib/workspace-reset.ts`)
2. Vite detects change and triggers HMR
3. Tauri reloads the app
4. App restart interrupts the agent mid-work
5. Agent continues, edits another file
6. **Infinite restart loop**

This makes it impossible for Loom to work on its own codebase in dev mode.

### Solutions

**Option 1: Use Preview Mode (Recommended)**
```bash
pnpm app:preview
```
- Builds the app once, then runs without hot reload
- Agents can edit files without triggering restarts
- Still faster than full production builds
- Requires rebuild to see UI changes

**Option 2: Use Production Mode**
```bash
pnpm app:build
# Then run the built app from ./target/release/
```
- Fully optimized production build
- No hot reload at all
- Slowest rebuild cycle

**Option 3: Work on Different Workspace**
```bash
# Clone Loom to a separate directory
git clone https://github.com/your-username/loom ~/loom-dev
cd ~/loom-dev
pnpm app:preview

# Point agent terminals at original workspace
# Agents work in ~/GitHub/loom, app runs from ~/loom-dev
```
- Separates running app from workspace being edited
- Best for testing factory reset and agent features
- Requires keeping both repos in sync

**Option 4: Disable Agent Terminals**
```bash
# Use app:dev but don't run any agent terminals
pnpm app:dev
# Keep terminals as plain shells or run agents on different repos
```
- Good for frontend development only
- Can't test agent orchestration features

### When to Use Each Mode

**Use `app:dev`**:
- Frontend-only development (CSS, UI components, layouts)
- No agent terminals running
- Working on a different repository with agent terminals

**Use `app:preview`**:
- Testing factory reset, agent launching, worktree management
- Agent terminals working on Loom codebase
- Integration testing with agents

**Use `app:build`**:
- Final testing before release
- Performance profiling
- Packaging for distribution

### Vite Ignore Configuration

Vite is already configured to ignore `.loom/**` directories:

```typescript
// vite.config.ts
export default defineConfig({
  server: {
    watch: {
      ignored: ["**/.loom/**"],  // Ignores .loom/worktrees/, .loom/config.json, etc.
    },
  },
});
```

However, this only prevents changes to `.loom/` from triggering reloads. Changes to `src/**`, `loom-daemon/**`, or other source files will still trigger HMR.

### Summary

**DO NOT use `app:dev` when agent terminals are working on the Loom codebase.** Always use `app:preview` or `app:build` for self-modification scenarios.
