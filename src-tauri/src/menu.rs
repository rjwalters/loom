use tauri::{
    menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    Emitter, Manager,
};

pub fn build_menu<R: tauri::Runtime>(
    handle: &impl tauri::Manager<R>,
) -> Result<tauri::menu::Menu<R>, tauri::Error> {
    // Build File menu (simplified - single session model)
    let close_workspace = MenuItemBuilder::new("Close Workspace")
        .id("close_workspace")
        .accelerator("CmdOrCtrl+W")
        .build(handle)?;

    let file_menu = SubmenuBuilder::new(handle, "File")
        .item(&close_workspace)
        .separator()
        .quit()
        .build()?;

    // Build Edit menu
    let edit_menu = SubmenuBuilder::new(handle, "Edit")
        .cut()
        .copy()
        .paste()
        .separator()
        .select_all()
        .build()?;

    // Build View menu
    let toggle_theme = MenuItemBuilder::new("Toggle Theme")
        .id("toggle_theme")
        .accelerator("CmdOrCtrl+Shift+T")
        .build(handle)?;
    let zoom_in = MenuItemBuilder::new("Zoom In")
        .id("zoom_in")
        .accelerator("CmdOrCtrl+=")
        .build(handle)?;
    let zoom_out = MenuItemBuilder::new("Zoom Out")
        .id("zoom_out")
        .accelerator("CmdOrCtrl+-")
        .build(handle)?;
    let reset_zoom = MenuItemBuilder::new("Reset Zoom")
        .id("reset_zoom")
        .accelerator("CmdOrCtrl+0")
        .build(handle)?;
    let show_intelligence_dashboard = MenuItemBuilder::new("Intelligence Dashboard")
        .id("show_intelligence_dashboard")
        .accelerator("CmdOrCtrl+I")
        .build(handle)?;
    let show_budget_management = MenuItemBuilder::new("Budget Management")
        .id("show_budget_management")
        .accelerator("CmdOrCtrl+B")
        .build(handle)?;
    let show_agent_metrics = MenuItemBuilder::new("Agent Metrics")
        .id("show_agent_metrics")
        .accelerator("CmdOrCtrl+M")
        .build(handle)?;
    let show_prompt_library = MenuItemBuilder::new("Prompt Library")
        .id("show_prompt_library")
        .accelerator("CmdOrCtrl+L")
        .build(handle)?;
    let show_comparative_analysis = MenuItemBuilder::new("Comparative Analysis")
        .id("show_comparative_analysis")
        .accelerator("CmdOrCtrl+Shift+A")
        .build(handle)?;
    let show_activity_playback = MenuItemBuilder::new("Activity Playback")
        .id("show_activity_playback")
        .accelerator("CmdOrCtrl+P")
        .build(handle)?;
    let show_health_dashboard = MenuItemBuilder::new("Health Dashboard")
        .id("show_health_dashboard")
        .accelerator("CmdOrCtrl+H")
        .build(handle)?;
    let show_metrics = MenuItemBuilder::new("Telemetry")
        .id("show_metrics")
        .accelerator("CmdOrCtrl+Shift+M")
        .build(handle)?;

    let view_menu = SubmenuBuilder::new(handle, "View")
        .item(&toggle_theme)
        .separator()
        .item(&zoom_in)
        .item(&zoom_out)
        .item(&reset_zoom)
        .separator()
        .item(&show_intelligence_dashboard)
        .item(&show_budget_management)
        .item(&show_agent_metrics)
        .item(&show_prompt_library)
        .item(&show_comparative_analysis)
        .item(&show_activity_playback)
        .item(&show_health_dashboard)
        .item(&show_metrics)
        .separator()
        .fullscreen()
        .build()?;

    // Build Window menu
    let window_menu = SubmenuBuilder::new(handle, "Window")
        .minimize()
        .maximize()
        .build()?;

    // Build Help menu
    let documentation = MenuItemBuilder::new("Documentation")
        .id("documentation")
        .build(handle)?;
    let view_github = MenuItemBuilder::new("View on GitHub")
        .id("view_github")
        .build(handle)?;
    let report_issue = MenuItemBuilder::new("Report Issue")
        .id("report_issue")
        .build(handle)?;
    let daemon_status = MenuItemBuilder::new("Daemon Status...")
        .id("daemon_status")
        .build(handle)?;
    let keyboard_shortcuts = MenuItemBuilder::new("Keyboard Shortcuts")
        .id("keyboard_shortcuts")
        .accelerator("CmdOrCtrl+/")
        .build(handle)?;

    let help_menu = SubmenuBuilder::new(handle, "Help")
        .item(&documentation)
        .item(&view_github)
        .item(&report_issue)
        .separator()
        .item(&daemon_status)
        .item(&keyboard_shortcuts)
        .build()?;

    MenuBuilder::new(handle)
        .item(&file_menu)
        .item(&edit_menu)
        .item(&view_menu)
        .item(&window_menu)
        .item(&help_menu)
        .build()
}

#[allow(clippy::needless_pass_by_value)]
pub fn handle_menu_event<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    event: tauri::menu::MenuEvent,
) {
    let menu_id = event.id().as_ref();

    match menu_id {
        "close_workspace" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("close-workspace", ());
            }
        }
        "toggle_theme" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("toggle-theme", ());
            }
        }
        "zoom_in" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("zoom-in", ());
            }
        }
        "zoom_out" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("zoom-out", ());
            }
        }
        "reset_zoom" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("reset-zoom", ());
            }
        }
        "show_intelligence_dashboard" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("show-intelligence-dashboard", ());
            }
        }
        "show_budget_management" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("show-budget-management", ());
            }
        }
        "show_agent_metrics" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("show-agent-metrics", ());
            }
        }
        "show_health_dashboard" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("show-health-dashboard", ());
            }
        }
        "show_metrics" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("show-metrics", ());
            }
        }
        "show_prompt_library" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("show-prompt-library", ());
            }
        }
        "show_comparative_analysis" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("show-comparative-analysis", ());
            }
        }
        "show_activity_playback" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("show-activity-playback", ());
            }
        }
        "documentation" => {
            let _ = tauri_plugin_opener::OpenerExt::opener(app)
                .open_url("https://github.com/rjwalters/loom#readme", None::<&str>);
        }
        "view_github" => {
            let _ = tauri_plugin_opener::OpenerExt::opener(app)
                .open_url("https://github.com/rjwalters/loom", None::<&str>);
        }
        "report_issue" => {
            let _ = tauri_plugin_opener::OpenerExt::opener(app)
                .open_url("https://github.com/rjwalters/loom/issues/new", None::<&str>);
        }
        "keyboard_shortcuts" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("show-shortcuts", ());
            }
        }
        "daemon_status" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.emit("show-daemon-status", ());
            }
        }
        _ => {}
    }
}
