use crate::activity::{ActivityDb, AgentInput, InputContext, InputType};
use crate::terminal::TerminalManager;
use crate::types::{Request, Response};
use anyhow::Result;
use chrono::Utc;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub struct IpcServer {
    socket_path: PathBuf,
    terminal_manager: Arc<Mutex<TerminalManager>>,
    activity_db: Arc<Mutex<ActivityDb>>,
}

impl IpcServer {
    pub fn new(
        socket_path: PathBuf,
        terminal_manager: Arc<Mutex<TerminalManager>>,
        activity_db: Arc<Mutex<ActivityDb>>,
    ) -> Self {
        Self {
            socket_path,
            terminal_manager,
            activity_db,
        }
    }

    pub async fn run(&self) -> Result<()> {
        // Remove old socket
        let _ = fs::remove_file(&self.socket_path).await;

        let listener = UnixListener::bind(&self.socket_path)?;
        log::info!("IPC server listening at {}", self.socket_path.display());

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tm = self.terminal_manager.clone();
                    let db = self.activity_db.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, tm, db).await {
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
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let request: Request = serde_json::from_str(&line)?;
        log::debug!("Request: {request:?}");

        let response = handle_request(request, &terminal_manager, &activity_db);

        let response_json = serde_json::to_string(&response)?;
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
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
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }

        Request::ListTerminals => {
            let tm = terminal_manager
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
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }

        Request::SendInput { id, data } => {
            // Record input to activity database
            let input = AgentInput {
                id: None,
                terminal_id: id.clone(),
                timestamp: Utc::now(),
                input_type: InputType::Manual, // Default to manual, could be enhanced later
                content: data.clone(),
                agent_role: None, // Could be populated from terminal metadata
                context: InputContext::default(),
            };

            if let Ok(db) = activity_db.lock() {
                if let Err(e) = db.record_input(&input) {
                    log::warn!("Failed to record input to activity database: {e}");
                }
            }

            // Send input to terminal
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.send_input(&id, &data) {
                Ok(()) => Response::Success,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }

        Request::GetTerminalOutput { id, start_byte } => {
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.get_terminal_output(&id, start_byte) {
                Ok((output_bytes, byte_count)) => {
                    // Encode bytes as base64 for JSON transmission
                    use base64::{engine::general_purpose, Engine as _};
                    let output = general_purpose::STANDARD.encode(&output_bytes);
                    log::debug!(
                        "GetTerminalOutput: {} raw bytes -> {} base64 chars, total byte_count={}",
                        output_bytes.len(),
                        output.len(),
                        byte_count
                    );
                    Response::TerminalOutput { output, byte_count }
                }
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }

        Request::ResizeTerminal { id, cols, rows } => {
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.resize_terminal(&id, cols, rows) {
                Ok(()) => Response::Success,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }

        Request::CheckSessionHealth { id } => {
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.has_tmux_session(&id) {
                Ok(has_session) => Response::SessionHealth { has_session },
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
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
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }

        Request::KillSession { session_name } => {
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.kill_session(&session_name) {
                Ok(()) => Response::Success,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }

        Request::SetWorktreePath { id, worktree_path } => {
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.set_worktree_path(&id, &worktree_path) {
                Ok(()) => Response::Success,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }

        Request::Shutdown => {
            log::info!("Shutdown requested");
            std::process::exit(0);
        }
    }
}
