# Loom AI Agent Guide

The goal of this document is to help both human developers and AI agents (Claude Code and Codex) ramp quickly while keeping token usage low. It captures the minimum shared context, points to deeper references when needed, and highlights agent-specific guardrails.

## Core References
- `README.md` – product overview and installation.
- `DEV_WORKFLOW.md` – dual-terminal workflow for daemon + Tauri.
- `DEVELOPMENT.md` – debugging tips, known gotchas.
- `docs/adr/README.md` – architecture decision index (consult before altering core patterns).

## Repository Layout
- `src/` – Vite frontend; features live under `src/lib/` with colocated `*.test.ts`.
- `src-tauri/` – Rust glue for Tauri and IPC.
- `loom-daemon/` – standalone Rust daemon.
- `defaults/` & `.loom/roles/` – runtime defaults and agent role prompts.
- `scripts/` – automation helpers; prefer these over ad-hoc commands.

## Essential Commands
- App lifecycle: `pnpm app:dev`, `pnpm daemon:dev`, `pnpm tauri:dev`.
- Builds: `pnpm build`, `pnpm daemon:build`, `pnpm tauri:build`.
- Quality gates: `pnpm lint`, `pnpm check:ci`, `pnpm clippy`, `pnpm format:rust`.
- Tests: `pnpm test` (Rust workspace), `pnpm test:unit`, `pnpm test:unit:coverage`.

## Engineering Guardrails
- TypeScript: Biome defaults (two spaces, trailing commas, explicit semicolons).
- Rust: must pass `cargo fmt` and `cargo clippy -- -D warnings`; serde-annotate IPC structs/enums.
- Tests should mirror filenames of the subject under test and keep fixtures minimal.
- Commits use imperative, Title Case subjects with issue references (e.g., `Fix Tmux Socket Mismatch (#144)`).
- Document new defaults or env vars under `defaults/` before requesting review.
- When touching `.loom` state, mention reset steps (`Help → Daemon Status → Yes`).

## AI Agent Alignment

### Shared Playbook
- Treat `AGENTS.md` as the quickstart index; update it alongside this file.
- Run relevant checks for any code edits and flag uncertainty in task outputs.
- Prefer small, reviewable patches and incremental commits.
- Review AI-authored changes like human contributions—reject anything that fails lint, tests, or security expectations.

### Claude Code Notes
- Uses `.claude/settings.json` for pre-approved commands and MCP server configuration; extend these when adding workflows.
- Personal overrides belong in `.claude/settings.local.json`.
- Keep `.loom/roles/` prompts in sync with new automation patterns.

### Codex CLI Notes
- Shares instructions from `AGENTS.md` and this guide; there is no dedicated `.codex` directory.
- Prompts should spell out deliverables, validation commands, and guardrails.
- `.codex/config.toml` mirrors `.claude/settings.json` permissions (workspace-write sandbox, no approval prompts); update both when policies change.
- Rely on existing scripts (`scripts/`), `pnpm`, and `cargo` rather than inventing ad-hoc sandbox commands.
- Treat Codex output as a draft—tight loops, explicit follow-ups for failing checks, minimal-scope edits unless broader refactors are requested.

## When You Need More Detail
- Architecture deep dives: consult ADRs or the specific module docs under `docs/`.
- UI patterns: see `docs/ui/` and Tailwind guidelines in `style.css`.
- Daemon behavior and tmux integration: `DAEMON_DEV_MODE.md` plus ADR-0008.
- Automation workflows and GitHub label state machine: `WORKFLOWS.md`.
