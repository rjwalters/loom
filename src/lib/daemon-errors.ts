/**
 * daemon-errors.ts - Structured error handling for daemon IPC
 *
 * This module provides TypeScript types and utilities for handling structured
 * errors from the Loom daemon. It corresponds to the Rust errors.rs module
 * and enables smart error handling on the frontend.
 *
 * Features:
 * - Typed error domains for categorization
 * - Error codes for programmatic handling
 * - Recovery hints for automation/agents
 * - Integration with circuit breaker pattern
 *
 * @see circuit-breaker.ts for integration with IPC resilience
 * @see health-monitor.ts for daemon connectivity monitoring
 */

/**
 * Error domains categorize errors by their source and handling strategy.
 *
 * Each domain maps to a specific subsystem and has implications for:
 * - Whether retries make sense
 * - Circuit breaker behavior
 * - User-facing messaging
 * - Escalation paths
 */
export enum ErrorDomain {
  /** tmux server/session errors (server not running, session missing, pipe failures) */
  Tmux = "tmux",

  /** IPC communication errors (socket unreachable, protocol mismatch, timeout) */
  Ipc = "ipc",

  /** Git/worktree errors (dirty state, conflicts, worktree creation failures) */
  Git = "git",

  /** File/directory access errors (permission denied, not found, disk full) */
  Filesystem = "filesystem",

  /** Configuration parsing/validation errors (invalid JSON, missing fields) */
  Configuration = "configuration",

  /** Activity database errors (sqlite failures, schema migration issues) */
  Activity = "activity",

  /** Terminal management errors (invalid ID, terminal not found, state inconsistency) */
  Terminal = "terminal",

  /** Internal errors (logic errors, unexpected state, assertion failures) */
  Internal = "internal",
}

/**
 * Well-known error codes for programmatic handling.
 *
 * Format: DOMAIN_SPECIFIC_ERROR (e.g., TMUX_NO_SERVER, GIT_DIRTY_WORKTREE)
 * These codes are stable and can be used for programmatic error handling.
 */
export const ErrorCodes = {
  // Tmux error codes
  TMUX_NO_SERVER: "TMUX_NO_SERVER",
  TMUX_SESSION_NOT_FOUND: "TMUX_SESSION_NOT_FOUND",
  TMUX_SESSION_EXISTS: "TMUX_SESSION_EXISTS",
  TMUX_PIPE_FAILED: "TMUX_PIPE_FAILED",
  TMUX_COMMAND_FAILED: "TMUX_COMMAND_FAILED",

  // IPC error codes
  IPC_CONNECTION_FAILED: "IPC_CONNECTION_FAILED",
  IPC_TIMEOUT: "IPC_TIMEOUT",
  IPC_PROTOCOL_ERROR: "IPC_PROTOCOL_ERROR",
  IPC_SERIALIZATION_FAILED: "IPC_SERIALIZATION_FAILED",

  // Git error codes
  GIT_WORKTREE_EXISTS: "GIT_WORKTREE_EXISTS",
  GIT_WORKTREE_NOT_FOUND: "GIT_WORKTREE_NOT_FOUND",
  GIT_DIRTY_STATE: "GIT_DIRTY_STATE",
  GIT_MERGE_CONFLICT: "GIT_MERGE_CONFLICT",
  GIT_COMMAND_FAILED: "GIT_COMMAND_FAILED",
  GIT_NOT_REPOSITORY: "GIT_NOT_REPOSITORY",

  // Filesystem error codes
  FS_NOT_FOUND: "FS_NOT_FOUND",
  FS_PERMISSION_DENIED: "FS_PERMISSION_DENIED",
  FS_ALREADY_EXISTS: "FS_ALREADY_EXISTS",
  FS_IO_ERROR: "FS_IO_ERROR",

  // Configuration error codes
  CONFIG_INVALID_JSON: "CONFIG_INVALID_JSON",
  CONFIG_MISSING_FIELD: "CONFIG_MISSING_FIELD",
  CONFIG_INVALID_VALUE: "CONFIG_INVALID_VALUE",
  CONFIG_FILE_NOT_FOUND: "CONFIG_FILE_NOT_FOUND",

  // Activity database error codes
  ACTIVITY_DB_LOCKED: "ACTIVITY_DB_LOCKED",
  ACTIVITY_DB_CORRUPTED: "ACTIVITY_DB_CORRUPTED",
  ACTIVITY_QUERY_FAILED: "ACTIVITY_QUERY_FAILED",
  ACTIVITY_SCHEMA_ERROR: "ACTIVITY_SCHEMA_ERROR",

  // Terminal error codes
  TERMINAL_NOT_FOUND: "TERMINAL_NOT_FOUND",
  TERMINAL_INVALID_ID: "TERMINAL_INVALID_ID",
  TERMINAL_ALREADY_EXISTS: "TERMINAL_ALREADY_EXISTS",
  TERMINAL_STATE_ERROR: "TERMINAL_STATE_ERROR",

  // Internal error codes
  INTERNAL_MUTEX_POISONED: "INTERNAL_MUTEX_POISONED",
  INTERNAL_UNEXPECTED_STATE: "INTERNAL_UNEXPECTED_STATE",
  INTERNAL_ASSERTION_FAILED: "INTERNAL_ASSERTION_FAILED",
} as const;

