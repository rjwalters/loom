# Loom Daemon Integration Tests

Comprehensive integration tests for the loom-daemon.

## Running Tests

```bash
# Run all tests with process-per-test isolation — preferred
cargo nextest run --workspace

# Run specific test file
cargo nextest run --test integration_basic

# Plain cargo test still works; it is what runs doctests
cargo test --workspace --doc

# Run with output
cargo test -- --nocapture

# Run serially (required for tmux tests under plain `cargo test`)
cargo test -- --test-threads=1
```

> **Isolation**: under `cargo nextest run --workspace` every test gets its own
> process (see the crate-level "Test isolation convention" docs in
> `loom-daemon/src/lib.rs`, issue #4385). These `integration_*` suites touch the
> host-global `tmux -L loom` server — `setup()` calls `cleanup_all_loom_sessions()`,
> which kills *every* `loom-*` session, not just its own — and spawn real daemons,
> so `.config/nextest.toml` puts them in the `daemon-integration` test group with
> `max-threads = 1`: at most one is in flight at a time, which is the exclusion
> `cargo test` provided implicitly by running one test binary at a time. The filter
> is `binary(/^integration_/)`, so a new `integration_*` suite is covered
> automatically. Verify with
> `cargo nextest show-config test-groups --profile ci`.
>
> That group bounds one nextest run, not the machine. A `cargo test` in another
> checkout on the same host runs the same nuclear cleanup and will kill this run's
> sessions; the resulting "session ... does not exist" failures reproduce
> identically under plain `cargo test`, so check for sibling test runs before
> suspecting a regression.

## Test Structure

```
tests/
├── common/
│   └── mod.rs           # TestDaemon and TestClient helpers
├── integration_basic.rs # IPC and terminal lifecycle tests
└── README.md            # This file
```

## Test Helpers

### `TestDaemon`

Starts a daemon instance with an isolated socket path in a temp directory.
Automatically cleans up on drop.

```rust
let daemon = TestDaemon::start().await?;
let socket_path = daemon.socket_path();
```

**Confinement invariant (#4573)** — a test daemon is a *real* `loom-daemon`
process, so `TestDaemon::start()` spawns it fail-closed:

- `LOOM_ROLE_RUNNER=0`, `LOOM_WORK_FINDER=0`, `LOOM_EPIC_SUPERVISOR=0` — these
  env vars win outright over `.loom/config.json` in the daemon's
  `env > config > default` chain, so a test daemon can never dispatch a real
  sweep or run a real role session (this repo's own committed config has
  `autonomous.roleRunner.enabled: true`).
- `LOOM_WORKSPACE`, `LOOM_WORKSPACES_PATH`, `LOOM_WORKTREE_ROOT` — all pinned
  inside the daemon's own `TempDir`, so it resolves *no* real repository state,
  no matter what the invoking environment exports.

If you add a new daemon spawn site to this suite, carry the same env over (see
`integration_drain_then_exit.rs`). `integration_workspace_confinement.rs`
enforces the invariant for `TestDaemon` and fails loudly if one of these is
dropped.

### `TestClient`

Client for communicating with the daemon.

```rust
let mut client = TestClient::connect(socket_path).await?;
client.ping().await?;
let id = client.create_terminal("my-terminal", None).await?;
```

## Test Status

✅ **All 9 integration tests passing**

The test infrastructure successfully validates:
- Basic IPC communication (Ping/Pong)
- Error handling (malformed requests)
- Terminal lifecycle (create, list, destroy)
- Working directory support
- Input handling
- Multiple concurrent clients
- Error conditions (non-existent terminal)

## Requirements

- `tmux` must be installed
- Unix domain sockets (macOS/Linux only)

## Future Enhancements

- [ ] Implement persistence tests (daemon restart, session recovery)
- [ ] Add concurrency/stress tests (many terminals, rapid operations)
- [ ] Add output capture tests (when daemon supports it)
- [ ] Integrate with CI (requires tmux on runners)
- [ ] Add performance benchmarks
- [ ] Test edge cases (long terminal names, special characters, etc.)
