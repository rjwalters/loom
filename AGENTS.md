# Loom Agent Quickstart

Use this as the lightweight entrypoint for human contributors and AI agents. For deeper context, see `CLAUDE.md`.

## Must-Know Commands
- Run the full loop with `pnpm app:dev` (daemon + Tauri).
- Standalone: `pnpm daemon:dev`, `pnpm tauri:dev`.
- Quality gates: `pnpm lint`, `pnpm check:ci`, `pnpm clippy`, `pnpm format:rust`.
- Tests: `pnpm test`, `pnpm test:unit`, `pnpm test:unit:coverage`.

## Style & Review Guardrails
- TypeScript: Biome defaults (two spaces, trailing commas, semicolons).
- Rust: enforce `cargo fmt` and `cargo clippy -- -D warnings`; serde-tag IPC structs/enums.
- Keep features scoped, expose shared helpers from `src/lib/index.ts` only when needed.
- Commits: imperative Title Case with issue reference, e.g., `Fix Tmux Socket Mismatch (#144)`.

## Operational Notes
- Role prompts live under `.loom/roles/`; keep Markdown and JSON metadata in sync.
- Document new defaults/env vars under `defaults/` before review.
- Resetting local state: `Help → Daemon Status → Yes` after touching `.loom` data.

For architecture background, workflows, and agent-specific guidance, jump to `CLAUDE.md`.