export type ErrorCode = (typeof ErrorCodes)[keyof typeof ErrorCodes] | string;

/**
 * Structured daemon error that deserializes from Rust DaemonError.
 *
 * This replaces the simple `{ type: "Error", payload: { message: string } }` response
 * with rich, actionable error information.
 */
export interface DaemonError {
  /** The error domain (categorizes the error source) */
  domain: ErrorDomain;

  /** Stable error code for programmatic handling */
  code: string;

  /** Human-readable error message */
  message: string;

  /** Whether the error is potentially recoverable with retry */
  recoverable: boolean;

  /** Optional additional context (e.g., file paths, terminal IDs) */
  details?: Record<string, unknown>;

  /** Optional hint for recovery (useful for agents/automation) */
  recovery_hint?: string;
}

/**
 * Type guard to check if an unknown value is a DaemonError.
 *
 * Use this to distinguish between legacy string errors and structured errors.
 */
export function isDaemonError(error: unknown): error is DaemonError {
  if (typeof error !== "object" || error === null) {
    return false;
  }

  const obj = error as Record<string, unknown>;
  return (
    typeof obj.domain === "string" &&
    typeof obj.code === "string" &&
    typeof obj.message === "string" &&
    typeof obj.recoverable === "boolean"
  );
}

/**
 * Custom error class wrapping a DaemonError for throw/catch patterns.
 *
 * Usage:
 * ```typescript
 * try {
 *   const result = await someOperation();
 *   if (isDaemonError(result)) {
 *     throw new DaemonErrorException(result);
 *   }
 * } catch (error) {
 *   if (error instanceof DaemonErrorException) {
 *     console.log(error.daemonError.recovery_hint);
 *   }
 * }
 * ```
 */
export class DaemonErrorException extends Error {
  public readonly daemonError: DaemonError;

  constructor(daemonError: DaemonError) {
    super(`[${daemonError.domain}:${daemonError.code}] ${daemonError.message}`);
    this.name = "DaemonErrorException";
    this.daemonError = daemonError;
  }

  /** Get the error domain */
  get domain(): ErrorDomain {
    return this.daemonError.domain;
  }

  /** Get the error code */
  get code(): string {
    return this.daemonError.code;
  }

  /** Check if the error is recoverable */
  get recoverable(): boolean {
    return this.daemonError.recoverable;
  }

  /** Get the recovery hint, if any */
  get recoveryHint(): string | undefined {
    return this.daemonError.recovery_hint;
  }

  /** Get error details, if any */
  get details(): Record<string, unknown> | undefined {
    return this.daemonError.details;
  }
}

/**
 * Returns true if errors in this domain are typically recoverable with retry.
 */
export function isDomainRecoverable(domain: ErrorDomain): boolean {
  switch (domain) {
    case ErrorDomain.Tmux:
    case ErrorDomain.Ipc:
    case ErrorDomain.Git:
    case ErrorDomain.Filesystem:
      return true;
    case ErrorDomain.Configuration:
    case ErrorDomain.Activity:
    case ErrorDomain.Terminal:
    case ErrorDomain.Internal:
      return false;
    default:
      return false;
  }
}

/**
 * Returns the recommended retry delay for a domain in milliseconds.
 */
export function getDefaultRetryDelay(domain: ErrorDomain): number {
  switch (domain) {
    case ErrorDomain.Tmux:
      return 2000; // tmux server might be restarting
    case ErrorDomain.Ipc:
      return 1000; // transient network issues
    case ErrorDomain.Git:
      return 500; // might be mid-operation
    case ErrorDomain.Filesystem:
      return 1000; // might be temp file locks
    case ErrorDomain.Activity:
      return 100; // sqlite lock contention
    case ErrorDomain.Terminal:
      return 500; // state synchronization
    case ErrorDomain.Configuration:
      return 0; // no retry, fix config
    case ErrorDomain.Internal:
      return 0; // no retry, need fix
    default:
      return 1000;
  }
}

