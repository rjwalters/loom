use tauri::{CustomMenuItem, Manager, Menu, MenuItem, Submenu};

pub fn build_menu() -> Menu {
    // Build File menu
    let new_terminal =
        CustomMenuItem::new("new_terminal", "New Terminal").accelerator("CmdOrCtrl+T");
    let close_terminal =
        CustomMenuItem::new("close_terminal", "Close Terminal").accelerator("CmdOrCtrl+Shift+W");
    let close_workspace =
        CustomMenuItem::new("close_workspace", "Close Workspace").accelerator("CmdOrCtrl+W");
    let start_workspace =
        CustomMenuItem::new("start_workspace", "Start...").accelerator("CmdOrCtrl+Shift+R");
    let force_start_workspace = CustomMenuItem::new("force_start_workspace", "Force Start")
        .accelerator("CmdOrCtrl+Shift+Alt+R");
    let factory_reset_workspace =
        CustomMenuItem::new("factory_reset_workspace", "Factory Reset...");

    let file_menu = Submenu::new(
        "File",
        Menu::new()
            .add_item(new_terminal)
            .add_item(close_terminal)
            .add_native_item(MenuItem::Separator)
            .add_item(close_workspace)
            .add_item(start_workspace)
            .add_item(force_start_workspace)
            .add_item(factory_reset_workspace)
            .add_native_item(MenuItem::Separator)
            .add_native_item(MenuItem::Quit),
    );

    // Build Edit menu
    let edit_menu = Submenu::new(
        "Edit",
        Menu::new()
            .add_native_item(MenuItem::Cut)
            .add_native_item(MenuItem::Copy)
            .add_native_item(MenuItem::Paste)
            .add_native_item(MenuItem::Separator)
            .add_native_item(MenuItem::SelectAll),
    );

    // Build View menu
    let toggle_theme =
        CustomMenuItem::new("toggle_theme", "Toggle Theme").accelerator("CmdOrCtrl+Shift+T");
    let zoom_in = CustomMenuItem::new("zoom_in", "Zoom In").accelerator("CmdOrCtrl+=");
    let zoom_out = CustomMenuItem::new("zoom_out", "Zoom Out").accelerator("CmdOrCtrl+-");
    let reset_zoom = CustomMenuItem::new("reset_zoom", "Reset Zoom").accelerator("CmdOrCtrl+0");

    let view_menu = Submenu::new(
        "View",
        Menu::new()
            .add_item(toggle_theme)
            .add_native_item(MenuItem::Separator)
            .add_item(zoom_in)
            .add_item(zoom_out)
            .add_item(reset_zoom)
            .add_native_item(MenuItem::Separator)
            .add_native_item(MenuItem::EnterFullScreen),
    );

    // Build Window menu
    let window_menu = Submenu::new(
        "Window",
        Menu::new()
            .add_native_item(MenuItem::Minimize)
            .add_native_item(MenuItem::Zoom),
    );

    // Build Help menu
    let documentation = CustomMenuItem::new("documentation", "Documentation");
    let view_github = CustomMenuItem::new("view_github", "View on GitHub");
    let report_issue = CustomMenuItem::new("report_issue", "Report Issue");
    let daemon_status = CustomMenuItem::new("daemon_status", "Daemon Status...");
    let keyboard_shortcuts =
        CustomMenuItem::new("keyboard_shortcuts", "Keyboard Shortcuts").accelerator("CmdOrCtrl+/");

    let help_menu = Submenu::new(
        "Help",
        Menu::new()
            .add_item(documentation)
            .add_item(view_github)
            .add_item(report_issue)
            .add_native_item(MenuItem::Separator)
            .add_item(daemon_status)
            .add_item(keyboard_shortcuts),
    );

    Menu::new()
        .add_submenu(file_menu)
        .add_submenu(edit_menu)
        .add_submenu(view_menu)
        .add_submenu(window_menu)
        .add_submenu(help_menu)
}

pub fn handle_menu_event(event: &tauri::WindowMenuEvent) {
    let menu_id = event.menu_item_id();

    match menu_id {
        "new_terminal" => {
            let _ = event.window().emit("new-terminal", ());
        }
        "close_terminal" => {
            let _ = event.window().emit("close-terminal", ());
        }
        "close_workspace" => {
            let _ = event.window().emit("close-workspace", ());
        }
        "start_workspace" => {
            let _ = event.window().emit("start-workspace", ());
        }
        "force_start_workspace" => {
            let _ = event.window().emit("force-start-workspace", ());
        }
        "factory_reset_workspace" => {
            let _ = event.window().emit("factory-reset-workspace", ());
        }
        "toggle_theme" => {
            let _ = event.window().emit("toggle-theme", ());
        }
        "zoom_in" => {
            let _ = event.window().emit("zoom-in", ());
        }
        "zoom_out" => {
            let _ = event.window().emit("zoom-out", ());
        }
        "reset_zoom" => {
            let _ = event.window().emit("reset-zoom", ());
        }
        "documentation" => {
            let _ = tauri::api::shell::open(
                &event.window().shell_scope(),
                "https://github.com/rjwalters/loom#readme",
                None,
            );
        }
        "view_github" => {
            let _ = tauri::api::shell::open(
                &event.window().shell_scope(),
                "https://github.com/rjwalters/loom",
                None,
            );
        }
        "report_issue" => {
            let _ = tauri::api::shell::open(
                &event.window().shell_scope(),
                "https://github.com/rjwalters/loom/issues/new",
                None,
            );
        }
        "keyboard_shortcuts" => {
            let _ = event.window().emit("show-keyboard-shortcuts", ());
        }
        "daemon_status" => {
            let _ = event.window().emit("show-daemon-status", ());
        }
        _ => {}
    }
}
