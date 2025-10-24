/**
 * terminal-state-parser.ts - Passive terminal state detection
 *
 * This module provides passive detection of terminal state by parsing existing
 * terminal output instead of sending probe commands. This approach is:
 * - Non-intrusive: No extra commands sent to terminals
 * - Real-time: Can detect state changes immediately from output
 * - No agent cooperation needed: Works by observing terminal patterns
 * - More informative: Can detect multiple states (working, paused, waiting, etc.)
 *
 * Observable Patterns:
 * - Claude Code bypass prompt: "WARNING: Claude Code running in Bypass Permissions mode"
 * - Claude Code ready: "⏺" (record symbol)
 * - Claude Code paused: "⏸" (pause symbol)
 * - Shell prompts: "$", "%", "#", "bash-5.2$", etc.
 * - Agent working: Natural language responses
 *
 * This replaces the active probe system from terminal-probe.ts which sent
 * bash comments to detect terminal type.
 */

import { Logger } from "./logger";

const logger = Logger.forComponent("terminal-state-parser");

/**
 * Terminal type classification
 */
export type TerminalType = "shell" | "claude-code" | "codex" | "unknown";

/**
 * Terminal status based on observed output
 */
export type TerminalStatus =
  | "idle" // Shell at prompt, waiting for input
  | "waiting-input" // Agent ready and waiting for user input (⏺)
  | "working" // Agent actively processing a task
  | "bypass-prompt" // Claude Code showing bypass permissions warning
  | "paused" // Agent paused (⏸)
  | "unknown"; // Cannot determine state from output

/**
 * Parsed terminal state from output analysis
 */
export interface TerminalState {
  /** The detected terminal type */
  type: TerminalType;
  /** The current status/state */
  status: TerminalStatus;
  /** The last visible prompt text (if any) */
  lastPrompt?: string;
  /** Raw terminal output that was analyzed */
  raw: string;
}

/**
 * Parse terminal output to detect current state
 *
 * This function analyzes terminal output looking for observable patterns
 * that indicate what type of process is running and what state it's in.
 *
 * Detection is performed in priority order:
 * 1. Claude Code bypass permissions prompt (highest priority - needs immediate action)
 * 2. Claude Code ready state (⏺ symbol indicates waiting for input)
 * 3. Claude Code paused state (⏸ symbol)
 * 4. Claude Code working state (natural language output)
 * 5. Shell prompt patterns (various shell types)
 * 6. Unknown (cannot determine)
 *
 * @param output - Raw terminal output to analyze (typically last 20-50 lines)
 * @returns Parsed terminal state with type, status, and metadata
 */
