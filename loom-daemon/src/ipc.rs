use crate::activity::{ActivityDb, AgentInput, AgentOutput, InputContext, InputType};
use crate::errors::DaemonError;
use crate::event_bus::EventBus;
use crate::forge_parser::parse_forge_events;
use crate::git_parser;
use crate::git_utils;
use crate::sweep_registry::SweepRegistry;
use crate::terminal::TerminalManager;
use crate::types::{Event, Request, Response};
use anyhow::Result;
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Bound on the singleton-guard liveness probe (#3806). Both the connect and
/// the `Ping`/`Pong` roundtrip are individually capped at this duration so a
/// hung or unresponsive peer can never stall daemon startup.
const LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Returns `true` if a live `loom-daemon` is currently listening on
/// `socket_path` and actively servicing requests.
///
/// The probe connects to the socket and performs a `Ping`/`Pong` roundtrip:
///
/// - A connect failure (`ECONNREFUSED`, `ENOENT`, `ENOTSOCK`, permission
///   error, …) means the socket is absent or stale — the file may linger from
///   a crashed daemon but nothing is listening — so it is safe to remove and
///   rebind. Returns `false`.
/// - A successful connect **and** a `Pong` reply confirms a live daemon owns
///   the socket. Returns `true`; the caller must refuse to start rather than
///   unlink the path out from under the incumbent.
///
/// A connect that succeeds but never yields a `Pong` within
/// `LIVENESS_PROBE_TIMEOUT` (e.g. an accept loop wedged before it services
/// requests, or a non-daemon process squatting the path) is treated as "not a
/// live, responsive daemon" and returns `false` — refusing to ever reclaim
/// such a socket would be worse than rebinding it.
async fn socket_has_live_listener(socket_path: &Path) -> bool {
    let stream = match tokio::time::timeout(
        LIVENESS_PROBE_TIMEOUT,
        UnixStream::connect(socket_path),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        // Connect refused/absent, or the connect itself timed out — not a
        // live listener.
        _ => return false,
    };

    let probe = async move {
        let (reader, mut writer) = stream.into_split();
        // Reuse the canonical Ping request shape so the probe stays in sync
        // with the wire protocol.
        let request_json = serde_json::to_string(&Request::Ping).ok()?;
        writer.write_all(request_json.as_bytes()).await.ok()?;
        writer.write_all(b"\n").await.ok()?;
        writer.flush().await.ok()?;

        let mut lines = BufReader::new(reader).lines();
        let line = lines.next_line().await.ok()??;
        let response: Response = serde_json::from_str(&line).ok()?;
        Some(matches!(response, Response::Pong))
    };

    matches!(tokio::time::timeout(LIVENESS_PROBE_TIMEOUT, probe).await, Ok(Some(true)))
}

/// Get the current git branch for a given directory
/// Returns None if not in a git repository or if the command fails
fn get_git_branch(working_dir: Option<&String>) -> Option<String> {
    let dir = working_dir?;

    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .current_dir(dir)
        .output()
        .ok()?;

    if output.status.success() {
        String::from_utf8(output.stdout)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    }
}

pub struct IpcServer {
    socket_path: PathBuf,
    terminal_manager: Arc<Mutex<TerminalManager>>,
    activity_db: Arc<Mutex<ActivityDb>>,
    sweep_registry: Arc<Mutex<SweepRegistry>>,
    event_bus: Arc<EventBus>,
}

impl IpcServer {
    pub fn new(
        socket_path: PathBuf,
        terminal_manager: Arc<Mutex<TerminalManager>>,
        activity_db: Arc<Mutex<ActivityDb>>,
        sweep_registry: Arc<Mutex<SweepRegistry>>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            socket_path,
            terminal_manager,
            activity_db,
            sweep_registry,
            event_bus,
        }
    }

    pub async fn run(&self) -> Result<()> {
        // Singleton guard (#3806): before touching the socket, probe whether a
        // live daemon is already listening on it. Starting a second daemon used
        // to unconditionally `remove_file` + rebind, silently orphaning the
        // incumbent (still running, still holding its children, but with its
        // socket unlinked). Refuse to start in that case; only a genuinely
        // stale/absent socket is removed and rebound below.
        if socket_has_live_listener(&self.socket_path).await {
            anyhow::bail!(
                "another loom-daemon is already listening on {} — refusing to start. \
                 If you intended to replace it, stop the running daemon first \
                 (e.g. `kill <pid>` or its shutdown path) and retry.",
                self.socket_path.display()
            );
        }

        // Remove old socket (best-effort; only reached when no live listener
        // answered the probe above, i.e. the file is stale or absent).
        let _ = fs::remove_file(&self.socket_path).await;

        let listener = UnixListener::bind(&self.socket_path)?;
        log::info!("IPC server listening at {}", self.socket_path.display());

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tm = self.terminal_manager.clone();
                    let db = self.activity_db.clone();
                    let sr = self.sweep_registry.clone();
                    let bus = self.event_bus.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, tm, db, sr, bus).await {
                            log::error!("Client error: {e}");
                        }
                    });
                }
                Err(e) => {
                    log::error!("Accept error: {e}");
                }
            }
        }
    }
}

