use crate::daemon_client::{ActivityEntry, DaemonClient, Request, Response, TerminalInfo};

/// Response struct for `get_terminal_output` command
#[derive(serde::Serialize)]
pub struct TerminalOutput {
    pub output: String,
    pub byte_count: usize,
}

#[tauri::command]
pub async fn create_terminal(
    config_id: String,
    name: String,
    working_dir: Option<String>,
    role: Option<String>,
    instance_number: Option<u32>,
) -> Result<String, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::CreateTerminal {
            config_id,
            name,
            working_dir,
            role,
            instance_number,
        })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::TerminalCreated { id } => Ok(id),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
pub async fn list_terminals() -> Result<Vec<TerminalInfo>, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::ListTerminals)
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::TerminalList { terminals } => Ok(terminals),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
pub async fn destroy_terminal(id: String) -> Result<(), String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::DestroyTerminal { id })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Success => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
pub async fn send_terminal_input(id: String, data: String) -> Result<(), String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::SendInput { id, data })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Success => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
pub async fn get_terminal_output(
    id: String,
    start_byte: Option<usize>,
) -> Result<TerminalOutput, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::GetTerminalOutput { id, start_byte })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::TerminalOutput { output, byte_count } => {
            Ok(TerminalOutput { output, byte_count })
        }
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
pub async fn get_terminal_activity(
    terminal_id: String,
    limit: usize,
) -> Result<Vec<ActivityEntry>, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::GetTerminalActivity {
            id: terminal_id,
            limit,
        })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::TerminalActivity { entries } => Ok(entries),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
pub async fn resize_terminal(id: String, cols: u16, rows: u16) -> Result<(), String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::ResizeTerminal { id, cols, rows })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Success => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
pub async fn check_session_health(id: String) -> Result<bool, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::CheckSessionHealth { id })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::SessionHealth { has_session } => Ok(has_session),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
pub async fn list_available_sessions() -> Result<Vec<String>, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::ListAvailableSessions)
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::AvailableSessions { sessions } => Ok(sessions),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
pub async fn attach_to_session(id: String, session_name: String) -> Result<(), String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::AttachToSession { id, session_name })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Success => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
pub async fn kill_session(session_name: String) -> Result<(), String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::KillSession { session_name })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Success => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
pub async fn set_worktree_path(id: String, worktree_path: String) -> Result<(), String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::SetWorktreePath { id, worktree_path })
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Success => Ok(()),
        Response::Error { message } => Err(message),
        _ => Err("Unexpected response".to_string()),
    }
}

#[tauri::command]
pub async fn kill_all_loom_sessions() -> Result<(), String> {
    use std::process::Command;

    // Get list of all tmux sessions
    let output = Command::new("tmux")
        .args(["-L", "loom", "list-sessions", "-F", "#{session_name}"])
        .output()
        .map_err(|e| format!("Failed to list tmux sessions: {e}"))?;

    if !output.status.success() {
        // If tmux isn't running or no sessions exist, that's okay
        return Ok(());
    }

    let sessions = String::from_utf8_lossy(&output.stdout);

    // Kill each loom session with process tree cleanup
    for session_name in sessions.lines() {
        if session_name.starts_with("loom-") {
            // Kill process tree before destroying session to prevent orphans
            kill_session_process_tree(session_name, true);
            let _ = Command::new("tmux")
                .args(["-L", "loom", "kill-session", "-t", session_name])
                .output();
        }
    }

    // Sweep for any orphaned claude processes
    sweep_orphaned_claude_processes();

    Ok(())
}

/// Kill the process tree rooted at a tmux session's pane processes.
/// Mirrors the logic in `loom-daemon/src/terminal.rs::kill_process_tree`.
fn kill_session_process_tree(session_name: &str, force: bool) {
    use std::process::Command;

    let pane_output = Command::new("tmux")
        .args([
            "-L",
            "loom",
            "list-panes",
            "-t",
            session_name,
            "-F",
            "#{pane_pid}",
        ])
        .output();

    let pane_pids: Vec<String> = match pane_output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(std::string::ToString::to_string)
            .collect(),
        _ => Vec::new(),
    };

    if pane_pids.is_empty() {
        return;
    }

    let mut all_pids: Vec<String> = Vec::new();
    for pane_pid in &pane_pids {
        collect_descendants(pane_pid, &mut all_pids);
        all_pids.push(pane_pid.clone());
    }

    if all_pids.is_empty() {
        return;
    }

    if force {
        for pid in &all_pids {
            let _ = Command::new("kill").args(["-9", pid]).output();
        }
    } else {
        for pid in &all_pids {
            let _ = Command::new("kill").args(["-15", pid]).output();
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        for pid in &all_pids {
            if Command::new("kill")
                .args(["-0", pid])
                .output()
                .is_ok_and(|o| o.status.success())
            {
                let _ = Command::new("kill").args(["-9", pid]).output();
            }
        }
    }
}

/// Recursively collect all descendant PIDs of a given PID (depth-first)
fn collect_descendants(parent_pid: &str, pids: &mut Vec<String>) {
    use std::process::Command;

    if let Ok(output) = Command::new("pgrep").args(["-P", parent_pid]).output() {
        if output.status.success() {
            let children: Vec<String> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(std::string::ToString::to_string)
                .collect();
            for child in &children {
                collect_descendants(child, pids);
                pids.push(child.clone());
            }
        }
    }
}

/// Sweep for orphaned claude processes with no controlling terminal (TTY ??)
fn sweep_orphaned_claude_processes() {
    use std::process::Command;

    // Find claude processes with no controlling terminal
    let ps_output = Command::new("ps").args(["aux"]).output();
    let Ok(output) = ps_output else { return };
    if !output.status.success() {
        return;
    }

    let ps_text = String::from_utf8_lossy(&output.stdout);
    let orphan_pids: Vec<&str> = ps_text
        .lines()
        .filter(|line| line.contains("claude") && !line.contains("grep"))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // ps aux format: USER PID %CPU %MEM VSZ RSS TTY STAT START TIME COMMAND
            if fields.len() > 7 && fields[6] == "??" {
                Some(fields[1])
            } else {
                None
            }
        })
        .collect();

    for pid in orphan_pids {
        let _ = Command::new("kill").args(["-9", pid]).output();
    }
}
