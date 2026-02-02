/**
 * Shared test selectors for Loom E2E tests.
 *
 * These selectors match the actual UI implementation in src/lib/ui/*.ts
 * and src/main.ts. Update these when the UI changes.
 */

export const selectors = {
  // Workspace selection - uses IDs from main.ts
  workspacePicker: "#workspace-path-container",
  workspaceSelectBtn: "#select-workspace-btn",
  workspaceInput: "#workspace-path-input",
  workspaceName: "#workspace-name",
  workspaceChangeBtn: "#change-workspace-btn",

  // Terminal management - uses IDs and classes from ui/terminal-primary.ts (single-session model)
  terminalContainer: "#terminal-wrapper",
  terminalView: "#terminal-view",
  terminalHeader: "#terminal-header",
  xtermContainers: "#persistent-xterm-containers",
  terminalSettingsBtn: "#terminal-settings-btn",
  terminalCloseBtn: "#terminal-close-btn",
  terminalSearchBtn: "#terminal-search-btn",
  terminalExportBtn: "#terminal-export-btn",
  terminalClearBtn: "#terminal-clear-btn",

  // Terminal settings modal
  settingsModal: "#terminal-settings-modal",
  roleSelect: "#role-select",
  workerTypeSelect: "#worker-type-select",
  intervalInput: "#interval-input",
  intervalPromptInput: "#interval-prompt-input",
  saveSettingsBtn: "#save-settings-btn",
  cancelSettingsBtn: "#cancel-settings-btn",

  // Engine controls - from header.ts
  startEngineBtn: "#start-engine-btn",
  stopEngineBtn: "#stop-engine-btn",
  engineStatus: "#engine-status",

  // Factory reset
  factoryResetBtn: "#factory-reset-btn",
  confirmResetBtn: "#confirm-reset-btn",
  cancelResetBtn: "#cancel-reset-btn",

  // Primary terminal view (same as terminal container in single-session model)
  primaryTerminalContainer: "#terminal-wrapper",
  primaryTerminalXterm: "#persistent-xterm-containers .xterm",

  // Dialogs and modals
  dialog: '[role="dialog"]',
  confirmDialog: ".confirm-dialog",
  alertDialog: ".alert-dialog",

  // App container
  app: "#app",
} as const;

/**
 * Text content matchers for elements without specific IDs
 */
export const textMatchers = {
  selectWorkspace: /Select.*Workspace|Choose.*Workspace|Open.*Workspace/i,
  save: "Save",
  cancel: "Cancel",
  delete: "Delete",
  confirm: "Confirm",
  startEngine: "Start",
  stopEngine: "Stop",
  factoryReset: "Factory Reset",
  noWorkspace: /No workspace selected|Select a workspace/i,
  openRepository: /Open a git repository to begin/i,
} as const;