async fn handle_client(
    stream: UnixStream,
    terminal_manager: Arc<Mutex<TerminalManager>>,
    activity_db: Arc<Mutex<ActivityDb>>,
    sweep_registry: Arc<Mutex<SweepRegistry>>,
    event_bus: Arc<EventBus>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let request: Request = serde_json::from_str(&line)?;
        log::debug!("Request: {request:?}");

        // SubscribeEvents is the only structurally-different request: it
        // returns a stream of `EventStream` frames on the same connection
        // rather than a single response. Once a client subscribes, the
        // connection is dedicated to the stream until the client closes
        // it (or the bus drops).
        if let Request::SubscribeEvents { topics } = request {
            stream_events(&event_bus, &mut writer, topics).await?;
            // After streaming ends (client disconnect or bus closed) the
            // connection has no more useful state — exit the loop.
            break;
        }

        let response =
            handle_request(request, &terminal_manager, &activity_db, &sweep_registry, &event_bus);

        let response_json = serde_json::to_string(&response)?;
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }

    Ok(())
}

/// Stream events from the bus to `writer` as long as the bus is alive
/// and the client connection is open.
///
/// This is the streaming-response path used by `Request::SubscribeEvents`.
/// Each event is encoded as a single `Response::EventStream { events }`
/// frame containing exactly one event (the `events: Vec<Event>` shape
/// gives us room to batch in a future revision without a protocol break).
///
/// Termination: the loop ends when either
///
/// - the bus is dropped (`Subscription::recv` returns `Closed`), or
/// - `writer.write_all` returns an error (the client closed the socket).
async fn stream_events(
    bus: &Arc<EventBus>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    topics: Vec<String>,
) -> Result<()> {
    use crate::event_bus::RecvError;

    let mut subscription = bus.subscribe(topics);

    loop {
        match subscription.recv().await {
            Ok(event) => {
                let frame = Response::EventStream {
                    events: vec![event],
                };
                let frame_json = serde_json::to_string(&frame)?;
                if writer.write_all(frame_json.as_bytes()).await.is_err() {
                    // Client disconnected — gracefully exit.
                    break;
                }
                if writer.write_all(b"\n").await.is_err() {
                    break;
                }
            }
            Err(RecvError::Closed) => {
                log::debug!("event stream: bus closed, ending subscription");
                break;
            }
            Err(RecvError::Empty) => {
                // recv() should never return Empty (it blocks); but if
                // the underlying receiver ever changes semantics, just
                // yield and try again.
                tokio::task::yield_now().await;
            }
        }
    }
    Ok(())
}

