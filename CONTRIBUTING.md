# Contributing to Loom

Thank you for your interest in contributing to Loom!

## About Loom Development

Loom is developed **using Loom itself** - our AI agents (Worker, Curator, Reviewer, Architect, Critic) collaborate through GitHub to build and improve the project. This means:

- **Code development** is handled by autonomous AI agents
- **External contributions** are primarily limited to suggestions and issue reports
- **All pull requests** are created and reviewed by the agent system

## How External Contributors Can Help

While direct code contributions follow a unique workflow, external contributors can still make valuable contributions:

### 1. Report Bugs

If you find a bug:
- Search existing issues to avoid duplicates
- Create a new issue with a clear, descriptive title
- Include steps to reproduce, expected vs actual behavior
- Add system information (OS, Loom version)
- Label with `bug` (if you have permissions)

The Architect agent will review bug reports and may create proposals for fixes.

### 2. Suggest Features

Have an idea for improving Loom?
- Search existing issues to see if it's been suggested
- Create a new issue with a detailed description
- Explain the use case and expected benefits
- Label with `enhancement` (if you have permissions)

The Architect agent periodically scans issues and creates `loom:proposal` issues for features it deems valuable. The maintainers review these proposals and approve by removing the `loom:proposal` label.

### 3. Improve Documentation

Documentation improvements are always welcome:
- Fix typos or unclear explanations
- Add examples or clarifications
- Update outdated information
- Expand guides based on your experience

Documentation changes follow the same PR process (see below).

### 4. Participate in Discussions

- Comment on issues with insights or additional context
- Help other users in discussions
- Share your experience using Loom
- Suggest workflow improvements

## Development Setup

If you want to explore the codebase or test changes locally:

### Prerequisites

- **Node.js** 20+ and pnpm
- **Rust** 1.60+ (stable)
- **tmux** (for terminal management)
- **Git**
- **GitHub CLI** (`gh`) for issue/PR management

### Installation

```bash
# Clone the repository
git clone https://github.com/rjwalters/loom.git
cd loom

# Install dependencies
pnpm install

# Start development environment
pnpm app:dev
```

### Alternative: Headless Installation

You can also initialize Loom in a repository using the CLI without the GUI:

```bash
# Build the daemon
cd loom
cargo build --release -p loom-daemon

# Initialize a repository
./target/release/loom-daemon init /path/to/repo

# Or initialize the Loom repo itself
./target/release/loom-daemon init .
```

This is useful for:
- Testing initialization logic
- Setting up Loom in CI/CD environments
- Bulk repository initialization
- Development without running the full GUI

**See also:**
- [Getting Started Guide](docs/guides/getting-started.md) - Complete setup walkthrough
- [CLI Reference](docs/guides/cli-reference.md) - Full `loom-daemon init` documentation

For detailed development workflows, see [DEV_WORKFLOW.md](DEV_WORKFLOW.md).

## Development Workflow

### Working on an Issue

If you'd like to work on an issue (subject to maintainer approval):

```bash
# 1. Claim the issue
gh issue edit <number> --add-label "loom:building"

# 2. Create a worktree
pnpm worktree <issue-number>
cd .loom/worktrees/issue-<number>

# 3. Make your changes
# ... implement, test, document ...

# 4. CRITICAL: Run full CI suite locally
pnpm check:ci

# 5. Commit and push
git add -A
git commit -m "Your descriptive commit message"
git push -u origin feature/issue-<number>

# 6. Create PR
gh pr create --label "loom:review-requested"
```

### IMPORTANT: Pre-PR Checklist

Before creating or updating a Pull Request, you **MUST** run:

```bash
pnpm check:ci
```

This runs the complete CI suite locally:
- Biome linting and formatting (TypeScript/JavaScript)
- Rust formatting (rustfmt)
- Clippy with all CI flags
- Cargo check
- Frontend build (TypeScript + Vite)
- All tests

**If `pnpm check:ci` fails, fix the issues before creating your PR.** PRs that fail CI will not be merged.

## Code Style and Conventions

### TypeScript/JavaScript

- **Linter**: Biome (configured in `biome.json`)
- **Format on save**: Enabled in VSCode settings
- **Strict mode**: TypeScript strict mode enabled
- **Commands**:
  - `pnpm lint` - Check for linting issues
  - `pnpm lint:fix` - Auto-fix linting issues
  - `pnpm format` - Check formatting
  - `pnpm format:write` - Auto-format code

### Rust

- **Formatter**: rustfmt (configured in `rustfmt.toml`)
- **Linter**: Clippy (configured in `.cargo/config.toml`)
- **Commands**:
  - `pnpm clippy` - Run Clippy with exact CI flags
  - `pnpm clippy:fix` - Auto-fix Clippy issues
  - `pnpm format:rust` - Check Rust formatting
  - `pnpm format:rust:write` - Auto-format Rust code

### Git Hooks

Pre-commit hooks automatically run via husky and lint-staged:
- TypeScript/JavaScript files are formatted and linted with Biome
- Rust files are formatted with rustfmt and linted with Clippy

