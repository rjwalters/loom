use crate::activity::ActivityEntry;
use serde::{Deserialize, Serialize};

pub type TerminalId = String;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Request {
    Ping,
    CreateTerminal {
        config_id: String,
        name: String,
        working_dir: Option<String>,
        role: Option<String>,
        instance_number: Option<u32>,
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
    KillSession {
        session_name: String,
    },
    SetWorktreePath {
        id: TerminalId,
        worktree_path: String,
    },
    GetTerminalActivity {
        id: TerminalId,
        limit: usize,
    },
    /// Capture git changes for a specific input
    /// Called after a prompt completes to record code changes
    CaptureGitChanges {
        input_id: i64,
        working_dir: String,
        before_commit: Option<String>,
    },
    /// Get the current git commit hash for a directory
    GetCurrentCommit {
        working_dir: String,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Response {
    Pong,
    TerminalCreated {
        id: TerminalId,
    },
    TerminalList {
        terminals: Vec<TerminalInfo>,
    },
    TerminalOutput {
        output: String,
        byte_count: usize,
    },
    /// Response from `SendInput` with tracking info for git changes
    InputSent {
        input_id: i64,
        before_commit: Option<String>,
    },
    SessionHealth {
        has_session: bool,
    },
    AvailableSessions {
        sessions: Vec<String>,
    },
    TerminalActivity {
        entries: Vec<ActivityEntry>,
    },
    /// Response with current git commit hash
    CurrentCommit {
        commit: Option<String>,
    },
    /// Response with git changes captured
    GitChangesCaptured {
        files_changed: i32,
        lines_added: i32,
        lines_removed: i32,
    },
    Success,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalInfo {
    pub id: TerminalId,
    pub name: String,
    pub tmux_session: String,
    pub working_dir: Option<String>,
    pub created_at: i64,
    // Agent-specific fields
    pub role: Option<String>,
    pub worktree_path: Option<String>,
    pub agent_pid: Option<u32>,
    #[serde(default)]
    pub agent_status: AgentStatus,
    pub last_interval_run: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    #[default]
    NotStarted,
    Initializing,
    Ready,
    Busy,
    WaitingForInput,
    Error,
    Stopped,
}
