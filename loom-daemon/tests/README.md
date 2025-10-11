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

## Known Issues

**Status: Tests currently failing with EOF errors**

The test infrastructure is complete but tests are failing because:
- Daemon may not be starting properly in test environment
- Need to capture daemon stderr/stdout for debugging
- May need better synchronization for daemon startup

This is work-in-progress. The test infrastructure is solid and provides a foundation for
debugging and fixing the actual daemon initialization issues.

## Requirements

- `tmux` must be installed
- Unix domain sockets (macOS/Linux only)

## TODO

- [ ] Fix daemon startup in test environment
- [ ] Add better error messages and debugging output
- [ ] Implement persistence tests (daemon restart, etc.)
- [ ] Add concurrency tests
- [ ] Add error condition tests
- [ ] Integrate with CI
