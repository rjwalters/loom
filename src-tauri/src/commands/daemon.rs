use crate::daemon_client::{DaemonClient, Request, Response};

/// Daemon status response
#[derive(serde::Serialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub socket_path: String,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn check_daemon_health() -> Result<bool, String> {
    let client = DaemonClient::new().map_err(|e| e.to_string())?;
    let response = client
        .send_request(Request::Ping)
        .await
        .map_err(|e| e.to_string())?;

    match response {
        Response::Pong => Ok(true),
        _ => Ok(false),
    }
}

#[tauri::command]
pub async fn get_daemon_status() -> DaemonStatus {
    let socket_path = dirs::home_dir()
        .map(|h| h.join(".loom/loom-daemon.sock"))
        .map_or_else(|| "Unknown".to_string(), |p| p.display().to_string());

    match DaemonClient::new() {
        Ok(client) => match client.send_request(Request::Ping).await {
            Ok(Response::Pong) => DaemonStatus {
                running: true,
                socket_path,
                error: None,
            },
            Ok(_) => DaemonStatus {
                running: false,
                socket_path,
                error: Some("Daemon responded with unexpected response".to_string()),
            },
            Err(e) => DaemonStatus {
                running: false,
                socket_path,
                error: Some(format!("Failed to ping daemon: {e}")),
            },
        },
        Err(e) => DaemonStatus {
            running: false,
            socket_path,
            error: Some(format!("Failed to create client: {e}")),
        },
    }
}
