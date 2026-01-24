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

  // Terminal management - uses IDs and classes from ui/terminal-grid.ts
  terminalContainer: "#mini-terminal-row",
  terminalCard: ".terminal-card",
  addTerminalBtn: "#add-terminal-btn",
  terminalSettingsBtn: ".terminal-settings-btn",
  terminalDeleteBtn: ".terminal-delete-btn",
  terminalOutput: "#terminal-output",
  terminalInput: "#terminal-input",

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

  // Primary terminal view
  primaryTerminalContainer: "#primary-terminal",
  primaryTerminalXterm: "#primary-terminal .xterm",

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
  addTerminal: "+",
  save: "Save",
  cancel: "Cancel",
  delete: "Delete",
  confirm: "Confirm",
  startEngine: "Start",
  stopEngine: "Stop",
  factoryReset: "Factory Reset",
  noWorkspace: /No workspace selected|Select a workspace/i,
} as const;