/**
 * Extracts a DaemonError from various response formats.
 *
 * Handles both:
 * - New structured errors: `{ type: "StructuredError", payload: DaemonError }`
 * - Legacy string errors: `{ type: "Error", payload: { message: string } }`
 *
 * @param response - The IPC response to check
 * @returns The extracted DaemonError, or null if not an error response
 */
export function extractDaemonError(response: unknown): DaemonError | null {
  if (typeof response !== "object" || response === null) {
    return null;
  }

  const obj = response as Record<string, unknown>;

  // Check for new structured error format
  if (obj.type === "StructuredError" && isDaemonError(obj.payload)) {
    return obj.payload;
  }

  // Check for legacy error format - convert to structured
  if (obj.type === "Error" && typeof obj.payload === "object" && obj.payload !== null) {
    const payload = obj.payload as Record<string, unknown>;
    if (typeof payload.message === "string") {
      return convertLegacyError(payload.message);
    }
  }

  return null;
}

/**
 * Converts a legacy error message to a structured DaemonError.
 *
 * This provides backwards compatibility while we migrate to structured errors.
 */
export function convertLegacyError(message: string): DaemonError {
  // Try to categorize based on error message patterns (mirrors Rust logic)
  if (message.includes("no server running")) {
    return {
      domain: ErrorDomain.Tmux,
      code: ErrorCodes.TMUX_NO_SERVER,
      message,
      recoverable: true,
      recovery_hint:
        "The tmux server may need to be started. Try running `tmux -L loom new-session` or restart the Loom daemon.",
    };
  }

  if (message.includes("no such session") || message.includes("session not found")) {
    return {
      domain: ErrorDomain.Tmux,
      code: ErrorCodes.TMUX_SESSION_NOT_FOUND,
      message,
      recoverable: true,
      recovery_hint: "The terminal session may have been killed. Try restarting the terminal.",
    };
  }

  if (message.includes("Terminal not found")) {
    return {
      domain: ErrorDomain.Terminal,
      code: ErrorCodes.TERMINAL_NOT_FOUND,
      message,
      recoverable: false,
    };
  }

  if (message.includes("Invalid terminal ID")) {
    return {
      domain: ErrorDomain.Terminal,
      code: ErrorCodes.TERMINAL_INVALID_ID,
      message,
      recoverable: false,
    };
  }

  if (message.includes("mutex") && message.includes("poison")) {
    return {
      domain: ErrorDomain.Internal,
      code: ErrorCodes.INTERNAL_MUTEX_POISONED,
      message,
      recoverable: false,
      recovery_hint: "This indicates a serious internal error. Restart the daemon.",
    };
  }

  if (message.includes("Database lock") || message.includes("database is locked")) {
    return {
      domain: ErrorDomain.Activity,
      code: ErrorCodes.ACTIVITY_DB_LOCKED,
      message,
      recoverable: true,
      recovery_hint: "The database may be busy. Wait briefly and retry.",
    };
  }

  // Default: treat as internal error
  return {
    domain: ErrorDomain.Internal,
    code: ErrorCodes.INTERNAL_UNEXPECTED_STATE,
    message,
    recoverable: false,
  };
}

/**
 * Checks if an error should trigger circuit breaker behavior.
 *
 * Returns true for errors that indicate systemic issues (daemon down, tmux dead)
 * vs transient issues (single terminal missing).
 */
export function shouldTripCircuitBreaker(error: DaemonError): boolean {
  // Systemic issues that affect all operations
  if (error.domain === ErrorDomain.Tmux && error.code === ErrorCodes.TMUX_NO_SERVER) {
    return true;
  }

  if (error.domain === ErrorDomain.Ipc) {
    return true;
  }

  if (error.domain === ErrorDomain.Internal) {
    return true;
  }

  // Individual terminal/resource issues don't trip the breaker
  return false;
}

/**
 * Gets a user-friendly error title for display.
 */
export function getErrorTitle(error: DaemonError): string {
  switch (error.domain) {
    case ErrorDomain.Tmux:
      return "Terminal System Error";
    case ErrorDomain.Ipc:
      return "Communication Error";
    case ErrorDomain.Git:
      return "Git Error";
    case ErrorDomain.Filesystem:
      return "File System Error";
    case ErrorDomain.Configuration:
      return "Configuration Error";
    case ErrorDomain.Activity:
      return "Database Error";
    case ErrorDomain.Terminal:
      return "Terminal Error";
    case ErrorDomain.Internal:
      return "Internal Error";
    default:
      return "Error";
  }
}
