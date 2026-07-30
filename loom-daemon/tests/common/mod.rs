// Test infrastructure - expect/unwrap are acceptable here since tests should panic on failure
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;

/// Per-binary unique prefix. Generated once per test binary execution (process),
/// shared across all tests in that binary. This allows cleanup to be scoped to
/// only sessions created by the current binary, preventing cross-binary interference
/// when multiple integration test binaries run in parallel.
static TEST_PREFIX: LazyLock<String> =
    LazyLock::new(|| format!("test-{}", uuid::Uuid::new_v4().simple()));

/// How long [`TestDaemon::start`] waits for the daemon to create its socket.
///
/// This is a **liveness** bound, not a latency assertion: it exists so a wedged
/// daemon fails the test instead of hanging it. It is deliberately generous
/// because CI runs the suite process-per-test under `cargo nextest` (#4385), so
/// the daemon can be competing for CPU with `num-cpus` sibling test processes.
/// The previous 5s budget was calibrated for `cargo test`, which runs one test
/// binary at a time, and it produced spurious "failed to create socket within 5s"
/// failures on a loaded host.
// Not every test binary that includes this module uses `TestDaemon` (e.g. tests
// that need raw `Child` control over the daemon they spawn), so the shared
// helper is `dead_code`-exempt rather than warning per-binary.
#[allow(dead_code)]
const DAEMON_SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

/// Absolute path to the `loom-daemon` binary under test.
///
/// Cargo builds the package's bin target before running an integration test and
/// hands us the path in `CARGO_BIN_EXE_<name>`. Resolving it this way is not just
/// tidier than hardcoding `../target/debug/loom-daemon` (which ignored
/// `CARGO_TARGET_DIR`) — it is load-bearing under process-per-test execution.
/// This harness previously shelled out to `cargo build --bin loom-daemon` on
/// *every* `TestDaemon::start()`; with one process per test those builds
/// serialize on `$CARGO_HOME/.package-cache` and the target-directory lock, which
/// are **cross-process** resources that test process isolation does nothing to
/// separate (#4385) — and the build raced the very binary it was about to spawn.
pub fn daemon_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_loom-daemon"))
}

/// Test daemon instance that cleans up on drop
#[allow(dead_code)]
pub struct TestDaemon {
    _temp_dir: TempDir,
    socket_path: PathBuf,
    process: Option<Child>,
}

