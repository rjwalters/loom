use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

// Re-export types from daemon (duplicated for now to avoid workspace dependencies)
pub type TerminalId = String;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Request {
    Ping,
    CreateTerminal {
        name: String,
        working_dir: Option<String>,
    },
    ListTerminals,
    DestroyTerminal {
        id: TerminalId,
    },
    SendInput {
        id: TerminalId,
        data: String,
    },
    GetTerminalOutput {
        id: TerminalId,
        start_byte: Option<usize>,
    },
    ResizeTerminal {
        id: TerminalId,
        cols: u16,
        rows: u16,
    },
    CheckSessionHealth {
        id: TerminalId,
    },
    ListAvailableSessions,
    AttachToSession {
        id: TerminalId,
        session_name: String,
    },
    CreateTerminalWithWorktree {
        name: String,
        workspace_path: String,
    },
    CleanupOrphanedWorktrees {
        workspace_path: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Response {
    Pong,
    TerminalCreated { id: TerminalId },
    TerminalList { terminals: Vec<TerminalInfo> },
    TerminalOutput { output: String, byte_count: usize },
    SessionHealth { has_session: bool },
    AvailableSessions { sessions: Vec<String> },
    Success,
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalInfo {
    pub id: TerminalId,
    pub name: String,
    pub tmux_session: String,
    pub working_dir: Option<String>,
    pub created_at: i64,
}

pub struct DaemonClient {
    socket_path: std::path::PathBuf,
}

impl DaemonClient {
    pub fn new() -> Result<Self> {
        let socket_path = dirs::home_dir()
            .ok_or_else(|| anyhow!("No home directory"))?
            .join(".loom/daemon.sock");

        Ok(Self { socket_path })
    }

    pub async fn send_request(&self, request: Request) -> Result<Response> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        let (reader, mut writer) = stream.into_split();

        // Send request
        let json = serde_json::to_string(&request)?;
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;

        // Read response
        let mut lines = BufReader::new(reader).lines();
        let response_line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("No response"))?;

        let response: Response = serde_json::from_str(&response_line)?;
        Ok(response)
    }
}
