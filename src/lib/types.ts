/**
 * Represents the operational status of a terminal.
 * Used to track the current state of terminal activity.
 */
export enum TerminalStatus {
  /** Terminal is idle and ready for new tasks */
  Idle = "idle",
  /** Terminal is actively executing a task */
  Busy = "busy",
  /** Terminal is waiting for user input */
  NeedsInput = "needs_input",
  /** Terminal has encountered an error */
  Error = "error",
  /** Terminal has been stopped */
  Stopped = "stopped",
}

/**
 * Represents the status of an AI agent running in a terminal.
 * Tracks the agent's lifecycle from initialization through execution.
 */
export enum AgentStatus {
  /** Agent has not been started yet */
  NotStarted = "not_started",
  /** Agent is initializing (spawning process, loading context) */
  Initializing = "initializing",
  /** Agent is ready and waiting for work */
  Ready = "ready",
  /** Agent is actively working on a task */
  Busy = "busy",
  /** Agent is waiting for user input */
  WaitingForInput = "waiting_for_input",
  /** Agent has encountered an error */
  Error = "error",
  /** Agent has been stopped */
  Stopped = "stopped",
}

/**
 * Defines a custom color theme for a terminal.
 * Used to personalize terminal appearance beyond preset themes.
 */
export interface ColorTheme {
  /** Display name of the theme */
  name: string;
  /** Primary color (hex or CSS color) */
  primary: string;
  /** Optional background color (hex or CSS color) */
  background?: string;
  /** Border color (hex or CSS color) */
  border: string;
}

/**
 * Represents a pending input request from an AI agent.
 * Agents can queue multiple input requests when they need user interaction.
 */
export interface InputRequest {
  /** Unique identifier for this input request */
  id: string;
  /** The question or prompt from the agent */
  prompt: string;
  /** Unix timestamp (ms) when the request was created */
  timestamp: number;
}

/**
 * Represents a single activity entry (input + output) from terminal history.
 * Used for displaying terminal activity timeline in the activity modal.
 */
export interface ActivityEntry {
  /** Unique database ID for this input */
  inputId: number;
  /** ISO 8601 timestamp of when the input was sent */
  timestamp: string;
  /** Type of input: manual, autonomous, system, or user_instruction */
  inputType: "manual" | "autonomous" | "system" | "user_instruction";
  /** The full prompt/command text */
  prompt: string;
  /** Agent role at time of input (e.g., "builder", "judge") */
  agentRole: string | null;
  /** Git branch at time of input */
  gitBranch: string | null;
  /** Preview of terminal output (first ~1KB) */
  outputPreview: string | null;
  /** Exit code from command (0 = success, non-zero = error) */
  exitCode: number | null;
  /** ISO 8601 timestamp of output (if captured) */
  outputTimestamp: string | null;
}

/**
 * Represents a terminal instance in the application.
 * Terminals can be plain shells or AI agents with specialized roles.
 */
export interface Terminal {
  /** Stable terminal identifier (e.g., "terminal-1") used for both config and runtime */
  id: string;
  /** User-friendly display name */
  name: string;
  /** Current operational status */
  status: TerminalStatus;
  /** Whether this terminal is currently selected as the primary view */
  isPrimary: boolean;
  /** Optional role identifier (e.g., "claude-code-worker"). Undefined = plain shell */
  role?: string;
  /** Role-specific configuration data (e.g., system prompt, worker type) */
  roleConfig?: Record<string, unknown>;
  /** Flag indicating the tmux session is missing (used in error recovery) */
  missingSession?: boolean;
  /** Theme identifier (e.g., "ocean", "forest") or "default" */
  theme?: string;
  /** Custom color theme configuration */
  customTheme?: ColorTheme;
  /** Path to git worktree (automatically created at .loom/worktrees/{id}) */
  worktreePath?: string;
  /** Process ID of the running agent */
  agentPid?: number;
  /** Current agent lifecycle status */
  agentStatus?: AgentStatus;
  /** Unix timestamp (ms) of the last autonomous interval execution */
  lastIntervalRun?: number;
  /** Queue of pending input requests from the agent */
  pendingInputRequests?: InputRequest[];
  /** Total milliseconds spent in busy state (for analytics) */
  busyTime?: number;
  /** Total milliseconds spent in idle state (for analytics) */
  idleTime?: number;
  /** Unix timestamp (ms) of the last status change */
  lastStateChange?: number;
}

/**
 * Type guard to check if a value is a valid Terminal.
 * Useful for filtering and narrowing types in array operations.
 */
export function isValidTerminal(t: Terminal | null | undefined): t is Terminal {
  return t !== null && t !== undefined && typeof t.id === "string" && t.id.length > 0;
}
