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
> host-global `tmux -L loom` server and spawn real daemons, so
> `.config/nextest.toml` puts them in the `daemon-integration` test group with
> `max-threads = 1`: at most one is in flight at a time, which is the exclusion
> `cargo test` provided implicitly by running one test binary at a time. Two of
> the four suites (`integration_security.rs`, `integration_factory_reset.rs`)
> additionally call `cleanup_all_loom_sessions()` in `setup()`, which kills
> *every* `loom-*` session, not just its own — a reviewed, commented exception
> for hardcoded/unprefixed terminal IDs a scoped cleanup can't see (issue
> #4622; see `tests/common/mod.rs`). The other two use the `TEST_PREFIX`-scoped
> `cleanup_test_sessions()`. The filter is `binary(/^integration_/)`, so a new
> `integration_*` suite is covered automatically. Verify with
> `cargo nextest show-config test-groups --profile ci`.
>
> That group bounds one nextest run, not the machine. A `cargo test` in another
> checkout on the same host runs the same nuclear cleanup and will kill this run's
> sessions; the resulting "session ... does not exist" failures reproduce
> identically under plain `cargo test`, so check for sibling test runs before
> suspecting a regression.

## Test Structure

This directory holds several dozen top-level `*.rs` integration binaries (each
compiles and runs as its own test target) plus shared support modules. It is
described by **category** rather than enumerated file-by-file, because a literal
listing goes stale on the next suite added — use `ls loom-daemon/tests/*.rs` for
the current set.

```
tests/
├── common/mod.rs           # TestDaemon / TestClient helpers (see "Test Helpers")
├── support/                # Shared non-daemon helpers (e.g. worker CLI harness)
├── doc_lint_support/       # Shared helpers for the *_doc_lint suites
├── fixtures/               # Static inputs: shell oracles, retired-script snapshots,
│                           #   OTLP transport captures, compiled-in .rs fixtures
├── integration_*.rs        # Full-daemon integration suites — spawn a real daemon and
│                           #   touch the host-global `tmux -L loom` server. Serialized
│                           #   by the `daemon-integration` nextest group (see above).
├── *_cli.rs                # Subcommand-surface suites — drive one `loom-daemon <cmd>`
│                           #   CLI (accounts, tokens check/unblock, forge check-open-pr,
│                           #   gh api repo flag …) without a running daemon.
├── *_doc_lint.rs           # Markdown contract checks over `defaults/.claude/commands/loom/`
│                           #   and `defaults/roles/` — read the rule below before adding to these.
└── <subject>.rs            # One-subject suites named for what they pin: worktree locking
                            #   and WIP verbs, clean/aggressive behavior, epic state
                            #   invariants, worker spawn, telemetry/collector fan-out,
                            #   model pricing, shell budget ratchet, and so on.
```

## Markdown Doc-Lint Tests: No New Prose-Existence Assertions

Some test files in this directory (`sweep_md_doc_lint.rs`, `bump_md_doc_lint.rs`,
`sweep_md_stage_minus_one_doc_lint.rs`) read a `.md` file from
`defaults/.claude/commands/loom/` or `defaults/roles/` and assert
`content.contains("<literal>")` against it — a **prose-existence assertion**: a
claim that a specific sentence, table row, or code-fence literal still exists
verbatim inside a markdown file.

**Do not add new prose-existence assertions.** This pattern has already broken
`main`:

- #7950 — a *correct* bug fix (#7876) reworded a pinned command-line literal
  inside `sweep.md`. The assertion failed on the corrected doc and took `main`
  red for 10 commits, over the same two-string mismatch each time.
- #7948 — the same failure shape produced a misdiagnosis: the failure message
  describes what the pinned text *means* ("lease-renewal races"), not the
  actual failure ("two strings no longer match"), so the next reader chased the
  wrong cause.

If you're about to add a `content.contains(...)` (or `.find(...)`) call to one
of these files, use one of the two patterns below instead:

1. **The literal lives inside a code fence** (a shell function, a JSON sample,
   a script template) — never pin it as a string. Extract the fenced block and
   **execute it**. This is the pattern already used by the ~20
   `defaults/scripts/tests/test-guide-*.sh` suites: locate the `.md` file, pull
   the shell functions defined in its fenced code blocks, and run them. That
   pattern fails when *behavior* changes; it does not fail when the prose
   around the fence is reworded or the fence is moved under a different
   heading.
2. **Genuine prose guidance with no executable surface** (a sentence, a
   rationale paragraph, a recommendation) — there is nothing to execute. Rely
   on human review at PR time, not a brittle string match. A `contains()`
   assertion over a sentence only ever verifies "this exact wording still
   exists" — it fails on a correct reword and has nothing to say about whether
   a subtly wrong rewording is actually wrong.

This rule governs new assertions going forward. It is not a mandate to rewrite
the doc-lint tests that already exist in this directory on sight — the
mechanical migration of those is tracked from #7979. Note those files already
distinguish CONTRACT identifiers (stable tokens like IPC variant names or
config keys, legitimately pinned exact) from PROSE (asserted structurally,
e.g. by heading prefix or a tolerant phrasing set) per #3877 — that framework
predates and is compatible with this rule, but "pin the CONTRACT identifier
exact" is not license to add a fresh *sentence* pin under a PROSE label.

**Ratchet-exemption decision**: see the "Correctness-PR exemption decision"
comment block in `scripts/check-markdown-token-budget.sh` for the recorded
answer to "should a purely-correctness PR be exempt from the per-file markdown
token ratchet when the fix requires adding explanatory text?" (short version:
no — trim elsewhere).

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