// Allow expect_used because mutex poisoning is a panic-level error that indicates
// a thread panicked while holding the lock. This is not recoverable and should crash.
// Allow too_many_lines because this is a central request dispatcher that handles all IPC commands.
#[allow(clippy::expect_used, clippy::too_many_lines)]
fn handle_request(
    request: Request,
    terminal_manager: &Arc<Mutex<TerminalManager>>,
    activity_db: &Arc<Mutex<ActivityDb>>,
    sweep_registry: &Arc<Mutex<SweepRegistry>>,
    event_bus: &Arc<EventBus>,
) -> Response {
    match request {
        Request::Ping => Response::Pong,

        Request::CreateTerminal {
            config_id,
            name,
            working_dir,
            role,
            instance_number,
        } => {
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.create_terminal(&config_id, name, working_dir, role.as_ref(), instance_number)
            {
                Ok(id) => Response::TerminalCreated { id },
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::ListTerminals => {
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            Response::TerminalList {
                terminals: tm.list_terminals(),
            }
        }

        Request::DestroyTerminal { id } => {
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.destroy_terminal(&id) {
                Ok(()) => Response::Success,
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::SendInput { id, data } => {
            // Get terminal info to extract role and workspace context
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");

            let terminal_info = tm.list_terminals().into_iter().find(|t| t.id == id);

            // Extract context from terminal info
            let (agent_role, working_dir, worktree_path) = if let Some(info) = terminal_info {
                (info.role, info.working_dir, info.worktree_path)
            } else {
                (None, None, None)
            };

            // Determine workspace path (prefer worktree, fallback to working_dir)
            let workspace_path = worktree_path.or(working_dir.clone());

            // Capture current git commit before sending input (for change tracking)
            let before_commit = workspace_path
                .as_ref()
                .and_then(|ws| git_utils::get_current_commit(std::path::Path::new(ws)));

            // Get git branch from workspace
            let git_branch = get_git_branch(workspace_path.as_ref());

            // Record input to activity database with full context
            let input = AgentInput {
                id: None,
                terminal_id: id.clone(),
                timestamp: Utc::now(),
                input_type: InputType::Manual, // Default to manual
                content: data.clone(),
                agent_role,
                context: InputContext {
                    workspace: workspace_path,
                    branch: git_branch,
                    ..Default::default()
                },
            };

            let input_id = if let Ok(db) = activity_db.lock() {
                match db.record_input(&input) {
                    Ok(id) => id,
                    Err(e) => {
                        log::warn!("Failed to record input to activity database: {e}");
                        0 // Use 0 as sentinel for failed recording
                    }
                }
            } else {
                0
            };

            // Send input to terminal
            match tm.send_input(&id, &data) {
                Ok(()) => Response::InputSent {
                    input_id,
                    before_commit,
                },
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::GetTerminalOutput { id, start_byte } => {
            use base64::{engine::general_purpose, Engine as _};

            // Get terminal info first (before releasing lock for output)
            let terminal_info = {
                let mut tm = terminal_manager
                    .lock()
                    .expect("Terminal manager mutex poisoned");
                tm.list_terminals().into_iter().find(|t| t.id == id)
            };

            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.get_terminal_output(&id, start_byte) {
                Ok((output_bytes, byte_count)) => {
                    // Record output sample to activity database if there's new data
                    if !output_bytes.is_empty() {
                        let output_str = String::from_utf8_lossy(&output_bytes).to_string();
                        // Take first 1024 characters (not bytes) to avoid slicing multi-byte UTF-8 chars
                        let preview = if output_str.chars().count() > 1024 {
                            output_str.chars().take(1024).collect::<String>()
                        } else {
                            output_str.clone()
                        };

                        let output_record = AgentOutput {
                            id: None,
                            input_id: None, // Could link to last input if tracked
                            terminal_id: id.clone(),
                            timestamp: Utc::now(),
                            content: Some(output_str.clone()),
                            content_preview: Some(preview),
                            exit_code: None,
                            metadata: None,
                        };

                        if let Ok(db) = activity_db.lock() {
                            if let Err(e) = db.record_output(&output_record) {
                                log::warn!("Failed to record output to activity database: {e}");
                            }

                            // Parse terminal output for forge events and record them
                            // TODO: Read forge_host from configuration once #3135 lands
                            let forge_host = "github.com";
                            let forge_events = parse_forge_events(&output_str, forge_host);
                            for parsed_event in forge_events {
                                let prompt_event = parsed_event.to_prompt_forge_event(None);
                                if let Err(e) = db.record_prompt_forge_event(&prompt_event) {
                                    log::warn!("Failed to record forge event: {e}");
                                } else {
                                    log::debug!(
                                        "Recorded forge event: {:?} (issue: {:?}, pr: {:?})",
                                        prompt_event.event_type,
                                        prompt_event.issue_number,
                                        prompt_event.pr_number
                                    );
                                }
                            }

                            // Parse terminal output for resource usage (token counts, costs)
                            match db.record_resource_usage_from_output(None, &output_str, None) {
                                Ok(Some(usage_id)) => {
                                    log::debug!(
                                        "Recorded resource usage (id: {usage_id}) from terminal output"
                                    );
                                }
                                Ok(None) => {
                                    // No resource usage found in output - this is normal
                                }
                                Err(e) => {
                                    log::warn!("Failed to record resource usage: {e}");
                                }
                            }

                            // Parse terminal output for quality metrics (test results, lint errors, build status)
                            // Issue #1054: Track test and quality outcomes
                            match db.record_quality_from_output(0, &output_str) {
                                Ok(Some(metrics_id)) => {
                                    log::debug!(
                                        "Recorded quality metrics (id: {metrics_id}) from terminal output"
                                    );
                                }
                                Ok(None) => {
                                    // No quality metrics found in output - this is normal
                                }
                                Err(e) => {
                                    log::warn!("Failed to record quality metrics: {e}");
                                }
                            }

                            // Parse terminal output for git commits and record changes
                            // This enables automatic prompt-to-commit correlation
                            if git_parser::contains_git_commit(&output_str) {
                                let git_commits = git_parser::parse_git_commits(&output_str);
                                for commit_event in git_commits {
                                    log::info!(
                                        "Detected git commit: {} ({:?})",
                                        commit_event.commit_hash,
                                        commit_event.commit_message
                                    );

                                    // Record the commit correlation if we have the terminal's workspace
                                    if let Some(ref info) = terminal_info {
                                        let workspace_path = info
                                            .worktree_path
                                            .as_ref()
                                            .or(info.working_dir.as_ref());

                                        if let Some(ws) = workspace_path {
                                            // Create a prompt_changes record linking to the commit
                                            // We use the commit hash as after_commit
                                            // The input_id would ideally link to the most recent input
                                            // but we don't have that context here, so we record
                                            // the commit with metrics from the parsed output
                                            let changes = crate::activity::PromptChanges {
                                                id: None,
                                                input_id: 0, // Will be correlated by timestamp
                                                before_commit: None,
                                                after_commit: Some(
                                                    commit_event.commit_hash.clone(),
                                                ),
                                                files_changed: commit_event
                                                    .files_changed
                                                    .unwrap_or(0),
                                                lines_added: commit_event.lines_added.unwrap_or(0),
                                                lines_removed: commit_event
                                                    .lines_removed
                                                    .unwrap_or(0),
                                                tests_added: 0, // Not available from commit output
                                                tests_modified: 0,
                                            };

                                            if let Err(e) = db.record_prompt_changes(&changes) {
                                                log::warn!(
                                                    "Failed to record git commit correlation: {e}"
                                                );
                                            } else {
                                                log::debug!(
                                                    "Recorded git commit {} in workspace {}",
                                                    commit_event.commit_hash,
                                                    ws
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Encode bytes as base64 for JSON transmission
                    let output = general_purpose::STANDARD.encode(&output_bytes);
                    log::debug!(
                        "GetTerminalOutput: {} raw bytes -> {} base64 chars, total byte_count={}",
                        output_bytes.len(),
                        output.len(),
                        byte_count
                    );
                    Response::TerminalOutput { output, byte_count }
                }
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::ResizeTerminal { id, cols, rows } => {
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.resize_terminal(&id, cols, rows) {
                Ok(()) => Response::Success,
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::CheckSessionHealth { id } => {
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.has_tmux_session(&id) {
                Ok(has_session) => Response::SessionHealth { has_session },
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::ListAvailableSessions => {
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            let sessions = tm.list_available_sessions();
            Response::AvailableSessions { sessions }
        }

        Request::AttachToSession { id, session_name } => {
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.attach_to_session(&id, session_name) {
                Ok(()) => Response::Success,
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::KillSession { session_name } => {
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.kill_session(&session_name) {
                Ok(()) => Response::Success,
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::SetWorktreePath { id, worktree_path } => {
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.set_worktree_path(&id, &worktree_path) {
                Ok(()) => Response::Success,
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::GetTerminalActivity { id, limit } => {
            if let Ok(db) = activity_db.lock() {
                match db.get_terminal_activity(&id, limit) {
                    Ok(entries) => Response::TerminalActivity { entries },
                    Err(e) => {
                        log::error!("Failed to get terminal activity: {e}");
                        Response::StructuredError(DaemonError::activity_query_failed(
                            "get terminal activity",
                            &e.to_string(),
                        ))
                    }
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::CaptureGitChanges {
            input_id,
            working_dir,
            before_commit,
        } => {
            let working_path = std::path::Path::new(&working_dir);

            // Capture git changes
            if let Some(changes) =
                git_utils::capture_prompt_changes(working_path, input_id, before_commit)
            {
                // Record to database
                if let Ok(db) = activity_db.lock() {
                    match db.record_prompt_changes(&changes) {
                        Ok(_) => Response::GitChangesCaptured {
                            files_changed: changes.files_changed,
                            lines_added: changes.lines_added,
                            lines_removed: changes.lines_removed,
                        },
                        Err(e) => {
                            log::error!("Failed to record prompt changes: {e}");
                            Response::StructuredError(DaemonError::activity_query_failed(
                                "record prompt changes",
                                &e.to_string(),
                            ))
                        }
                    }
                } else {
                    Response::StructuredError(DaemonError::activity_db_locked())
                }
            } else {
                // No changes detected or not a git repo
                Response::GitChangesCaptured {
                    files_changed: 0,
                    lines_added: 0,
                    lines_removed: 0,
                }
            }
        }

        Request::GetCurrentCommit { working_dir } => {
            let working_path = std::path::Path::new(&working_dir);
            let commit = git_utils::get_current_commit(working_path);
            Response::CurrentCommit { commit }
        }

        // ====================================================================
        // Issue Claim Registry Handlers (Issue #1159)
        // ====================================================================
        Request::ClaimIssue {
            number,
            claim_type,
            terminal_id,
            label,
            agent_role,
            stale_threshold_secs,
        } => {
            if let Ok(db) = activity_db.lock() {
                match db.claim_issue(
                    number,
                    claim_type,
                    &terminal_id,
                    label.as_deref(),
                    agent_role.as_deref(),
                    stale_threshold_secs,
                ) {
                    Ok(result) => Response::ClaimResult(result),
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "claim issue",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::ReleaseClaim {
            number,
            claim_type,
            terminal_id,
        } => {
            if let Ok(db) = activity_db.lock() {
                match db.release_claim(number, claim_type, terminal_id.as_deref()) {
                    Ok(released) => {
                        if released {
                            Response::Success
                        } else {
                            Response::StructuredError(
                                DaemonError::new(
                                    crate::errors::ErrorDomain::Activity,
                                    crate::errors::ErrorCode::ACTIVITY_QUERY_FAILED,
                                    "Claim not found or not owned",
                                )
                                .recoverable(false),
                            )
                        }
                    }
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "release claim",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::HeartbeatClaim {
            number,
            claim_type,
            terminal_id,
        } => {
            if let Ok(db) = activity_db.lock() {
                match db.heartbeat_claim(number, claim_type, &terminal_id) {
                    Ok(updated) => {
                        if updated {
                            Response::Success
                        } else {
                            Response::StructuredError(
                                DaemonError::new(
                                    crate::errors::ErrorDomain::Activity,
                                    crate::errors::ErrorCode::ACTIVITY_QUERY_FAILED,
                                    "Claim not found or not owned",
                                )
                                .recoverable(false),
                            )
                        }
                    }
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "update heartbeat",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::GetClaim { number, claim_type } => {
            if let Ok(db) = activity_db.lock() {
                match db.get_claim(number, claim_type) {
                    Ok(claim) => Response::Claim(claim),
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "get claim",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::GetTerminalClaims { terminal_id } => {
            if let Ok(db) = activity_db.lock() {
                match db.get_claims_by_terminal(&terminal_id) {
                    Ok(claims) => Response::Claims(claims),
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "get terminal claims",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::GetAllClaims => {
            if let Ok(db) = activity_db.lock() {
                match db.get_all_claims() {
                    Ok(claims) => Response::Claims(claims),
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "get all claims",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::GetClaimsSummary {
            stale_threshold_secs,
        } => {
            if let Ok(db) = activity_db.lock() {
                let threshold = stale_threshold_secs.unwrap_or(3600);
                match db.get_claims_summary(threshold) {
                    Ok(summary) => Response::ClaimsSummary(summary),
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "get claims summary",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::ReleaseStaleCliams {
            stale_threshold_secs,
        } => {
            if let Ok(db) = activity_db.lock() {
                let threshold = stale_threshold_secs.unwrap_or(3600);
                match db.release_stale_claims(threshold) {
                    Ok(count) => Response::ClaimsReleased { count },
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "release stale claims",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::ReleaseTerminalClaims { terminal_id } => {
            if let Ok(db) = activity_db.lock() {
                match db.release_terminal_claims(&terminal_id) {
                    Ok(count) => Response::ClaimsReleased { count },
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "release terminal claims",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        // ====================================================================
        // Sweep Registry Handlers (Issue #3452 — Phase A of #3449)
        // ====================================================================
        Request::DispatchSweep {
            kind,
            idempotency_key,
            model,
            effort,
            depends_on,
        } => {
            let mut sr = sweep_registry
                .lock()
                .expect("Sweep registry mutex poisoned");
            match sr.dispatch(
                &kind,
                idempotency_key,
                model.as_deref(),
                effort.as_deref(),
                depends_on,
            ) {
                Ok(outcome) => Response::SweepDispatched {
                    sweep_id: outcome.sweep_id,
                    pid: outcome.pid,
                    token_name: outcome.token_name,
                    log_path: outcome.log_path,
                },
                Err(e) => Response::Error {
                    message: format!("dispatch_sweep failed: {e}"),
                },
            }
        }

        Request::ListSweeps { state_filter } => {
            let sr = sweep_registry
                .lock()
                .expect("Sweep registry mutex poisoned");
            let sweeps = sr.list(state_filter.as_ref());
            Response::SweepList { sweeps }
        }

        // ====================================================================
        // Sweep Monitoring Handlers (Issue #3455 — Phase C of #3449)
        // ====================================================================
        Request::GetSweepStatus { sweep_id } => {
            let sr = sweep_registry
                .lock()
                .expect("Sweep registry mutex poisoned");
            let info = sr.get_status(&sweep_id);
            Response::SweepStatus { info }
        }

        Request::TailSweepLog { sweep_id, lines } => {
            let sr = sweep_registry
                .lock()
                .expect("Sweep registry mutex poisoned");
            match sr.tail_log(&sweep_id, lines) {
                Ok((log_path, lines)) => Response::SweepLogTail {
                    sweep_id,
                    lines,
                    log_path,
                },
                Err(e) => Response::Error {
                    message: format!("tail_sweep_log failed: {e}"),
                },
            }
        }

        Request::CancelSweep {
            sweep_id,
            grace_secs,
        } => {
            let mut sr = sweep_registry
                .lock()
                .expect("Sweep registry mutex poisoned");
            match sr.cancel(&sweep_id, std::time::Duration::from_secs(grace_secs)) {
                Ok(outcome) => Response::SweepCancelled {
                    sweep_id: outcome.sweep_id,
                    pid: outcome.pid,
                    sigkill_sent: outcome.sigkill_sent,
                    was_running: outcome.was_running,
                },
                Err(e) => Response::Error {
                    message: format!("cancel_sweep failed: {e}"),
                },
            }
        }

        // ====================================================================
        // Event Bus Handlers (Issue #3453 — Phase B of #3449)
        // ====================================================================
        Request::PublishEvent { topic, payload } => {
            // Generic publish path used by sweep children — the topic is
            // the canonical name (e.g., "sweep.issue.123.phase") and the
            // payload is opaque JSON. See `defaults/.claude/commands/loom/
            // sweep.md` for the per-topic payload schema.
            let event = Event::Generic {
                topic: topic.clone(),
                payload,
            };
            match event_bus.publish(event) {
                Ok(receivers) => Response::EventPublished { topic, receivers },
                Err(_) => Response::EventPublished {
                    topic,
                    receivers: 0,
                },
            }
        }

        Request::SubscribeEvents { .. } => {
            // SubscribeEvents is intercepted in `handle_client` before it
            // reaches this dispatcher because it requires a streaming
            // response (not a single Response frame). If this branch is
            // ever reached, the IPC server's handle_client logic is bugged
            // — fail loud so it doesn't silently mis-route.
            Response::Error {
                message: "internal: SubscribeEvents must be handled by stream_events, not \
                          handle_request"
                    .to_string(),
            }
        }

        Request::Shutdown => {
            log::info!("Shutdown requested");
            std::process::exit(0);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::activity::ActivityDb;
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use crate::types::{SweepKind, SweepState};
    use tempfile::tempdir;

    fn setup_test_context() -> (
        Arc<Mutex<TerminalManager>>,
        Arc<Mutex<ActivityDb>>,
        Arc<Mutex<SweepRegistry>>,
        Arc<EventBus>,
    ) {
        let tm = Arc::new(Mutex::new(TerminalManager::new()));
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test_activity.db");
        let db = ActivityDb::new(db_path).unwrap();
        let db = Arc::new(Mutex::new(db));
        let mut sr_config = SweepRegistryConfig::new(dir.path().to_path_buf());
        sr_config.skip_label_flip = true;
        let bus = Arc::new(EventBus::new());
        let mut registry = SweepRegistry::new(sr_config);
        registry.set_event_bus(bus.clone());
        let sr = Arc::new(Mutex::new(registry));
        // Keep dir alive so the temp directory isn't deleted
        std::mem::forget(dir);
        (tm, db, sr, bus)
    }

    // ===== Ping/Pong =====

    #[test]
    fn test_handle_request_ping() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(Request::Ping, &tm, &db, &sr, &bus);
        assert!(matches!(response, Response::Pong));
    }

    // ===== ListTerminals =====

    #[test]
    fn test_handle_request_list_terminals_empty() {
        let (tm, db, sr, bus) = setup_test_context();
        // Set LOOM_NO_RESTORE to prevent tmux restore attempts
        std::env::set_var("LOOM_NO_RESTORE", "1");
        let response = handle_request(Request::ListTerminals, &tm, &db, &sr, &bus);
        std::env::remove_var("LOOM_NO_RESTORE");
        match response {
            Response::TerminalList { terminals } => {
                assert!(terminals.is_empty());
            }
            other => panic!("Expected TerminalList, got: {other:?}"),
        }
    }

    // ===== GetCurrentCommit =====

    #[test]
    fn test_handle_request_get_current_commit_nonexistent_dir() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(
            Request::GetCurrentCommit {
                working_dir: "/nonexistent/path".to_string(),
            },
            &tm,
            &db,
            &sr,
            &bus,
        );
        match response {
            Response::CurrentCommit { commit } => {
                assert!(commit.is_none());
            }
            other => panic!("Expected CurrentCommit, got: {other:?}"),
        }
    }

    // ===== GetTerminalActivity =====

    #[test]
    fn test_handle_request_get_terminal_activity_empty() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(
            Request::GetTerminalActivity {
                id: "nonexistent".to_string(),
                limit: 10,
            },
            &tm,
            &db,
            &sr,
            &bus,
        );
        match response {
            Response::TerminalActivity { entries } => {
                assert!(entries.is_empty());
            }
            other => panic!("Expected TerminalActivity, got: {other:?}"),
        }
    }

    // ===== GetAllClaims =====

    #[test]
    fn test_handle_request_get_all_claims_empty() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(Request::GetAllClaims, &tm, &db, &sr, &bus);
        match response {
            Response::Claims(claims) => {
                assert!(claims.is_empty());
            }
            other => panic!("Expected Claims, got: {other:?}"),
        }
    }

    // ===== GetClaimsSummary =====

    #[test]
    fn test_handle_request_get_claims_summary() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(
            Request::GetClaimsSummary {
                stale_threshold_secs: Some(3600),
            },
            &tm,
            &db,
            &sr,
            &bus,
        );
        match response {
            Response::ClaimsSummary(summary) => {
                assert_eq!(summary.total_claims, 0);
            }
            other => panic!("Expected ClaimsSummary, got: {other:?}"),
        }
    }

    // ===== CaptureGitChanges with nonexistent dir =====

    #[test]
    fn test_handle_request_capture_git_changes_no_repo() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(
            Request::CaptureGitChanges {
                input_id: 1,
                working_dir: "/nonexistent/path".to_string(),
                before_commit: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
        );
        match response {
            Response::GitChangesCaptured {
                files_changed,
                lines_added,
                lines_removed,
            } => {
                assert_eq!(files_changed, 0);
                assert_eq!(lines_added, 0);
                assert_eq!(lines_removed, 0);
            }
            other => panic!("Expected GitChangesCaptured, got: {other:?}"),
        }
    }

    // ===== get_git_branch tests =====

    #[test]
    fn test_get_git_branch_none_input() {
        assert!(get_git_branch(None).is_none());
    }

    #[test]
    fn test_get_git_branch_nonexistent_dir() {
        let dir = "/nonexistent/path".to_string();
        assert!(get_git_branch(Some(&dir)).is_none());
    }

    // ===== Sweep registry IPC handlers (Issue #3452) =====

    /// Build a SweepRegistry that won't actually launch real children.
    /// The fixture spawn binary writes its argv to a sibling log and exits
    /// immediately (same pattern as the sweep_registry unit tests).
    fn setup_sweep_registry_in_tempdir(
    ) -> (Arc<Mutex<SweepRegistry>>, tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let scripts_dir = dir.path().join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        let record_log = dir.path().join("ipc-fake-spawn.log");
        let script = format!(
            r#"#!/usr/bin/env bash
echo "argv: $*" >> "{rec}"
exit 0
"#,
            rec = record_log.display()
        );
        std::fs::write(&fake_bin, script).unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.spawn_bin = Some(fake_bin);
        config.skip_label_flip = true;
        let sr = Arc::new(Mutex::new(SweepRegistry::new(config)));
        (sr, dir, record_log)
    }

    #[test]
    fn test_handle_request_list_sweeps_empty() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        let response =
            handle_request(Request::ListSweeps { state_filter: None }, &tm, &db, &sr, &bus);
        match response {
            Response::SweepList { sweeps } => {
                assert!(sweeps.is_empty());
            }
            other => panic!("Expected SweepList, got: {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_handle_request_dispatch_sweep_happy_path() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();

        let response = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(2024),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
        );
        match response {
            Response::SweepDispatched {
                sweep_id,
                pid,
                token_name,
                log_path,
            } => {
                assert!(sweep_id.starts_with("sweep-issue-2024-"));
                assert!(pid > 0);
                assert_eq!(token_name, "unknown");
                assert!(log_path.to_string_lossy().contains("sweep-issue-2024.log"));
            }
            other => panic!("Expected SweepDispatched, got: {other:?}"),
        }

        // Follow-up ListSweeps should see the new entry.
        let response =
            handle_request(Request::ListSweeps { state_filter: None }, &tm, &db, &sr, &bus);
        match response {
            Response::SweepList { sweeps } => {
                assert_eq!(sweeps.len(), 1);
                assert!(matches!(sweeps[0].state, SweepState::Running));
            }
            other => panic!("Expected SweepList, got: {other:?}"),
        }
    }

    #[test]
    fn test_handle_request_dispatch_sweep_rejects_prset_in_phase_a() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();

        let response = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::PrSet(vec![100, 200]),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
        );
        match response {
            Response::Error { message } => {
                assert!(
                    message.contains("PrSet"),
                    "expected PrSet rejection message; got: {message}"
                );
            }
            other => panic!("Expected Error, got: {other:?}"),
        }
    }

    // ===== DispatchSweep serde compat (Issue #3477, Phase 1) =====

    /// A wire payload WITHOUT the `model` field (the pre-#3477 client shape)
    /// must deserialize with `model == None` — `#[serde(default)]` keeps
    /// existing clients compatible.
    #[test]
    fn test_dispatch_sweep_deserializes_without_model_field() {
        let json = r#"{"type":"DispatchSweep","payload":{"kind":{"type":"Issue","value":42},"idempotency_key":null}}"#;
        let request: Request = serde_json::from_str(json).expect("pre-#3477 payload must parse");
        match request {
            Request::DispatchSweep {
                kind,
                idempotency_key,
                model,
                effort,
                depends_on: _,
            } => {
                assert!(matches!(kind, SweepKind::Issue(42)));
                assert!(idempotency_key.is_none());
                assert!(model.is_none(), "absent model field must default to None");
                assert!(effort.is_none(), "absent effort field must default to None");
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_sweep_serde_round_trip_with_model() {
        let request = Request::DispatchSweep {
            kind: SweepKind::Issue(7),
            idempotency_key: Some("key-B".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            effort: None,
            depends_on: None,
        };
        let json = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        match back {
            Request::DispatchSweep {
                kind,
                idempotency_key,
                model,
                effort,
                depends_on: _,
            } => {
                assert!(matches!(kind, SweepKind::Issue(7)));
                assert_eq!(idempotency_key.as_deref(), Some("key-B"));
                assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
                assert!(effort.is_none());
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_sweep_serde_round_trip_without_model() {
        let request = Request::DispatchSweep {
            kind: SweepKind::Issue(8),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
        };
        let json = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        match back {
            Request::DispatchSweep { model, .. } => assert!(model.is_none()),
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    // ===== DispatchSweep serde compat for `effort` (Issue #3716) =====

    /// A wire payload WITHOUT the `effort` field (the pre-#3716 client shape)
    /// must deserialize with `effort == None` — `#[serde(default)]` keeps
    /// existing clients compatible.
    #[test]
    fn test_dispatch_sweep_deserializes_without_effort_field() {
        let json = r#"{"type":"DispatchSweep","payload":{"kind":{"type":"Issue","value":42},"idempotency_key":null,"model":"claude-sonnet-4-6"}}"#;
        let request: Request = serde_json::from_str(json).expect("pre-#3716 payload must parse");
        match request {
            Request::DispatchSweep { model, effort, .. } => {
                assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
                assert!(effort.is_none(), "absent effort field must default to None");
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_sweep_serde_round_trip_with_effort() {
        let request = Request::DispatchSweep {
            kind: SweepKind::Issue(9),
            idempotency_key: Some("key-E".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            effort: Some("xhigh".to_string()),
            depends_on: None,
        };
        let json = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        match back {
            Request::DispatchSweep { model, effort, .. } => {
                assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
                assert_eq!(effort.as_deref(), Some("xhigh"));
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_sweep_serde_round_trip_with_empty_effort() {
        let request = Request::DispatchSweep {
            kind: SweepKind::Issue(10),
            idempotency_key: None,
            model: None,
            effort: Some(String::new()),
            depends_on: None,
        };
        let json = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        match back {
            // Empty string round-trips as-is at the wire layer; normalization
            // to None happens spawn-side (registry) exactly like `model`.
            Request::DispatchSweep { effort, .. } => {
                assert_eq!(effort.as_deref(), Some(""));
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    // ===== DispatchSweep serde compat for `depends_on` (Issue #3729) =====

    /// A wire payload WITHOUT the `depends_on` field (the pre-#3729 client
    /// shape) must deserialize with `depends_on == None` — `#[serde(default)]`
    /// keeps existing clients compatible.
    #[test]
    fn test_dispatch_sweep_deserializes_without_depends_on_field() {
        let json = r#"{"type":"DispatchSweep","payload":{"kind":{"type":"Issue","value":42},"idempotency_key":null,"model":"claude-sonnet-4-6","effort":"xhigh"}}"#;
        let request: Request = serde_json::from_str(json).expect("pre-#3729 payload must parse");
        match request {
            Request::DispatchSweep { depends_on, .. } => {
                assert!(depends_on.is_none(), "absent depends_on must default to None");
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_sweep_serde_round_trip_with_depends_on() {
        let request = Request::DispatchSweep {
            kind: SweepKind::Issue(3725),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: Some(3726),
        };
        let json = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        match back {
            Request::DispatchSweep { depends_on, .. } => {
                assert_eq!(depends_on, Some(3726));
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    // ===== Event bus IPC handlers (Issue #3453, Phase B) =====

    #[tokio::test]
    async fn test_handle_request_publish_event_routes_to_subscribers() {
        let (tm, db, sr, bus) = setup_test_context();
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let response = handle_request(
            Request::PublishEvent {
                topic: "sweep.issue.123.phase".to_string(),
                payload: serde_json::json!({"phase": "builder"}),
            },
            &tm,
            &db,
            &sr,
            &bus,
        );

        match response {
            Response::EventPublished { topic, receivers } => {
                assert_eq!(topic, "sweep.issue.123.phase");
                assert!(receivers >= 1, "expected at least 1 receiver; got {receivers}");
            }
            other => panic!("Expected EventPublished, got: {other:?}"),
        }

        // Subscriber should now see the published event.
        let ev = sub.recv().await.unwrap();
        match ev {
            Event::Generic { topic, payload } => {
                assert_eq!(topic, "sweep.issue.123.phase");
                assert_eq!(payload, serde_json::json!({"phase": "builder"}));
            }
            other => panic!("Expected Generic event, got: {other:?}"),
        }
    }

    // ===== Sweep monitoring IPC handlers (Issue #3455, Phase C) =====

    #[test]
    fn test_handle_request_get_sweep_status_missing() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        let response = handle_request(
            Request::GetSweepStatus {
                sweep_id: "no-such-sweep".to_string(),
            },
            &tm,
            &db,
            &sr,
            &bus,
        );
        match response {
            Response::SweepStatus { info } => assert!(info.is_none()),
            other => panic!("Expected SweepStatus, got: {other:?}"),
        }
    }

    #[test]
    fn test_handle_request_tail_sweep_log_missing_sweep_returns_error() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        let response = handle_request(
            Request::TailSweepLog {
                sweep_id: "no-such-sweep".to_string(),
                lines: 10,
            },
            &tm,
            &db,
            &sr,
            &bus,
        );
        match response {
            Response::Error { message } => {
                assert!(
                    message.contains("unknown sweep_id"),
                    "expected unknown sweep_id; got: {message}"
                );
            }
            other => panic!("Expected Error, got: {other:?}"),
        }
    }

    #[test]
    fn test_handle_request_cancel_sweep_unknown_returns_error() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        let response = handle_request(
            Request::CancelSweep {
                sweep_id: "no-such-sweep".to_string(),
                grace_secs: 1,
            },
            &tm,
            &db,
            &sr,
            &bus,
        );
        match response {
            Response::Error { message } => {
                assert!(
                    message.contains("unknown sweep_id"),
                    "expected unknown sweep_id; got: {message}"
                );
            }
            other => panic!("Expected Error, got: {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_handle_request_get_sweep_status_returns_existing() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();

        // Dispatch a sweep to get a real entry in the registry.
        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(444),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
        );
        let sweep_id = match dispatched {
            Response::SweepDispatched { sweep_id, .. } => sweep_id,
            other => panic!("Expected SweepDispatched, got: {other:?}"),
        };

        let response = handle_request(
            Request::GetSweepStatus {
                sweep_id: sweep_id.clone(),
            },
            &tm,
            &db,
            &sr,
            &bus,
        );
        match response {
            Response::SweepStatus { info } => {
                let info = info.expect("status should be Some");
                assert_eq!(info.sweep_id, sweep_id);
                assert!(matches!(info.kind, SweepKind::Issue(444)));
            }
            other => panic!("Expected SweepStatus, got: {other:?}"),
        }
    }

    // ===== Singleton guard liveness probe (Issue #3806) =====

    #[tokio::test]
    async fn test_socket_has_live_listener_absent_path() {
        // A path that doesn't exist at all → not live.
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope.sock");
        assert!(!socket_has_live_listener(&missing).await);
    }

    #[tokio::test]
    async fn test_socket_has_live_listener_stale_file() {
        // A regular file at the socket path (a crashed daemon's leftover) has
        // nothing listening behind it → not live, safe to remove/rebind.
        let dir = tempdir().unwrap();
        let stale = dir.path().join("stale.sock");
        std::fs::write(&stale, b"").unwrap();
        assert!(!socket_has_live_listener(&stale).await);
    }

    #[tokio::test]
    async fn test_socket_has_live_listener_non_daemon_listener() {
        // A bound UnixListener that never answers Ping (no accept/respond loop)
        // must be treated as NOT a live, responsive daemon so startup can still
        // recover rather than wedging forever.
        let dir = tempdir().unwrap();
        let sock = dir.path().join("silent.sock");
        let _listener = UnixListener::bind(&sock).unwrap();
        // We never accept()/respond, so the Ping/Pong roundtrip times out.
        assert!(!socket_has_live_listener(&sock).await);
    }

    #[tokio::test]
    async fn test_socket_has_live_listener_true_for_ponging_daemon() {
        // Stand up a minimal accept loop that answers Ping with Pong, exactly
        // like the real IPC server, and confirm the probe reports it live.
        let dir = tempdir().unwrap();
        let sock = dir.path().join("live.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                if let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(Request::Ping) = serde_json::from_str::<Request>(&line) {
                        let json = serde_json::to_string(&Response::Pong).unwrap();
                        let _ = writer.write_all(json.as_bytes()).await;
                        let _ = writer.write_all(b"\n").await;
                        let _ = writer.flush().await;
                    }
                }
            }
        });

        assert!(socket_has_live_listener(&sock).await);
        server.abort();
    }

    #[test]
    fn test_handle_request_subscribe_events_short_circuits_to_error() {
        // SubscribeEvents must be handled by stream_events (not the
        // dispatcher). If it ever reaches handle_request, the dispatcher
        // returns an Error sentinel so the bug is visible.
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(
            Request::SubscribeEvents {
                topics: vec!["sweep".to_string()],
            },
            &tm,
            &db,
            &sr,
            &bus,
        );
        match response {
            Response::Error { message } => {
                assert!(
                    message.contains("SubscribeEvents must be handled by stream_events"),
                    "expected internal-bug error message; got: {message}"
                );
            }
            other => panic!("Expected Error sentinel, got: {other:?}"),
        }
    }
}
