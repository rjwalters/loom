/**
 * Structured logging system for Loom frontend
 *
 * Provides JSON-formatted logs with consistent structure for easy parsing
 * and debugging across MCP tools and log aggregation systems.
 *
 * @example
 * const logger = Logger.forComponent("worktree-manager");
 * logger.info("Creating worktree", { terminalId, worktreePath });
 * logger.error("Failed to create worktree", error, { terminalId });
 */

export type LogLevel = "INFO" | "WARN" | "ERROR";

/**
 * Context object for structured logging
 * Always includes component, optionally includes IDs and paths
 */
export interface LogContext {
  component: string;
  terminalId?: string;
  workspacePath?: string;
  errorId?: string;
  [key: string]: unknown;
}

/**
 * Structured log entry format
 */
export interface LogEntry {
  timestamp: string;
  level: LogLevel;
  message: string;
  context: LogContext;
}

/**
 * Generates unique error ID for tracking
 */
function generateErrorId(): string {
  return `ERR-${Date.now().toString(36)}-${Math.random().toString(36).substring(2, 7)}`;
}

/**
 * Logger class for structured logging
 */
export class Logger {
  private component: string;

  private constructor(component: string) {
    this.component = component;
  }

  /**
   * Create a logger for a specific component
   */
  static forComponent(component: string): Logger {
    return new Logger(component);
  }

  /**
   * Log a structured message
   */
  private log(level: LogLevel, message: string, context?: Partial<LogContext>): void {
    const entry: LogEntry = {
      timestamp: new Date().toISOString(),
      level,
      message,
      context: {
        component: this.component,
        ...context,
      },
    };

    // Output as JSON for structured parsing
    const json = JSON.stringify(entry);

    // Also use appropriate console method for browser DevTools
    switch (level) {
      case "INFO":
        console.log(json);
        break;
      case "WARN":
        console.warn(json);
        break;
      case "ERROR":
        console.error(json);
        break;
    }
  }

  /**
   * Log informational message
   */
  info(message: string, context?: Partial<LogContext>): void {
    this.log("INFO", message, context);
  }

  /**
   * Log warning message
   */
  warn(message: string, context?: Partial<LogContext>): void {
    this.log("WARN", message, context);
  }

  /**
   * Log error message with Error object
   */
  error(message: string, error: Error | unknown, context?: Partial<LogContext>): void {
    const errorId = generateErrorId();

    // Extract error details
    const errorDetails =
      error instanceof Error
        ? {
            errorMessage: error.message,
            errorStack: error.stack,
          }
        : {
            errorMessage: String(error),
          };

    this.log("ERROR", message, {
      ...context,
      errorId,
      ...errorDetails,
    });
  }
}
