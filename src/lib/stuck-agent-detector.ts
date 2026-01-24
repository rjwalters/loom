/**
 * stuck-agent-detector.ts - Multi-signal stuck agent detection and recovery
 *
 * Architecture:
 * - This module provides intelligent stuck detection beyond simple timeouts
 * - Uses multiple signals: no output duration, needs_input state, repeated patterns
 * - Role-specific thresholds allow different detection sensitivity per agent type
 * - Integrates with health-monitor for activity tracking and terminal-state-parser for state detection
 *
 * Detection Signals:
 * 1. No output duration - Time since last terminal output
 * 2. Needs input duration - Time spent waiting for user input
 * 3. Repeated patterns - Detecting circular conversation patterns via output hash analysis
 * 4. Prompt without progress - Prompt sent but no meaningful tool calls detected
 *
 * Recovery Actions:
 * - none: Agent is healthy, no intervention needed
 * - notify: Show warning to user, agent may be stuck
 * - restart: Recommend restarting the agent
 * - escalate: Log for analysis, potential systematic issue
 *
 * @see health-monitor.ts for activity tracking
 * @see terminal-state-parser.ts for terminal state detection
 */

import { getHealthMonitor } from "./health-monitor";
import { getIntervalPromptManager } from "./interval-prompt-manager";
import { Logger } from "./logger";
import { getAppState, type Terminal } from "./state";
import { detectTerminalState, type TerminalState } from "./terminal-state-parser";

const logger = Logger.forComponent("stuck-agent-detector");

/**
 * Signals used to detect if an agent is stuck
 */
export interface StuckSignals {
  /** Time in milliseconds since last terminal output */
  noOutputDuration: number;
  /** Whether repeated prompt/response patterns detected */
  repeatedPatterns: boolean;
  /** Time in milliseconds spent in needs_input/waiting state */
  needsInputDuration: number;
  /** Whether a prompt was sent but no meaningful output received */
  promptWithoutProgress: boolean;
  /** Current terminal state from parser */
  terminalState: TerminalState | null;
}

/**
 * Role-specific thresholds for stuck detection
 */
export interface StuckThresholds {
  /** Maximum time with no output before considering stuck (ms) */
  maxNoOutput: number;
  /** Maximum time waiting for input before considering stuck (ms) */
  maxNeedsInput: number;
  /** Number of identical output chunks before flagging as pattern */
  patternRepeatThreshold: number;
  /** Time window for output chunking in seconds (default 30s) */
  chunkWindowSeconds: number;
  /** Maximum time after prompt without progress before flagging (ms) */
  noProgressTimeout: number;
}

/**
 * Default thresholds used when role doesn't specify custom values
 */
export const DEFAULT_STUCK_THRESHOLDS: StuckThresholds = {
  maxNoOutput: 15 * 60 * 1000, // 15 minutes (same as current stale threshold)
  maxNeedsInput: 5 * 60 * 1000, // 5 minutes waiting for input
  patternRepeatThreshold: 3, // 3+ identical chunks
  chunkWindowSeconds: 30, // 30-second output windows
  noProgressTimeout: 10 * 60 * 1000, // 10 minutes without progress after prompt
};

/**
 * Role-specific default thresholds based on expected work patterns
 */