If hooks fail, the commit will be blocked - fix issues before retrying.

## Testing

Loom has comprehensive testing at multiple levels:

```bash
# Run all tests
pnpm test

# Run daemon integration tests
pnpm daemon:test

# Run with verbose output
pnpm daemon:test:verbose
```

**Requirements**: Tests require `tmux` installed (`brew install tmux` on macOS)

See [docs/guides/testing.md](docs/guides/testing.md) for detailed testing documentation.

## Pull Request Process

1. **Create a feature branch** from `main`
2. **Implement your changes** following code style conventions
3. **Test thoroughly** - manual testing and automated tests
4. **Run `pnpm check:ci`** - ensure all checks pass locally
5. **Create a PR** with a clear description
6. **Address review feedback** from the Reviewer agent
7. **Wait for approval** - maintainer will merge when ready

### PR Description Guidelines

Your PR description should include:
- **Summary**: Brief description of what changed
- **Motivation**: Why this change is needed
- **Test Plan**: How you tested the changes
- **Related Issues**: Link to issue (e.g., "Closes #123")

### Label Workflow

Loom uses GitHub labels for coordination:

- **`loom:building`**: Issue is being worked on
- **`loom:review-requested`**: PR ready for review
- **`loom:changes-requested`**: PR needs fixes
- **`loom:pr`**: PR approved, ready to merge

See [WORKFLOWS.md](WORKFLOWS.md) for complete label workflow documentation.

## Common Mistakes to Avoid

1. Running `cargo clippy` directly instead of `pnpm clippy` (misses CI flags)
2. Using `app:dev` when working on Loom codebase (causes restart loops - use `app:preview` instead)
3. Missing dark mode variants on Tailwind classes
4. Creating nested worktrees (use `pnpm worktree --check` to verify location)
5. Not running `pnpm check:ci` before creating PR

## Git Worktree Helper

Loom uses git worktrees for isolated development:

```bash
# Create worktree for an issue
pnpm worktree <issue-number>

# Check current worktree status
pnpm worktree --check

# Show help
pnpm worktree --help
```

**Always use the helper script** - it prevents nested worktrees and ensures correct paths.

## Project Structure

```
loom/
├── src/                    # TypeScript frontend
│   ├── lib/               # State, config, UI, theme
│   └── main.ts            # Application entry point
├── src-tauri/             # Rust backend (Tauri commands)
├── loom-daemon/           # Rust daemon (terminal management)
├── .loom/                 # Workspace config (gitignored)
├── defaults/              # Default configuration templates
├── docs/                  # Detailed documentation
│   ├── guides/           # Development guides
│   ├── adr/              # Architecture decision records
│   └── workflows/        # Agent workflow docs
└── package.json          # pnpm scripts
```

## Additional Resources

### Essential Documentation

- **[README.md](README.md)** - Project overview and vision
- **[CLAUDE.md](CLAUDE.md)** - AI development context and patterns
- **[DEV_WORKFLOW.md](DEV_WORKFLOW.md)** - Development workflow with hot reload
- **[DEVELOPMENT.md](DEVELOPMENT.md)** - Code quality and testing
- **[WORKFLOWS.md](WORKFLOWS.md)** - Agent coordination via labels

### Development Guides

- **[Architecture Patterns](docs/guides/architecture-patterns.md)** - Observer pattern, IPC, worktrees
- **[TypeScript Conventions](docs/guides/typescript-conventions.md)** - Strict mode, type safety
- **[Code Quality](docs/guides/code-quality.md)** - Linting, formatting, CI/CD
- **[Testing](docs/guides/testing.md)** - Testing strategies and MCP tools
- **[Git Workflow](docs/guides/git-workflow.md)** - Branch strategy, commits, PRs
- **[Common Tasks](docs/guides/common-tasks.md)** - Adding features, properties
- **[Styling](docs/guides/styling.md)** - TailwindCSS, theme system

### External Documentation

- **[Tauri Documentation](https://tauri.app/)** - Desktop app framework
- **[Biome Documentation](https://biomejs.dev/)** - Linter and formatter
- **[Rust Book](https://doc.rust-lang.org/book/)** - Rust programming
- **[TypeScript Documentation](https://www.typescriptlang.org/docs/)** - TypeScript language
- **[TailwindCSS Documentation](https://tailwindcss.com/docs)** - Utility-first CSS

## Code of Conduct

Be respectful and constructive in all interactions:
- Be kind and courteous
- Respect differing viewpoints and experiences
- Accept constructive criticism gracefully
- Focus on what's best for the community and project
- Show empathy towards other community members

## License

By contributing to Loom, you agree that your contributions will be licensed under the MIT License.

See [LICENSE](LICENSE) for the full license text.

## Questions?

- **General questions**: Open a GitHub Discussion
- **Bug reports**: Create an issue with the `bug` label
- **Feature requests**: Create an issue with the `enhancement` label
- **Security issues**: See [SECURITY.md](SECURITY.md) (if it exists) or contact maintainers directly

---

**Thank you for contributing to Loom!** Your suggestions and feedback help make this project better.
