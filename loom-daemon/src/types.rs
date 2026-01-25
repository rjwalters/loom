use crate::activity::{ActivityEntry, ClaimResult, ClaimsSummary, ClaimType, IssueClaim};
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
    // ========================================================================
    // Issue Claim Registry Requests (Issue #1159)
    // ========================================================================
    /// Claim an issue or PR for a terminal
    ClaimIssue {
        number: i32,
        claim_type: ClaimType,
        terminal_id: TerminalId,
        label: Option<String>,
        agent_role: Option<String>,
        /// Stale threshold in seconds (default: 3600 = 1 hour)
        stale_threshold_secs: Option<i64>,
    },
    /// Release a claim on an issue or PR
    ReleaseClaim {
        number: i32,
        claim_type: ClaimType,
        /// Only release if owned by this terminal
        terminal_id: Option<TerminalId>,
    },
    /// Update heartbeat for an active claim
    HeartbeatClaim {
        number: i32,
        claim_type: ClaimType,
        terminal_id: TerminalId,
    },
    /// Get a specific claim
    GetClaim {
        number: i32,
        claim_type: ClaimType,
    },
    /// Get all claims for a terminal
    GetTerminalClaims {
        terminal_id: TerminalId,
    },
    /// Get all active claims
    GetAllClaims,
    /// Get claims summary
    GetClaimsSummary {
        /// Stale threshold in seconds (default: 3600 = 1 hour)
        stale_threshold_secs: Option<i64>,
    },
    /// Release all stale claims (crash recovery)
    ReleaseStaleCliams {
        /// Stale threshold in seconds (default: 3600 = 1 hour)
        stale_threshold_secs: Option<i64>,
    },
    /// Release all claims for a terminal
    ReleaseTerminalClaims {
        terminal_id: TerminalId,
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
    // ========================================================================
    // Issue Claim Registry Responses (Issue #1159)
    // ========================================================================
    /// Result of claiming an issue
    ClaimResult(ClaimResult),
    /// A specific claim
    Claim(Option<IssueClaim>),
    /// List of claims
    Claims(Vec<IssueClaim>),
    /// Claims summary
    ClaimsSummary(ClaimsSummary),
    /// Count of claims released
    ClaimsReleased {
        count: usize,
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
