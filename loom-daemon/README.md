# loom-daemon

The Rust daemon at the core of Loom: the Tier 2 orchestration backend that
dispatches and supervises `/loom:sweep` runs across managed repos, plus a large
family of native CLI subcommands that replaced Loom's earlier Python tooling
(epic #4081).

One daemon runs per machine (launchd/systemd-supervised), managing every repo
registered in `~/.loom/workspaces.json`. Clients — the `mcp-loom` MCP server,
`loom-daemon dispatch`, the fleet dashboard — talk to it over a Unix-socket IPC
protocol (default `~/.loom/loom-daemon.sock`).

## What lives here

- **Daemon runtime** — work finder, role runner, epic supervisor, event bus,
  sweep registry, health monitor, host/GitHub circuit breakers, capacity model
  (token pool × disk × CPU headroom).
- **Operator CLI** — `status`, `health`, `dispatch`, `watch`, `fleet`,
  `tokens`, `clean`, `recover-orphans`, `forge`, and more. Run
  `loom-daemon --help` for the full annotated list; each subcommand's help text
  is the authoritative reference.
- **Embedded dashboard** — `loom-daemon serve` hosts a read-only status page
  (`src/dashboard.html`) over the same IPC snapshot the CLI uses.

## Build & test

```bash
cargo build --package loom-daemon --release   # or: pnpm run daemon:build
cargo test --package loom-daemon              # or: pnpm run daemon:test
```

Dev workflow wrappers (`pnpm run daemon:dev`, `daemon:headless`, `daemon:stop`)
are documented in [`scripts/README.md`](../scripts/README.md).

## Documentation

- [`.loom/docs/daemon-reference.md`](../.loom/docs/daemon-reference.md) — MCP
  surface, event taxonomy, autonomous configuration, operability
- [`CLAUDE.md`](../CLAUDE.md) — orchestration architecture and usage modes
- [`docs/adr/`](../docs/adr) — architecture decision records (ADR-0009 covers
  the shepherd deprecation; epic #3449 rebuilt the daemon surface as this crate)
- [`tests/README.md`](tests/README.md) — integration test layout
