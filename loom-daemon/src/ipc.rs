use crate::terminal::TerminalManager;
use crate::types::{Request, Response};
use anyhow::Result;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub struct IpcServer {
    socket_path: PathBuf,
    terminal_manager: Arc<Mutex<TerminalManager>>,
}

impl IpcServer {
    pub fn new(socket_path: PathBuf, terminal_manager: Arc<Mutex<TerminalManager>>) -> Self {
        Self {
            socket_path,
            terminal_manager,
        }
    }

    pub async fn run(&self) -> Result<()> {
        // Remove old socket
        let _ = fs::remove_file(&self.socket_path).await;

        let listener = UnixListener::bind(&self.socket_path)?;
        log::info!("IPC server listening at {:?}", self.socket_path);

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tm = self.terminal_manager.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, tm).await {
                            log::error!("Client error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    log::error!("Accept error: {}", e);
                }
            }
        }
    }
}

async fn handle_client(
    stream: UnixStream,
    terminal_manager: Arc<Mutex<TerminalManager>>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let request: Request = serde_json::from_str(&line)?;
        log::debug!("Request: {:?}", request);

        let response = handle_request(request, &terminal_manager);

        let response_json = serde_json::to_string(&response)?;
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }

    Ok(())
}

fn handle_request(
    request: Request,
    terminal_manager: &Arc<Mutex<TerminalManager>>,
) -> Response {
    match request {
        Request::Ping => Response::Pong,

        Request::CreateTerminal { name, working_dir } => {
            let mut tm = terminal_manager.lock().unwrap();
            match tm.create_terminal(name, working_dir) {
                Ok(id) => Response::TerminalCreated { id },
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }

        Request::ListTerminals => {
            let tm = terminal_manager.lock().unwrap();
            Response::TerminalList {
                terminals: tm.list_terminals(),
            }
        }

        Request::DestroyTerminal { id } => {
            let mut tm = terminal_manager.lock().unwrap();
            match tm.destroy_terminal(&id) {
                Ok(_) => Response::Success,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }

        Request::SendInput { id, data } => {
            let tm = terminal_manager.lock().unwrap();
            match tm.send_input(&id, &data) {
                Ok(_) => Response::Success,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }

        Request::GetTerminalOutput { id, start_line } => {
            let tm = terminal_manager.lock().unwrap();
            match tm.get_terminal_output(&id, start_line) {
                Ok((output, line_count)) => Response::TerminalOutput { output, line_count },
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
