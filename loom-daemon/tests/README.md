# Loom Daemon Integration Tests

Comprehensive integration tests for the loom-daemon.

## Running Tests

```bash
# Run all integration tests
cargo test

# Run specific test file
cargo test --test integration_basic

# Run with output
cargo test -- --nocapture

# Run serially (required for tmux tests)
cargo test -- --test-threads=1
```

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