#[allow(dead_code)]
impl TestDaemon {
    /// Start a new daemon instance with a unique socket path
    pub async fn start() -> Result<Self> {
        let temp_dir = TempDir::new().context("Failed to create temp directory")?;
        let socket_path = temp_dir.path().join("daemon.sock");
        // Isolated registry file (mirrors `integration_drain_then_exit.rs`): an
        // empty/scratch `LOOM_WORKSPACES_PATH` guarantees `effective_roots()`
        // reduces to the single seeded default (`LOOM_WORKSPACE`, set below)
        // rather than silently picking up whatever repos are registered in
        // this *host's* real `~/.loom/workspaces.json`.
        let workspaces_path = temp_dir.path().join("workspaces.json");
        // Absolute (required — `worktree_root()` rejects a relative override
        // and falls back to the default) and inside the `TempDir`.
        let worktree_root = temp_dir.path().join("worktrees");

        let mut process = Command::new(daemon_bin())
            .env("LOOM_SOCKET_PATH", &socket_path)
            .env("RUST_LOG", "debug")
            // Disable restore_from_tmux() to prevent cross-test-binary contamination
            // via the shared tmux server. Each test manages its own terminals.
            .env("LOOM_NO_RESTORE", "1")
            // Fail-closed autonomy toggles (#4573): without these, a spawned
            // test daemon inherits this repo's real `.loom/config.json`
            // (`autonomous.roleRunner.enabled: true`) and can dispatch real
            // `/loom:sweep` sessions — burning API/GitHub rate-limit quota in
            // what is meant to be an inert integration-test daemon. Each of
            // these env vars *wins outright* over config in the daemon's own
            // env > config > default precedence (`resolve_enabled` in
            // `role_runner.rs` / `work_finder.rs` / `epic_supervisor.rs`), so
            // setting them here is authoritative regardless of what any
            // repo's committed config says.
            .env("LOOM_ROLE_RUNNER", "0")
            .env("LOOM_WORK_FINDER", "0")
            .env("LOOM_EPIC_SUPERVISOR", "0")
            // Repoint the daemon's own workspace root at this test's throwaway
            // `TempDir` (#4573) instead of letting it inherit the real repo
            // checkout via `LOOM_WORKSPACE`/cwd.
            .env("LOOM_WORKSPACE", temp_dir.path())
            .env("LOOM_WORKSPACES_PATH", &workspaces_path)
            // …and pin the worktree base directory too (#4573). This is NOT
            // redundant with `LOOM_WORKSPACE`: `worktree_root()` reads
            // `LOOM_WORKTREE_ROOT` as its *highest-priority* source
            // (`worktree_root.rs`), ahead of both `worktree.root` in config
            // and the `${repo_root}/.loom/worktrees` default. A spawned child
            // inherits the parent's environment, so on a host that sets
            // `LOOM_WORKTREE_ROOT` (the documented external-scratch-volume
            // setup) an unpinned test daemon would resolve worktrees onto that
            // real volume — outside its `TempDir` — even with `LOOM_WORKSPACE`
            // confined. Setting it explicitly makes confinement independent of
            // the invoking environment. Note the daemon namespaces an
            // *override* by repo basename, so the effective root is
            // `<temp>/worktrees/<temp-basename>` — still inside the `TempDir`.
            .env("LOOM_WORKTREE_ROOT", &worktree_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn daemon")?;

        // Wait for socket to be created (with timeout)
        let start = std::time::Instant::now();
        while !socket_path.exists() {
            if start.elapsed() > DAEMON_SOCKET_TIMEOUT {
                // Kill the process and get logs
                let _ = process.kill();
                let output = process.wait_with_output()?;
                anyhow::bail!(
                    "Daemon failed to create socket within {}s.\nStderr: {}",
                    DAEMON_SOCKET_TIMEOUT.as_secs(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Ok(Self {
            _temp_dir: temp_dir,
            socket_path,
            process: Some(process),
        })
    }

    /// Get the socket path for connecting clients
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The throwaway `TempDir` this daemon was pinned to via `LOOM_WORKSPACE`
    /// (#4573) — the workspace root the daemon must resolve to, never the
    /// real repo checkout.
    #[allow(dead_code)]
    pub fn workspace_path(&self) -> &Path {
        self._temp_dir.path()
    }

    /// PID of the spawned daemon child, or `None` once it has been reaped.
    ///
    /// Exposed so the #4573 confinement regression test can read the child's
    /// *actual* environment (`/proc/<pid>/environ` on Linux) rather than
    /// trusting that this module's `.env()` calls are still present — the only
    /// way to verify pass-through for `LOOM_WORKTREE_ROOT`, which the daemon
    /// consumes internally and never reports over IPC.
    #[allow(dead_code)]
    pub fn pid(&self) -> Option<u32> {
        self.process.as_ref().map(std::process::Child::id)
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            // Try graceful shutdown first
            let _ = process.kill();
            let _ = process.wait();
        }
        // temp_dir cleanup handled by TempDir's Drop
    }
}

/// Test client for communicating with daemon
pub struct TestClient {
    reader: BufReader<tokio::io::ReadHalf<UnixStream>>,
    writer: tokio::io::WriteHalf<UnixStream>,
}

impl TestClient {
    /// Connect to daemon at given socket path with retry logic
    ///
    /// The daemon creates the socket file before it starts listening, creating a race condition.
    /// This method retries with exponential backoff to handle this race.
    ///
    /// The retry budget (~6s of backoff) is sized for process-per-test execution
    /// under `cargo nextest` (#4385): the daemon may be descheduled between
    /// creating the socket and calling `listen(2)` while `num-cpus` sibling test
    /// processes run. The previous 5-attempt budget (~0.75s) was calibrated for
    /// `cargo test`'s one-binary-at-a-time model.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let max_retries = 8;
        let mut retry_delay = Duration::from_millis(50);

        for attempt in 0..max_retries {
            match timeout(Duration::from_secs(2), UnixStream::connect(socket_path)).await {
                Ok(Ok(stream)) => {
                    let (reader, writer) = tokio::io::split(stream);
                    let reader = BufReader::new(reader);
                    return Ok(Self { reader, writer });
                }
                Ok(Err(_e)) if attempt < max_retries - 1 => {
                    // Connection failed, retry with backoff
                    tokio::time::sleep(retry_delay).await;
                    retry_delay *= 2; // Exponential backoff
                }
                Ok(Err(e)) => {
                    // Final attempt failed
                    return Err(e).context("Failed to connect to daemon");
                }
                Err(_) => {
                    return Err(anyhow::anyhow!("Timeout connecting to daemon"));
                }
            }
        }

        unreachable!("Loop should always return before reaching here")
    }

    /// Send a request and receive a response
    pub async fn send_request(&mut self, request: serde_json::Value) -> Result<serde_json::Value> {
        // Serialize and send request
        let request_json = serde_json::to_string(&request)?;
        self.writer
            .write_all(request_json.as_bytes())
            .await
            .context("Failed to write request")?;
        self.writer
            .write_all(b"\n")
            .await
            .context("Failed to write newline")?;
        self.writer.flush().await.context("Failed to flush")?;

        // Read response
        let mut response_line = String::new();
        timeout(Duration::from_secs(2), self.reader.read_line(&mut response_line))
            .await
            .context("Timeout reading response")?
            .context("Failed to read response")?;

        // Parse response
        serde_json::from_str(&response_line).context("Failed to parse response JSON")
    }

    /// Helper: Send Ping request
    #[allow(dead_code)]
    pub async fn ping(&mut self) -> Result<()> {
        let request = serde_json::json!({"type": "Ping"});
        let response = self.send_request(request).await?;

        if response != serde_json::json!({"type": "Pong"}) {
            anyhow::bail!("Expected Pong, got: {response:?}");
        }

        Ok(())
    }

    /// Helper: Create terminal
    ///
    /// For security tests, the first parameter (id) is used as the `config_id`.
    /// For non-security tests that need unique IDs, use `create_terminal_with_unique_id` instead.
    #[allow(dead_code)]
    pub async fn create_terminal(
        &mut self,
        id: impl Into<String>,
        working_dir: Option<String>,
    ) -> Result<String> {
        let id_str: String = id.into();
        // Use the provided ID as both config_id and name for security testing
        self.create_terminal_with_config(&id_str, &id_str, working_dir, None, None)
            .await
    }

    /// Helper: Create terminal with auto-generated unique ID
    ///
    /// Uses the per-binary `TEST_PREFIX` so that cleanup can be scoped to
    /// only sessions created by the current test binary.
    #[allow(dead_code)]
    pub async fn create_terminal_with_unique_id(
        &mut self,
        name: impl Into<String>,
        working_dir: Option<String>,
    ) -> Result<String> {
        // Incorporate the binary-specific prefix so tmux sessions are namespaced
        let config_id = format!("{}-{}", *TEST_PREFIX, uuid::Uuid::new_v4().simple());
        self.create_terminal_with_config(config_id, name, working_dir, None, None)
            .await
    }

    /// Helper: Create terminal with explicit configuration parameters
    #[allow(dead_code)]
    pub async fn create_terminal_with_config(
        &mut self,
        config_id: impl Into<String>,
        name: impl Into<String>,
        working_dir: Option<String>,
        role: Option<String>,
        instance_number: Option<u32>,
    ) -> Result<String> {
        let config_id: String = config_id.into();
        let name: String = name.into();

        let request = serde_json::json!({
            "type": "CreateTerminal",
            "payload": {
                "config_id": config_id,
                "name": name,
                "working_dir": working_dir,
                "role": role,
                "instance_number": instance_number
            }
        });

        let response = self.send_request(request).await?;

        if let Some(payload) = response.get("payload") {
            if let Some(id) = payload.get("id") {
                return Ok(id.as_str().unwrap().to_string());
            }
        }
        anyhow::bail!("Unexpected response: {response:?}");
    }

    /// Helper: List terminals
    #[allow(dead_code)]
    pub async fn list_terminals(&mut self) -> Result<Vec<serde_json::Value>> {
        let request = serde_json::json!({"type": "ListTerminals"});
        let response = self.send_request(request).await?;

        if let Some(payload) = response.get("payload") {
            if let Some(terminals) = payload.get("terminals") {
                return Ok(terminals.as_array().unwrap().clone());
            }
        }
        anyhow::bail!("Unexpected response: {response:?}");
    }

    /// Helper: Destroy terminal
    #[allow(dead_code)]
    pub async fn destroy_terminal(&mut self, id: &str) -> Result<()> {
        let request = serde_json::json!({
            "type": "DestroyTerminal",
            "payload": { "id": id }
        });

        let response = self.send_request(request).await?;

        if response != serde_json::json!({"type": "Success"}) {
            anyhow::bail!("Expected Success, got: {response:?}");
        }

        Ok(())
    }

    /// Helper: Send input to terminal
    /// Returns the `input_id` for tracking git changes
    #[allow(dead_code)]
    pub async fn send_input(&mut self, id: &str, data: &str) -> Result<i64> {
        let request = serde_json::json!({
            "type": "SendInput",
            "payload": { "id": id, "data": data }
        });

        let response = self.send_request(request).await?;

        // Accept both Success (legacy) and InputSent (new) responses
        if response.get("type") == Some(&serde_json::json!("Success")) {
            return Ok(0);
        }

        if response.get("type") == Some(&serde_json::json!("InputSent")) {
            if let Some(payload) = response.get("payload") {
                if let Some(input_id) = payload.get("input_id") {
                    return Ok(input_id.as_i64().unwrap_or(0));
                }
            }
        }

        anyhow::bail!("Expected Success or InputSent, got: {response:?}");
    }

    /// Helper: Check session health for a terminal ID
    #[allow(dead_code)]
    pub async fn check_session_health(&mut self, id: &str) -> Result<bool> {
        let request = serde_json::json!({
            "type": "CheckSessionHealth",
            "payload": { "id": id }
        });

        let response = self.send_request(request).await?;

        if let Some(payload) = response.get("payload") {
            if let Some(has_session) = payload.get("has_session") {
                return Ok(has_session.as_bool().unwrap_or(false));
            }
        }

        anyhow::bail!("Unexpected response: {response:?}");
    }
}

/// Helper: Check if a tmux session exists
#[allow(dead_code)]
pub fn tmux_session_exists(session_name: &str) -> bool {
    Command::new("tmux")
        .args(["-L", "loom", "has-session", "-t", session_name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Helper: Kill a tmux session (for cleanup)
pub fn kill_tmux_session(session_name: &str) {
    let _ = Command::new("tmux")
        .args(["-L", "loom", "kill-session", "-t", session_name])
        .output();
}

/// Helper: Get list of all loom-* tmux sessions
pub fn get_loom_tmux_sessions() -> Vec<String> {
    let output = Command::new("tmux")
        .args(["-L", "loom", "list-sessions", "-F", "#{session_name}"])
        .output();

    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("loom-"))
            .map(std::string::ToString::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Helper: Kill **every** `loom-*` tmux session on the shared `-L loom` server —
/// not just this test binary's own sessions.
///
/// # Danger (issue #4622)
///
/// This is host-wide and unscoped: on a shared host it can destroy another
/// user's sessions, a concurrently running (unrelated) test binary's sessions,
/// or a live *production* `loom-daemon`'s real work sessions. **Prefer
/// [`cleanup_test_sessions`] by default** — it is scoped to this binary's
/// `TEST_PREFIX` and is safe for the overwhelming majority of tests.
///
/// Reach for this nuclear variant only when a suite provably creates tmux
/// sessions whose names do **not** carry `TEST_PREFIX` (e.g. hardcoded/literal
/// terminal IDs used to test injection handling or to mirror a fixed
/// production naming scheme) — in which case `cleanup_test_sessions()` cannot
/// see, and therefore cannot clean up, those sessions. Every such call site
/// must carry a comment at the point of use explaining *why* the scoped helper
/// is insufficient there (see `integration_security.rs` and
/// `integration_factory_reset.rs` for examples). Do not use it just for
/// convenience or as a "belt and suspenders" default — `cleanup_test_sessions()`
/// is enough for any suite whose terminal IDs are TEST_PREFIX-scoped.
#[allow(dead_code)]
pub fn cleanup_all_loom_sessions() {
    for session in get_loom_tmux_sessions() {
        kill_tmux_session(&session);
    }
}

/// Helper: Get loom tmux sessions scoped to the current test binary's prefix.
///
/// Only returns sessions whose names start with `loom-{TEST_PREFIX}`, ensuring
/// that parallel test binaries don't interfere with each other.
#[allow(dead_code)]
pub fn get_test_tmux_sessions() -> Vec<String> {
    let prefix = format!("loom-{}", *TEST_PREFIX);
    get_loom_tmux_sessions()
        .into_iter()
        .filter(|session| session.starts_with(&prefix))
        .collect()
}

/// Helper: Clean up only tmux sessions belonging to the current test binary.
///
/// Uses `TEST_PREFIX` to scope cleanup so that parallel test binaries — and
/// anything else sharing the host's `-L loom` tmux server, including another
/// user's sessions or a live production `loom-daemon` — are left untouched.
/// This is the default cleanup helper; see [`cleanup_all_loom_sessions`] for
/// the narrow, justified-only-per-call-site exception.
#[allow(dead_code)]
pub fn cleanup_test_sessions() {
    for session in get_test_tmux_sessions() {
        kill_tmux_session(&session);
    }
}

/// Helper: Check if the tmux server is running
#[allow(dead_code)]
pub fn tmux_server_running() -> bool {
    Command::new("tmux")
        .args(["-L", "loom", "list-sessions"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Probe whether tmux is actually *usable* on this host (issue #3985).
///
/// Returns `true` only when the `tmux` binary is present AND a server can be
/// started on the dedicated `-L loom` socket — i.e. a throwaway detached
/// session can be created and torn down. This is stronger than
/// [`tmux_server_running`] (which only checks whether a server is *already*
/// up): the terminal integration tests need to *create* sessions, so what
/// matters is whether tmux can fork a server at all, not whether one already
/// exists.
///
/// The point is to let host-dependent terminal tests **skip cleanly** on a
/// machine without a working tmux (no binary, dead server, unwritable
/// `/tmp/tmux-*` socket dir) instead of reddening the shared build gate. CI —
/// which controls its environment and always has a working tmux — still
/// exercises every one of these paths, so coverage is unchanged where it
/// matters. See `.loom/docs/build-gate.md` (§"Local gate vs. CI").
#[allow(dead_code)]
pub fn tmux_available() -> bool {
    let probe = format!("loom-{}-tmuxprobe", *TEST_PREFIX);
    // `new-session -d` (detached) with the default shell forks a server on the
    // loom socket if one isn't already running. Success => tmux is usable.
    let started = Command::new("tmux")
        .args(["-L", "loom", "new-session", "-d", "-s", &probe])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if started {
        kill_tmux_session(&probe);
    }
    started
}

/// Helper: Capture terminal output using tmux capture-pane
///
/// Uses tmux's built-in capture mechanism to read the terminal's pane content.
/// Returns the captured output as a String.
///
/// This function includes retry logic to handle transient tmux server state
/// issues that can occur during test setup/teardown.
///
/// # Arguments
/// * `session_name` - The tmux session name to capture from
///
/// # Returns
/// * `Result<String>` - The captured output or an error message
#[allow(dead_code)]
pub fn capture_terminal_output(session_name: &str) -> Result<String> {
    const MAX_RETRIES: u32 = 3;
    const RETRY_DELAY_MS: u64 = 100;

    let mut last_error = String::new();

    for attempt in 0..MAX_RETRIES {
        // First verify the session exists
        if !tmux_session_exists(session_name) {
            last_error = format!("tmux session '{session_name}' does not exist");
            if attempt < MAX_RETRIES - 1 {
                std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));
                continue;
            }
            anyhow::bail!("{last_error}");
        }

        let output = Command::new("tmux")
            .args(["-L", "loom", "capture-pane", "-t", session_name, "-p"])
            .output()
            .context("Failed to execute tmux capture-pane")?;

        if output.status.success() {
            return String::from_utf8(output.stdout).context("Invalid UTF-8 in captured output");
        }

        last_error = String::from_utf8_lossy(&output.stderr).to_string();

        // If this is not the last attempt, wait and retry
        if attempt < MAX_RETRIES - 1 {
            std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));
        }
    }

    anyhow::bail!("tmux capture-pane failed after {MAX_RETRIES} attempts: {last_error}")
}
