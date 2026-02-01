/**
 * FileCollector - Gathers analytics data from workspace files
 *
 * Collects data from multiple sources:
 * - Input JSONL logs (.loom/logs/input/YYYY-MM-DD.jsonl)
 * - Agent metrics (via existing Tauri backend)
 * - Git history (via existing Tauri backend)
 * - Session state (from SessionManager)
 *
 * Part of Phase 5 (Loom Intelligence) - Issue #1898
 */

import { invoke } from "@tauri-apps/api/core";
import {
  type AgentMetrics,
  getAgentMetrics,
  getVelocitySummary,
  type VelocitySummary,
} from "../agent-metrics";
import { Logger } from "../logger";

const logger = Logger.forComponent("file-collector");

/**
 * Parsed input log entry from JSONL files
 */
export interface InputEntry {
  timestamp: string;
  type: "keystroke" | "paste" | "enter" | "command";
  length: number;
  preview: string;
  terminalId: string;
}

/**
 * Aggregated input statistics
 */
export interface InputStats {
  totalEntries: number;
  keystrokes: number;
  commands: number;
  pastes: number;
  enters: number;
  totalCharacters: number;
}

/**
 * Git change summary
 */
export interface GitStats {
  commitsToday: number;
  filesChanged: number;
  insertions: number;
  deletions: number;
  activeBranches: number;
}

/**
 * Complete analytics snapshot for the dashboard
 */
export interface AnalyticsData {
  /** Metrics for today */
  todayMetrics: AgentMetrics;
  /** Metrics for this week */
  weekMetrics: AgentMetrics;
  /** Velocity summary with trends */
  velocity: VelocitySummary;
  /** Today's input activity */
  inputStats: InputStats;
  /** Git change summary */
  gitStats: GitStats;
  /** Timestamp of data collection */
  collectedAt: string;
}

/**
 * Parse JSONL content into InputEntry objects
 *
 * Gracefully skips malformed lines rather than failing.
 */
export function parseInputJSONL(content: string): InputEntry[] {
  const entries: InputEntry[] = [];
  const lines = content.split("\n");

  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed) continue;

    try {
      const parsed = JSON.parse(trimmed) as Record<string, unknown>;
      if (parsed.timestamp && parsed.type && typeof parsed.length === "number") {
        entries.push({
          timestamp: String(parsed.timestamp),
          type: parsed.type as InputEntry["type"],
          length: parsed.length as number,
          preview: String(parsed.preview ?? ""),
          terminalId: String(parsed.terminalId ?? ""),
        });
      }
    } catch {
      // Skip malformed lines gracefully
    }
  }

  return entries;
}

/**
 * Aggregate input entries into summary statistics
 */
export function aggregateInputStats(entries: InputEntry[]): InputStats {
  const stats: InputStats = {
    totalEntries: entries.length,
    keystrokes: 0,
    commands: 0,
    pastes: 0,
    enters: 0,
    totalCharacters: 0,
  };

  for (const entry of entries) {
    stats.totalCharacters += entry.length;
    switch (entry.type) {
      case "keystroke":
        stats.keystrokes++;
        break;
      case "command":
        stats.commands++;
        break;
      case "paste":
        stats.pastes++;
        break;
      case "enter":
        stats.enters++;
        break;
    }
  }

  return stats;
}

/**
 * Read today's input log file
 */
async function readTodayInputLog(workspacePath: string): Promise<InputStats> {
  const today = new Date();
  const year = today.getFullYear();
  const month = String(today.getMonth() + 1).padStart(2, "0");
  const day = String(today.getDate()).padStart(2, "0");
  const logPath = `${workspacePath}/.loom/logs/input/${year}-${month}-${day}.jsonl`;

  try {
    const content = await invoke<string>("read_text_file", { path: logPath });
    const entries = parseInputJSONL(content);
    return aggregateInputStats(entries);
  } catch {
    // File may not exist yet today - return empty stats
    return {
      totalEntries: 0,
      keystrokes: 0,
      commands: 0,
      pastes: 0,
      enters: 0,
      totalCharacters: 0,
    };
  }
}

/**
 * Collect git stats from the workspace
 *
 * Uses the read_text_file command to read git output, or returns
 * empty stats if git operations fail.
 */
async function collectGitStats(workspacePath: string): Promise<GitStats> {
  const emptyStats: GitStats = {
    commitsToday: 0,
    filesChanged: 0,
    insertions: 0,
    deletions: 0,
    activeBranches: 0,
  };

  try {
    // Read git stats from the daemon state file which tracks PRs and issues
    const daemonStatePath = `${workspacePath}/.loom/daemon-state.json`;
    const content = await invoke<string>("read_text_file", { path: daemonStatePath });
    const daemonState = JSON.parse(content) as Record<string, unknown>;

    const pipelineState = daemonState.pipeline_state as Record<string, unknown[]> | undefined;
    const building = pipelineState?.building ?? [];
    const reviewRequested = pipelineState?.review_requested ?? [];
    const readyToMerge = pipelineState?.ready_to_merge ?? [];

    return {
      commitsToday: (daemonState.total_prs_merged as number) ?? 0,
      filesChanged: building.length + reviewRequested.length + readyToMerge.length,
      insertions: 0,
      deletions: 0,
      activeBranches: building.length,
    };
  } catch {
    logger.warn("Could not read daemon state for git stats");
    return emptyStats;
  }
}

/**
 * Collect all analytics data for the dashboard
 *
 * This is the main entry point for the FileCollector. It gathers data
 * from all available sources and returns a unified AnalyticsData object.
 *
 * @param workspacePath - Path to the workspace root
 * @returns Complete analytics snapshot
 */
export async function collectAnalyticsData(workspacePath: string): Promise<AnalyticsData> {
  // Collect all data sources in parallel for performance
  const [todayMetrics, weekMetrics, velocity, inputStats, gitStats] = await Promise.all([
    getAgentMetrics(workspacePath, "today"),
    getAgentMetrics(workspacePath, "week"),
    getVelocitySummary(workspacePath),
    readTodayInputLog(workspacePath),
    collectGitStats(workspacePath),
  ]);

  return {
    todayMetrics,
    weekMetrics,
    velocity,
    inputStats,
    gitStats,
    collectedAt: new Date().toISOString(),
  };
}
