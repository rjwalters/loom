/**
 * Shared test selectors for Loom E2E tests.
 *
 * Using data-testid attributes for stable selectors that won't break
 * when CSS classes or text content changes.
 */

export const selectors = {
	// Workspace selection
	workspacePicker: '[data-testid="workspace-picker"]',
	workspaceSelectBtn: '[data-testid="select-workspace-btn"]',
	workspaceName: '[data-testid="workspace-name"]',
	workspaceChangeBtn: '[data-testid="change-workspace-btn"]',

	// Terminal management
	terminalContainer: '[data-testid="terminal-container"]',
	terminalCard: '[data-testid="terminal-card"]',
	addTerminalBtn: '[data-testid="add-terminal-btn"]',
	terminalSettingsBtn: '[data-testid="terminal-settings-btn"]',
	terminalDeleteBtn: '[data-testid="terminal-delete-btn"]',
	terminalOutput: '[data-testid="terminal-output"]',
	terminalInput: '[data-testid="terminal-input"]',

	// Terminal settings modal
	settingsModal: '[data-testid="terminal-settings-modal"]',
	roleSelect: '[data-testid="role-select"]',
	workerTypeSelect: '[data-testid="worker-type-select"]',
	intervalInput: '[data-testid="interval-input"]',
	intervalPromptInput: '[data-testid="interval-prompt-input"]',
	saveSettingsBtn: '[data-testid="save-settings-btn"]',
	cancelSettingsBtn: '[data-testid="cancel-settings-btn"]',

	// Engine controls
	startEngineBtn: '[data-testid="start-engine-btn"]',
	stopEngineBtn: '[data-testid="stop-engine-btn"]',
	engineStatus: '[data-testid="engine-status"]',

	// Factory reset
	factoryResetBtn: '[data-testid="factory-reset-btn"]',
	confirmResetBtn: '[data-testid="confirm-reset-btn"]',
	cancelResetBtn: '[data-testid="cancel-reset-btn"]',

	// Tarot cards
	tarotCard: '[data-testid="tarot-card"]',
	tarotName: '[data-testid="tarot-name"]',

	// Mini terminals row
	miniTerminalRow: '[data-testid="mini-terminal-row"]',
	miniTerminal: '[data-testid="mini-terminal"]',

	// Dialogs and modals
	dialog: '[role="dialog"]',
	confirmDialog: '[data-testid="confirm-dialog"]',
	alertDialog: '[data-testid="alert-dialog"]',
} as const;

/**
 * Text content matchers for elements without data-testid
 */
export const textMatchers = {
	selectWorkspace: "Select Workspace",
	addTerminal: "+",
	save: "Save",
	cancel: "Cancel",
	delete: "Delete",
	confirm: "Confirm",
	startEngine: "Start",
	stopEngine: "Stop",
	factoryReset: "Factory Reset",
} as const;
