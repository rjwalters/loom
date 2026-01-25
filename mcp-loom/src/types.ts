/**
 * Shared TypeScript types for Loom MCP server
 */

/**
 * Result structure for log/output operations
 */
export interface LogResult {
  content: string;
  linesReturned: number;
  totalLines: number;
  error?: string;
}

/**
 * Terminal state from daemon
 */
export interface Terminal {
  id: string;
  name: string;
  role?: string;
  working_dir?: string;
  tmux_session: string;
  created_at: number;
  isPrimary?: boolean;
}

/**
 * State file structure
 */
export interface StateFile {
  daemonPid?: number;
  nextAgentNumber: number;
  terminals: Array<{
    id: string;
    status: string;
    isPrimary: boolean;
    worktreePath?: string;
    agentPid?: number;
    agentStatus?: string;
    lastIntervalRun?: number;
  }>;
  selectedTerminalId: string | null;
  lastUpdated: string;
}

/**
 * Role configuration for a terminal
 */
export interface RoleConfig {
  workerType?: string;
  roleFile?: string;
  targetInterval?: number;
  intervalPrompt?: string;
}

/**
 * Terminal configuration in config.json
 */
export interface TerminalConfig {
  id: string;
  name: string;
  role?: string;
  roleConfig?: RoleConfig;
  theme?: string;
}

/**
 * Workspace config file structure
 */
export interface ConfigFile {
  version: string;
  offlineMode?: boolean;
  terminals: TerminalConfig[];
}

/**
 * Configuration for creating a terminal
 */
export interface CreateTerminalConfig {
  name?: string;
  role?: string;
  roleFile?: string;
  targetInterval?: number;
  intervalPrompt?: string;
  theme?: string;
  workingDir?: string;
}

/**
 * Configuration options for updating a terminal
 */
export interface ConfigureTerminalOptions {
  name?: string;
  role?: string;
  roleConfig?: Partial<RoleConfig>;
  theme?: string;
}

/**
 * Agent metrics result structure
 */
export interface AgentMetricsResult {
  success: boolean;
  data?: unknown;
  error?: string;
  format: "json" | "text";
  output: string;
}