export function parseTerminalState(output: string): TerminalState {
  const trimmed = output.trim();
  // Remove ANSI escape codes for cleaner pattern matching
  // ANSI codes look like: \x1b[...m or \u001b[...m
  // Use dynamic regex construction to avoid biome noControlCharactersInRegex lint error
  const escapeChar = String.fromCharCode(27); // ESC character (\x1b or \u001b)
  const ansiPattern = new RegExp(`${escapeChar}\\[[0-9;]*m`, "g");
  const cleaned = trimmed.replace(ansiPattern, "");

  logger.info("Parsing terminal state", {
    outputLength: output.length,
    trimmedLength: trimmed.length,
    cleanedLength: cleaned.length,
    outputPreview: cleaned.substring(0, 100).replace(/\n/g, "\\n"),
  });

  // Priority 1: Check for Claude Code bypass permissions prompt
  // This is highest priority because it requires immediate action (send "2")
  if (/WARNING:.*Bypass Permissions mode/i.test(cleaned)) {
    logger.info("Detected Claude Code bypass prompt", { outputPreview: cleaned.substring(0, 200) });
    return {
      type: "claude-code",
      status: "bypass-prompt",
      raw: output,
    };
  }

  // Priority 2: Check for Claude Code paused state (⏸ character)
  if (/⏸/.test(cleaned)) {
    logger.info("Detected Claude Code paused state");
    return {
      type: "claude-code",
      status: "paused",
      raw: output,
    };
  }

  // Priority 3: Check for Codex patterns BEFORE working patterns
  // Codex has different output patterns than Claude Code
  // Check this early to avoid false positives with working patterns
  if (/\[Codex\]/i.test(cleaned)) {
    logger.info("Detected Codex terminal");
    // Determine Codex status based on output
    if (/>\s*$/.test(cleaned)) {
      return {
        type: "codex",
        status: "waiting-input",
        raw: output,
      };
    }
    return {
      type: "codex",
      status: "working",
      raw: output,
    };
  }

  // Priority 4: Check for Claude Code working state
  // Look for common patterns when Claude is actively working
  // Check this BEFORE ready state (⏺) because agent may have been ready
  // but is now working (output contains both)
  const workingPatterns = [
    /I'll help/i,
    /Let me/i,
    /I'm going to/i,
    /I can see/i,
    /Looking at/i,
    /<function_calls>/,
    /<invoke/,
    /Analyzing/i,
    /Implementing/i,
  ];

  if (workingPatterns.some((pattern) => pattern.test(cleaned))) {
    logger.info("Detected Claude Code working state", {
      matchedPatterns: workingPatterns.filter((p) => p.test(cleaned)).map((p) => p.toString()),
    });
    return {
      type: "claude-code",
      status: "working",
      raw: output,
    };
  }

  // Priority 5: Check for Claude Code ready prompt (⏺ character)
  // This indicates agent is waiting for user input
  // Checked AFTER working patterns so that active work takes precedence
  if (/⏺/.test(cleaned)) {
    const lastLine = extractLastNonEmptyLine(cleaned);
    logger.info("Detected Claude Code ready state", { lastPrompt: lastLine });
    return {
      type: "claude-code",
      status: "waiting-input",
      lastPrompt: lastLine,
      raw: output,
    };
  }

  // Priority 6: Check for shell prompts
  // Various shell prompt patterns
  // Use cleaned output (ANSI codes removed) for better matching
  const shellPromptPatterns = [
    /[$%#]\s*$/m, // Generic shell prompt ($ % #) at end of line
    /bash-\d+\.\d+\$/m, // Bash version prompt (e.g., bash-5.2$)
    /\w+@\w+.*[$%#]\s*$/m, // User@host prompt
    /╭.*╰.*[$%#]/m, // Fancy multi-line prompts (oh-my-zsh, starship, etc.)
  ];

  if (shellPromptPatterns.some((pattern) => pattern.test(cleaned))) {
    logger.info("Detected shell prompt", {
      matchedPatterns: shellPromptPatterns.filter((p) => p.test(cleaned)).map((p) => p.toString()),
    });
    return {
      type: "shell",
      status: "idle",
      raw: output,
    };
  }

  // Priority 7: Empty or minimal output suggests shell
  // A shell with no commands run will show very little output
  if (cleaned === "" || cleaned.length < 10) {
    logger.info("Detected empty/minimal output, assuming shell");
    return {
      type: "shell",
      status: "idle",
      raw: output,
    };
  }

  // Cannot determine state
  logger.info("Could not determine terminal state", { outputPreview: trimmed.substring(0, 200) });
  return {
    type: "unknown",
    status: "unknown",
    raw: output,
  };
}

/**
 * Extract the last non-empty line from output
 *
 * This is useful for finding the current prompt or last message.
 *
 * @param text - Text to extract from
 * @returns The last non-empty line, or empty string if none found
 */
function extractLastNonEmptyLine(text: string): string {
  const lines = text.split("\n").map((line) => line.trim());
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i].length > 0) {
      return lines[i];
    }
  }
  return "";
}

/**
 * Read terminal output and get last N lines
 *
 * Internal helper for detectTerminalState(). Reads terminal output from daemon
 * and returns the last N lines for state detection.
 *
 * @param terminalId - Terminal session ID
 * @param lineCount - Number of lines to return
 * @returns The last N lines of terminal output
 */
async function getLastLines(terminalId: string, lineCount: number): Promise<string> {
  const { invoke } = await import("@tauri-apps/api/core");

  interface TerminalOutput {
    output: string; // Base64-encoded
    byte_count: number;
  }

  try {
    const result = await invoke<TerminalOutput>("get_terminal_output", {
      id: terminalId,
      startByte: null,
    });

    // Decode base64 output
    if (!result.output || result.output.length === 0) {
      logger.info("No output from terminal", { terminalId, byteCount: result.byte_count });
      return "";
    }

    // Base64 decode
    const binaryString = atob(result.output);
    const bytes = new Uint8Array(binaryString.length);
    for (let i = 0; i < binaryString.length; i++) {
      bytes[i] = binaryString.charCodeAt(i);
    }
    const text = new TextDecoder("utf-8").decode(bytes);

    // Extract last N lines
    const lines = text.split("\n");
    const lastLines = lines.slice(-lineCount).join("\n");

    logger.info("Read and extracted last lines", {
      terminalId,
      byteCount: result.byte_count,
      totalLines: lines.length,
      requestedLines: lineCount,
      returnedLines: lastLines.split("\n").length,
    });

    return lastLines;
  } catch (error) {
    logger.error("Failed to read terminal output", error, { terminalId });
    return "";
  }
}

/**
 * Detect terminal state by reading and parsing output
 *
 * This is the main entry point for passive state detection. It reads
 * the last N lines of terminal output and parses them to determine
 * the current terminal type and status.
 *
 * @param terminalId - Terminal session ID
 * @param lineCount - Number of lines to analyze (default: 20)
 * @returns Parsed terminal state
 */
export async function detectTerminalState(
  terminalId: string,
  lineCount: number = 20
): Promise<TerminalState> {
  logger.info("Detecting terminal state", { terminalId, lineCount });

  const output = await getLastLines(terminalId, lineCount);
  const state = parseTerminalState(output);

  logger.info("Terminal state detected", {
    terminalId,
    type: state.type,
    status: state.status,
    hasPrompt: !!state.lastPrompt,
  });

  return state;
}
