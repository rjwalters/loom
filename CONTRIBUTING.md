# Contributing to Loom

Loom is not a typical open-source project. It is built primarily by AI agents — Claude Code instances orchestrated by the Loom system itself. There is no core team of human developers writing code day-to-day. Instead, the system generates work, builds features, reviews its own PRs, and merges them.

This means the most valuable thing you can do is **use Loom on real projects and tell us what happens**.

## How to Contribute

### Use Loom

The single most impactful contribution is running Loom against a real codebase. The agents can't discover problems they don't encounter. Every time you use Loom and something goes wrong — a confusing error, a broken workflow, a daemon that hangs, a PR that doesn't make sense — that's information we need.

Install Loom, point it at a project, and see what happens. The [Getting Started Guide](docs/guides/getting-started.md) will walk you through setup.

### Report Bugs

When something breaks, [open an issue](https://github.com/rjwalters/loom/issues/new). Good bug reports include:

- **What you were doing** — which mode (daemon, manual, app), which roles were active
- **What went wrong** — error messages, unexpected behavior, things that got stuck
- **Logs if you have them** — daemon state, terminal output, GitHub Actions logs
- **Your environment** — macOS version, Loom version, how you installed it

Don't worry about formatting it perfectly. A rough report is infinitely more useful than silence.

### Request Features

If you find yourself wishing Loom did something differently, [open an issue](https://github.com/rjwalters/loom/issues/new). Describe the workflow you want, not just the feature. "I want Loom to handle monorepos" is good. "I want a `--monorepo` flag" is less useful because it assumes an implementation.

The system's Architect agent scans issues and generates proposals for features it considers viable. Maintainers review and approve proposals, and then the Builder agents implement them.

### Share Your Experience

Comment on existing issues with additional context. If you hit the same bug someone else reported, say so — frequency signals priority. If you tried a workaround, share it. If you have opinions about how a feature should work, weigh in.

## What About Pull Requests?

PRs in this repository are almost exclusively created by the agent system. The typical lifecycle is:

1. A human (or the Architect agent) creates an issue
2. The Curator agent refines it with technical details
3. The Builder agent implements it in a worktree
4. The Judge agent reviews the PR
5. The Champion agent merges it

If you want to submit a PR directly, you're welcome to, but know that it will be reviewed by both AI agents and the maintainer. For small fixes (typos, broken links, documentation clarifications), a PR is fine. For anything substantial, open an issue first — the system may be able to build it faster than you expect, and the issue ensures your intent is captured even if the implementation takes a different shape.

### If You Do Submit a PR

Run `pnpm check:ci` before pushing. This runs the full CI suite locally (linting, formatting, type checking, tests). PRs that fail CI won't be merged.

## Development Setup

If you want to explore the codebase or run Loom locally:

**Prerequisites**: Node.js 20+, pnpm, Rust (stable), tmux, Git, GitHub CLI (`gh`)

```bash
git clone https://github.com/rjwalters/loom.git
cd loom
pnpm install
pnpm app:dev
```

Or headless (no GUI):

```bash
cargo build --release -p loom-daemon
./target/release/loom-daemon init /path/to/your/repo
```

See the [Getting Started Guide](docs/guides/getting-started.md) and [CLI Reference](docs/guides/cli-reference.md) for details.

## Project Structure

```
loom/
├── src/                    # TypeScript frontend (Svelte + Tauri)
├── src-tauri/              # Rust backend (Tauri commands)
├── loom-daemon/            # Rust daemon (terminal management)
├── .loom/                  # Workspace config and agent roles
├── defaults/               # Default configuration templates
├── docs/                   # Documentation
└── scripts/                # Installation and setup scripts
```

## Code of Conduct

Be respectful and constructive. This is a small project and a weird one — AI building software is new territory for everyone. Patience and good faith go a long way.

## License

Contributions are licensed under the [MIT License](LICENSE).

## Questions?

- **Bug reports and feature requests**: [GitHub Issues](https://github.com/rjwalters/loom/issues)
- **General discussion**: [GitHub Discussions](https://github.com/rjwalters/loom/discussions)
- **Security concerns**: Contact the maintainer directly