export const ROLE_DEFAULT_THRESHOLDS: Record<string, Partial<StuckThresholds>> = {
  builder: {
    maxNoOutput: 30 * 60 * 1000, // 30 minutes - builders work on long tasks
    maxNeedsInput: 5 * 60 * 1000,
    chunkWindowSeconds: 30,
    patternRepeatThreshold: 3,
    noProgressTimeout: 10 * 60 * 1000, // 10 min
  },
  judge: {
    maxNoOutput: 10 * 60 * 1000, // 10 minutes - reviews should be faster
    maxNeedsInput: 3 * 60 * 1000,
    chunkWindowSeconds: 60, // Longer window for review work
    patternRepeatThreshold: 4, // More tolerance
    noProgressTimeout: 15 * 60 * 1000, // 15 min - reviewing is slow
  },
  curator: {
    maxNoOutput: 10 * 60 * 1000, // 10 minutes
    maxNeedsInput: 3 * 60 * 1000,
    chunkWindowSeconds: 30,
    patternRepeatThreshold: 3,
    noProgressTimeout: 10 * 60 * 1000,
  },
  champion: {
    maxNoOutput: 5 * 60 * 1000, // 5 minutes - merge operations are quick
    maxNeedsInput: 2 * 60 * 1000,
    chunkWindowSeconds: 20, // Shorter window for quick operations
    patternRepeatThreshold: 2, // Less tolerance
    noProgressTimeout: 5 * 60 * 1000, // 5 min - merges should be fast
  },
  doctor: {
    maxNoOutput: 20 * 60 * 1000, // 20 minutes - bug fixes can take time
    maxNeedsInput: 5 * 60 * 1000,
    chunkWindowSeconds: 30,
    patternRepeatThreshold: 3,
    noProgressTimeout: 15 * 60 * 1000, // 15 min - debugging takes time
  },
  architect: {
    maxNoOutput: 20 * 60 * 1000, // 20 minutes - analysis takes time
    maxNeedsInput: 5 * 60 * 1000,
    chunkWindowSeconds: 45, // Longer window for design work
    patternRepeatThreshold: 4, // More tolerance
    noProgressTimeout: 20 * 60 * 1000, // 20 min - design work is slow
  },
  hermit: {
    maxNoOutput: 20 * 60 * 1000, // 20 minutes
    maxNeedsInput: 5 * 60 * 1000,
    chunkWindowSeconds: 45,
    patternRepeatThreshold: 4,
    noProgressTimeout: 20 * 60 * 1000,
  },
  guide: {
    maxNoOutput: 10 * 60 * 1000, // 10 minutes
    maxNeedsInput: 3 * 60 * 1000,
    chunkWindowSeconds: 30,
    patternRepeatThreshold: 3,
    noProgressTimeout: 10 * 60 * 1000,
  },
  shepherd: {
    maxNoOutput: 45 * 60 * 1000, // 45 minutes - orchestration can take long
    maxNeedsInput: 5 * 60 * 1000,
    chunkWindowSeconds: 60, // Longer window for orchestration
    patternRepeatThreshold: 4,
    noProgressTimeout: 30 * 60 * 1000, // 30 min - orchestration spans many phases
  },
  loom: {
    maxNoOutput: 10 * 60 * 1000, // 10 minutes - daemon should be responsive
    maxNeedsInput: 3 * 60 * 1000,
    chunkWindowSeconds: 30,
    patternRepeatThreshold: 3,
    noProgressTimeout: 10 * 60 * 1000,
  },
};

/**
 * Patterns that indicate meaningful progress (tool calls, completions)
 * Generic patterns catch all tools automatically without hardcoded lists
 */
