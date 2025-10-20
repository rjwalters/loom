# Repository Guidelines

## Project Structure & Module Organization
The Vite-powered frontend lives in `src/` with feature modules under `src/lib/` and colocated Vitest specs (`*.test.ts`). Tauri glue code and native bindings reside in `src-tauri/`, while the Rust daemon runs from `loom-daemon/`. Runtime defaults and role templates sit in `defaults/` and `.loom/roles/`; automation helpers live in `scripts/`, and production bundles emit to `dist/`.

## Build, Test, and Development Commands
- `pnpm app:dev` boots the daemon then Tauri for an end-to-end local session.
- `pnpm daemon:dev` + `pnpm tauri:dev` mirrors the two-terminal workflow in `DEV_WORKFLOW.md`.
- `pnpm build`, `pnpm daemon:build`, and `pnpm tauri:build` produce frontend assets, the daemon binary, and the packaged desktop app.
- Quality gates: `pnpm lint`, `pnpm format:rust`, `pnpm clippy`, and `pnpm check:ci`.

## Coding Style & Naming Conventions
TypeScript follows Biome defaults (two-space indent, trailing commas, explicit semicolons); prefer `camelCase` for functions and `PascalCase` for exported classes. Keep modules single-purpose and expose public helpers from `src/lib/` index files only when shared. Rust code must pass `cargo fmt` and `cargo clippy -- -D warnings`; keep IPC command enums and structs serde-annotated for stability.

## Testing Guidelines
Rust suites run through `pnpm test` (aliased to `cargo test --workspace --locked --all-features`). Frontend units reside beside their sources and execute with `pnpm test:unit`; use `pnpm test:unit:coverage` when verifying regressions or new UI states. Mirror test filenames after the subject and include minimal fixtures so daemon logs stay readable.

## Commit & Pull Request Guidelines
Commits use imperative, title-cased subjects with the related issue in parentheses (e.g., `Fix tmux socket mismatch (#144)`). Group work logically and include generated artifacts or schema updates in the same change. Pull requests should outline intent, list the commands you ran, and link issues or Loom workflow references. Attach screenshots or terminal excerpts for UX-facing updates and call out any skipped checks.

## Git Worktree Workflow
Use the worktree helper script for isolated work on issues:

```bash
# Create worktree for issue #42
./.loom/scripts/worktree.sh 42
# → Creates: .loom/worktrees/issue-42
# → Branch: feature/issue-42

# Change to worktree
cd .loom/worktrees/issue-42

# Do your work, then commit and push
git add -A
git commit -m "Your message"
git push -u origin feature/issue-42

# Return to main workspace
cd ../..
```

### Resuming Abandoned Work

If a previous agent abandoned work on an issue, you can resume seamlessly:

```bash
# The branch feature/issue-42 exists, but worktree was removed
./.loom/scripts/worktree.sh 42
# → Reuses existing branch (no prompt)
# → Creates fresh worktree
# → Continue where previous agent left off
```

The script is **non-interactive** and automatically reuses existing branches, making it safe for AI agents to resume abandoned work without user intervention.

## Daemon & Configuration Notes
Agent role prompts live under `.loom/roles/`; keep Markdown and any sibling JSON metadata in sync. Workspace overrides persist in `~/.loom/`, so mention reset steps (`Help → Daemon Status → Yes`) when altering stateful behavior. Document new environment variables or defaults under `defaults/` before requesting review.