export const PROGRESS_PATTERNS = {
  // Generic: any tool call is progress (catches new tools automatically)
  toolCall: /<function_calls>/,
  toolResult: /<function_results>/,

  // High-value specific patterns (for future weighted scoring)
  fileWrite: /\b(Edit|Write|NotebookEdit)\b.*\bfile_path\b/,
  gitOp: /git (push|commit|add|merge)/,
  ghOp: /gh (pr|issue|api) (create|edit|merge|close)/,

  // Completion indicators
  success: /(Successfully|Created PR #|MERGED|Completed)/i,
};

/**
 * Normalize text for consistent hashing
 * Removes timestamps and normalizes whitespace to reduce false positives
 */
export function normalizeForHash(text: string): string {
  return text
    .replace(/\d{4}-\d{2}-\d{2}T[\d:]+Z?/g, "") // Remove ISO timestamps
    .replace(/\d{4}-\d{2}-\d{2}/g, "") // Remove date formats
    .replace(/\d{2}:\d{2}:\d{2}/g, "") // Remove time formats
    .replace(/\s+/g, " ") // Normalize whitespace
    .trim();
}

/**
 * Hash a normalized chunk of text using djb2 algorithm
 * Returns a 32-bit integer hash
 */
export function hashChunk(text: string): number {
  const normalized = normalizeForHash(text);
  let hash = 5381;
  for (let i = 0; i < normalized.length; i++) {
    hash = ((hash << 5) + hash + normalized.charCodeAt(i)) | 0;
  }
  return hash;
}

/**
 * Check if two output chunks are similar
 * Uses hash equality and length ratio to reduce collision false positives
 */
export function isSimilarChunk(a: OutputChunk, b: OutputChunk): boolean {
  if (a.hash !== b.hash) return false;
  // Require similar length to reduce collision false positives
  const lengthRatio = Math.min(a.length, b.length) / Math.max(a.length, b.length);
  return lengthRatio > 0.8;
}

/**
 * Detect if text contains progress indicators (tool calls)
 */
export function detectProgress(output: string): boolean {
  // Any tool call or tool result counts as progress
  return PROGRESS_PATTERNS.toolCall.test(output) || PROGRESS_PATTERNS.toolResult.test(output);
}

/**
 * Maximum number of chunks to track per terminal
 */
const MAX_CHUNKS = 10;

/**
 * Recommended action based on stuck analysis
 */
export type StuckAction = "none" | "notify" | "restart" | "escalate";

/**
 * Confidence level of stuck detection
 */
export type StuckConfidence = "low" | "medium" | "high";

/**
 * Result of analyzing a terminal for stuck conditions
 */
export interface StuckAnalysis {
  /** Terminal ID that was analyzed */
  terminalId: string;
  /** Whether the agent appears to be stuck */
  isStuck: boolean;
  /** Confidence level of the detection */
  confidence: StuckConfidence;
  /** Recommended action to take */
  recommendedAction: StuckAction;
  /** Human-readable reason for the detection */
  reason: string;
  /** The signals that contributed to this analysis */
  signals: StuckSignals;
  /** The thresholds used for this analysis */
  thresholds: StuckThresholds;
  /** Timestamp of the analysis */
  timestamp: number;
}

/**
 * Callback type for stuck detection events
 */
export type StuckDetectedCallback = (terminalId: string, analysis: StuckAnalysis) => void;

/**
 * Represents a hashed output chunk for pattern detection
 */
export interface OutputChunk {
  /** Hash of normalized chunk content */
  hash: number;
  /** Length of original content (for similarity comparison) */
  length: number;
  /** Timestamp when chunk was created */
  timestamp: number;
}

/**
 * State for pattern detection (Phase 2a)
 */
interface PatternState {
  /** Rolling window of recent output chunks */
  chunks: OutputChunk[];
  /** Timestamp when current chunk started accumulating */
  currentChunkStart: number;
  /** Accumulated content for current chunk */
  currentChunkContent: string;
}

/**
 * State for progress tracking (Phase 2b)
 */
interface ProgressState {
  /** Timestamp of last interval prompt sent */
  lastPromptTime: number | null;
  /** Timestamp of last detected progress (tool call) */
  lastProgressTime: number | null;
  /** Recent progress events for debugging (last 5) */
  recentProgress: string[];
}

/**
 * Internal state tracked per terminal for stuck detection
 */
interface TerminalStuckState {
  /** When the terminal entered needs_input/waiting state */
  needsInputSince: number | null;
  /** Last detected terminal state */
  lastState: TerminalState | null;
  /** Count of consecutive stuck detections */
  consecutiveStuckCount: number;
  /** Last time we notified about this terminal being stuck */
  lastNotification: number | null;
  /** Pattern detection state */
  patternState: PatternState;
  /** Progress tracking state */
  progressState: ProgressState;
}

/**
 * StuckAgentDetector - Intelligent stuck agent detection and recovery
 *
 * Features:
 * - Multi-signal detection (no output, needs input, patterns)
 * - Role-specific thresholds
 * - Confidence-based recommendations
 * - Notification throttling to prevent alert fatigue
 * - Integration with health monitor for activity data
 */
export class StuckAgentDetector {
  private terminalStates: Map<string, TerminalStuckState> = new Map();
  private callbacks: Set<StuckDetectedCallback> = new Set();
  private checkInterval: number | null = null;
  private checkIntervalMs: number = 60000; // Check every 60 seconds
  private notificationCooldownMs: number = 5 * 60 * 1000; // 5 minutes between notifications
  private running: boolean = false;

  /**
   * Start the stuck detection monitoring
   */
  start(): void {
    if (this.running) {
      logger.warn("Stuck agent detector already running");
      return;
    }

    logger.info("Starting stuck agent detector");
    this.running = true;

    // Perform initial check
    void this.checkAllTerminals();

    // Start periodic checks
    this.checkInterval = window.setInterval(() => {
      void this.checkAllTerminals();
    }, this.checkIntervalMs);
  }

  /**
   * Stop the stuck detection monitoring
   */
  stop(): void {
    if (!this.running) {
      return;
    }

    logger.info("Stopping stuck agent detector");
    this.running = false;

    if (this.checkInterval !== null) {
      window.clearInterval(this.checkInterval);
      this.checkInterval = null;
    }
  }

  /**
   * Set the check interval
   */
  setCheckInterval(ms: number): void {
    this.checkIntervalMs = ms;
    if (this.running && this.checkInterval !== null) {
      window.clearInterval(this.checkInterval);
      this.checkInterval = window.setInterval(() => {
        void this.checkAllTerminals();
      }, this.checkIntervalMs);
    }
  }

  /**
   * Register a callback for stuck detection events
   * @returns Cleanup function to unregister
   */
  onStuckDetected(callback: StuckDetectedCallback): () => void {
    this.callbacks.add(callback);
    return () => this.callbacks.delete(callback);
  }

  /**
   * Get thresholds for a specific role
   */
  getThresholdsForRole(role: string | undefined): StuckThresholds {
    if (!role) {
      return { ...DEFAULT_STUCK_THRESHOLDS };
    }

    const roleThresholds = ROLE_DEFAULT_THRESHOLDS[role.toLowerCase()];
    if (roleThresholds) {
      return {
        ...DEFAULT_STUCK_THRESHOLDS,
        ...roleThresholds,
      };
    }

    return { ...DEFAULT_STUCK_THRESHOLDS };
  }

  /**
   * Analyze a specific terminal for stuck conditions
   */
  async analyzeTerminal(terminalId: string): Promise<StuckAnalysis> {
    const state = getAppState();
    const terminal = state.terminals.getTerminal(terminalId);
    const healthMonitor = getHealthMonitor();
    const now = Date.now();

    if (!terminal) {
      return {
        terminalId,
        isStuck: false,
        confidence: "low",
        recommendedAction: "none",
        reason: "Terminal not found",
        signals: this.getEmptySignals(),
        thresholds: DEFAULT_STUCK_THRESHOLDS,
        timestamp: now,
      };
    }

    // Get or initialize terminal state tracking
    let terminalState = this.terminalStates.get(terminalId);
    if (!terminalState) {
      terminalState = this.createEmptyTerminalState();
      this.terminalStates.set(terminalId, terminalState);
    }

    // Get thresholds for this terminal's role
    const thresholds = this.getThresholdsForRole(terminal.role);

    // Gather signals
    const signals = await this.gatherSignals(terminal, terminalState, healthMonitor);

    // Update terminal state tracking
    this.updateTerminalState(terminalState, signals);

    // Analyze signals against thresholds
    const analysis = this.analyzeSignals(terminalId, signals, thresholds, terminalState, now);

    // Notify callbacks if stuck and not in cooldown
    if (analysis.isStuck && this.shouldNotify(terminalState, now)) {
      terminalState.lastNotification = now;
      terminalState.consecutiveStuckCount++;
      this.notifyCallbacks(terminalId, analysis);
    } else if (!analysis.isStuck) {
      // Reset consecutive count when not stuck
      terminalState.consecutiveStuckCount = 0;
    }

    return analysis;
  }

  /**
   * Check all active terminals for stuck conditions
   */
  async checkAllTerminals(): Promise<Map<string, StuckAnalysis>> {
    const state = getAppState();
    const terminals = state.terminals.getTerminals();
    const results = new Map<string, StuckAnalysis>();

    for (const terminal of terminals) {
      // Skip stopped terminals
      if (terminal.status === "stopped") {
        continue;
      }

      // Skip terminals without roles (plain shells)
      if (!terminal.role) {
        continue;
      }

      try {
        const analysis = await this.analyzeTerminal(terminal.id);
        results.set(terminal.id, analysis);
      } catch (error) {
        logger.error("Error analyzing terminal for stuck conditions", error, {
          terminalId: terminal.id,
        });
      }
    }

    return results;
  }

  /**
   * Get the recommended action based on analysis
   */
  getRecommendedAction(analysis: StuckAnalysis): StuckAction {
    return analysis.recommendedAction;
  }

  /**
   * Get current stuck state for a terminal (for UI display)
   */
  getTerminalStuckState(terminalId: string): TerminalStuckState | undefined {
    return this.terminalStates.get(terminalId);
  }

  /**
   * Clear stuck state for a terminal (e.g., after restart)
   */
  clearTerminalState(terminalId: string): void {
    this.terminalStates.delete(terminalId);
  }

  /**
   * Gather all signals for stuck detection
   */
  private async gatherSignals(
    terminal: Terminal,
    terminalState: TerminalStuckState,
    healthMonitor: ReturnType<typeof getHealthMonitor>
  ): Promise<StuckSignals> {
    const now = Date.now();
    const thresholds = this.getThresholdsForRole(terminal.role);

    // Get last activity time from health monitor
    const lastActivity = healthMonitor.getLastActivity(terminal.id);
    const noOutputDuration = lastActivity ? now - lastActivity : 0;

    // Get current terminal state from parser
    let currentState: TerminalState | null = null;
    try {
      currentState = await detectTerminalState(terminal.id);
    } catch (error) {
      logger.warn("Failed to detect terminal state", { terminalId: terminal.id, error });
    }

    // Calculate needs_input duration
    let needsInputDuration = 0;
    if (currentState?.status === "waiting-input" || currentState?.status === "bypass-prompt") {
      const stuckState = this.terminalStates.get(terminal.id);
      if (stuckState?.needsInputSince) {
        needsInputDuration = now - stuckState.needsInputSince;
      }
    }

    // Phase 2a: Detect repeated patterns
    const repeatedPatterns = this.detectRepeatedPatterns(terminalState, thresholds);

    // Phase 2b: Detect prompt without progress
    const promptWithoutProgress = this.detectPromptWithoutProgress(
      terminal.id,
      terminalState,
      thresholds,
      now
    );

    return {
      noOutputDuration,
      repeatedPatterns,
      needsInputDuration,
      promptWithoutProgress,
      terminalState: currentState,
    };
  }

  /**
   * Phase 2a: Detect repeated output patterns
   * Returns true if N identical chunks detected within the rolling window
   */
  private detectRepeatedPatterns(
    terminalState: TerminalStuckState,
    thresholds: StuckThresholds
  ): boolean {
    const { chunks } = terminalState.patternState;

    // Need at least threshold chunks to detect repetition
    if (chunks.length < thresholds.patternRepeatThreshold) {
      return false;
    }

    // Count occurrences of each unique hash (with length similarity)
    const hashCounts = new Map<number, number>();
    for (const chunk of chunks) {
      // Find matching chunks (same hash AND similar length)
      let foundMatch = false;
      for (const [existingHash] of hashCounts) {
        const existingChunk = chunks.find((c) => c.hash === existingHash);
        if (existingChunk && isSimilarChunk(chunk, existingChunk)) {
          hashCounts.set(existingHash, (hashCounts.get(existingHash) || 0) + 1);
          foundMatch = true;
          break;
        }
      }
      if (!foundMatch) {
        hashCounts.set(chunk.hash, 1);
      }
    }

    // Check if any hash appears >= threshold times
    for (const count of hashCounts.values()) {
      if (count >= thresholds.patternRepeatThreshold) {
        logger.info("Detected repeated output pattern", {
          repeatCount: count,
          threshold: thresholds.patternRepeatThreshold,
          chunkCount: chunks.length,
        });
        return true;
      }
    }

    return false;
  }

  /**
   * Phase 2b: Detect prompt sent without meaningful progress
   * Returns true if prompt was sent but no tool calls detected within timeout
   */
  private detectPromptWithoutProgress(
    terminalId: string,
    terminalState: TerminalStuckState,
    thresholds: StuckThresholds,
    now: number
  ): boolean {
    const { progressState } = terminalState;

    // Get last prompt time from interval prompt manager if available
    try {
      const intervalManager = getIntervalPromptManager();
      const status = intervalManager.getStatus(terminalId);
      if (status?.lastPromptTime) {
        // Update our tracking with the manager's data
        progressState.lastPromptTime = status.lastPromptTime;
      }
    } catch {
      // Interval manager may not be available in all contexts
    }

    // No prompt sent, can't detect prompt-without-progress
    if (!progressState.lastPromptTime) {
      return false;
    }

    const timeSincePrompt = now - progressState.lastPromptTime;

    // If progress was detected after the prompt, we're good
    if (
      progressState.lastProgressTime &&
      progressState.lastProgressTime > progressState.lastPromptTime
    ) {
      return false;
    }

    // Check if timeout exceeded
    if (timeSincePrompt > thresholds.noProgressTimeout) {
      logger.info("Detected prompt without progress", {
        terminalId,
        timeSincePromptMs: timeSincePrompt,
        timeoutMs: thresholds.noProgressTimeout,
        lastPromptTime: progressState.lastPromptTime,
        lastProgressTime: progressState.lastProgressTime,
      });
      return true;
    }

    return false;
  }

  /**
   * Record output for pattern analysis
   * Called by output-poller integration to feed output data
   */
  recordOutput(terminalId: string, output: string): void {
    let terminalState = this.terminalStates.get(terminalId);
    if (!terminalState) {
      terminalState = this.createEmptyTerminalState();
      this.terminalStates.set(terminalId, terminalState);
    }

    const now = Date.now();
    const { patternState, progressState } = terminalState;

    // Get thresholds for this terminal (need role from app state)
    const state = getAppState();
    const terminal = state.terminals.getTerminal(terminalId);
    const thresholds = this.getThresholdsForRole(terminal?.role);
    const chunkWindowMs = thresholds.chunkWindowSeconds * 1000;

    // Accumulate output in current chunk
    patternState.currentChunkContent += output;

    // Check if chunk window has elapsed
    if (now - patternState.currentChunkStart >= chunkWindowMs) {
      // Finalize current chunk
      if (patternState.currentChunkContent.length > 0) {
        const chunk: OutputChunk = {
          hash: hashChunk(patternState.currentChunkContent),
          length: patternState.currentChunkContent.length,
          timestamp: now,
        };
        patternState.chunks.push(chunk);

        // Maintain rolling window
        while (patternState.chunks.length > MAX_CHUNKS) {
          patternState.chunks.shift();
        }
      }

      // Start new chunk
      patternState.currentChunkStart = now;
      patternState.currentChunkContent = "";
    }

    // Check for progress indicators in output
    if (detectProgress(output)) {
      progressState.lastProgressTime = now;

      // Track recent progress for debugging
      const progressSummary = output.substring(0, 100).replace(/\n/g, " ");
      progressState.recentProgress.push(`${now}: ${progressSummary}`);
      while (progressState.recentProgress.length > 5) {
        progressState.recentProgress.shift();
      }

      logger.info("Progress detected in output", {
        terminalId,
        progressSummary,
      });
    }
  }

  /**
   * Record that a prompt was sent to a terminal
   * Called by interval-prompt-manager integration
   */
  recordPromptSent(terminalId: string): void {
    let terminalState = this.terminalStates.get(terminalId);
    if (!terminalState) {
      terminalState = this.createEmptyTerminalState();
      this.terminalStates.set(terminalId, terminalState);
    }

    terminalState.progressState.lastPromptTime = Date.now();
    logger.info("Prompt recorded", { terminalId });
  }

  /**
   * Create an empty terminal state with all Phase 2 fields initialized
   */
  private createEmptyTerminalState(): TerminalStuckState {
    return {
      needsInputSince: null,
      lastState: null,
      consecutiveStuckCount: 0,
      lastNotification: null,
      patternState: {
        chunks: [],
        currentChunkStart: Date.now(),
        currentChunkContent: "",
      },
      progressState: {
        lastPromptTime: null,
        lastProgressTime: null,
        recentProgress: [],
      },
    };
  }

  /**
   * Update terminal state tracking based on current signals
   */
  private updateTerminalState(terminalState: TerminalStuckState, signals: StuckSignals): void {
    const now = Date.now();
    const currentStatus = signals.terminalState?.status;

    // Track when terminal entered needs_input state
    if (currentStatus === "waiting-input" || currentStatus === "bypass-prompt") {
      if (terminalState.needsInputSince === null) {
        terminalState.needsInputSince = now;
      }
    } else {
      terminalState.needsInputSince = null;
    }

    terminalState.lastState = signals.terminalState;
  }

  /**
   * Analyze signals against thresholds to determine if stuck
   */
  private analyzeSignals(
    terminalId: string,
    signals: StuckSignals,
    thresholds: StuckThresholds,
    terminalState: TerminalStuckState,
    timestamp: number
  ): StuckAnalysis {
    const reasons: string[] = [];
    let confidence: StuckConfidence = "low";
    let isStuck = false;

    // Check no output duration
    if (signals.noOutputDuration > thresholds.maxNoOutput) {
      reasons.push(
        `No output for ${Math.round(signals.noOutputDuration / 60000)} minutes (threshold: ${Math.round(thresholds.maxNoOutput / 60000)} min)`
      );
      isStuck = true;
      confidence = "medium";
    }

    // Check needs input duration
    if (signals.needsInputDuration > thresholds.maxNeedsInput) {
      reasons.push(
        `Waiting for input for ${Math.round(signals.needsInputDuration / 60000)} minutes (threshold: ${Math.round(thresholds.maxNeedsInput / 60000)} min)`
      );
      isStuck = true;
      // Waiting for input is a stronger signal
      confidence = confidence === "medium" ? "high" : "medium";
    }

    // Check bypass prompt (always considered stuck if waiting too long)
    if (signals.terminalState?.status === "bypass-prompt" && signals.needsInputDuration > 60000) {
      reasons.push("Waiting at bypass permissions prompt");
      isStuck = true;
      confidence = "high";
    }

    // Phase 2a: Check for repeated patterns
    if (signals.repeatedPatterns) {
      reasons.push(
        `Detected repeated output patterns (${thresholds.patternRepeatThreshold}+ identical chunks)`
      );
      isStuck = true;
      // Repeated patterns is a strong signal of being stuck in a loop
      confidence = confidence === "low" ? "medium" : "high";
    }

    // Phase 2b: Check for prompt without progress
    if (signals.promptWithoutProgress) {
      reasons.push(
        `No tool calls after prompt (timeout: ${Math.round(thresholds.noProgressTimeout / 60000)} min)`
      );
      isStuck = true;
      // Prompt without progress indicates agent may be stuck thinking or in a loop
      confidence = confidence === "low" ? "medium" : confidence;
    }

    // Multiple consecutive detections increases confidence
    if (terminalState.consecutiveStuckCount >= 3) {
      confidence = "high";
    }

    // Determine recommended action
    let recommendedAction: StuckAction = "none";
    if (isStuck) {
      if (confidence === "high") {
        recommendedAction = terminalState.consecutiveStuckCount >= 5 ? "escalate" : "restart";
      } else if (confidence === "medium") {
        recommendedAction = "notify";
      } else {
        recommendedAction = "none"; // Low confidence, don't act yet
      }
    }

    const reason = reasons.length > 0 ? reasons.join("; ") : "Agent appears healthy";

    return {
      terminalId,
      isStuck,
      confidence,
      recommendedAction,
      reason,
      signals,
      thresholds,
      timestamp,
    };
  }

  /**
   * Check if we should send a notification (respects cooldown)
   */
  private shouldNotify(terminalState: TerminalStuckState, now: number): boolean {
    if (terminalState.lastNotification === null) {
      return true;
    }
    return now - terminalState.lastNotification > this.notificationCooldownMs;
  }

  /**
   * Notify all registered callbacks
   */
  private notifyCallbacks(terminalId: string, analysis: StuckAnalysis): void {
    logger.info("Notifying stuck detection callbacks", {
      terminalId,
      isStuck: analysis.isStuck,
      confidence: analysis.confidence,
      action: analysis.recommendedAction,
      reason: analysis.reason,
    });

    for (const callback of this.callbacks) {
      try {
        callback(terminalId, analysis);
      } catch (error) {
        logger.error("Error in stuck detection callback", error);
      }
    }
  }

  /**
   * Get empty signals object
   */
  private getEmptySignals(): StuckSignals {
    return {
      noOutputDuration: 0,
      repeatedPatterns: false,
      needsInputDuration: 0,
      promptWithoutProgress: false,
      terminalState: null,
    };
  }
}

// Singleton instance
let stuckDetectorInstance: StuckAgentDetector | null = null;

/**
 * Get the singleton stuck agent detector instance
 */
export function getStuckAgentDetector(): StuckAgentDetector {
  if (!stuckDetectorInstance) {
    stuckDetectorInstance = new StuckAgentDetector();
  }
  return stuckDetectorInstance;
}
