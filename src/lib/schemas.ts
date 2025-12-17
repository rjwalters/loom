/**
 * Zod schemas for runtime validation of configuration and JSON data.
 * These schemas provide type-safe parsing with clear error messages.
 *
 * @module schemas
 */

import { z } from "zod";

// ============================================================================
// Enums and Constants
// ============================================================================

/**
 * Terminal status values matching TerminalStatus enum
 */
export const TerminalStatusSchema = z.enum(["idle", "busy", "needs_input", "error", "stopped"]);

/**
 * Agent status values matching AgentStatus enum
 */
export const AgentStatusSchema = z.enum([
  "not_started",
  "initializing",
  "ready",
  "busy",
  "waiting_for_input",
  "error",
  "stopped",
]);

// ============================================================================
// Primitive Schemas
// ============================================================================

/**
 * Color theme schema for terminal customization
 */
export const ColorThemeSchema = z.object({
  name: z.string(),
  primary: z.string(),
  background: z.string().optional(),
  border: z.string(),
});

export type ColorThemeFromSchema = z.infer<typeof ColorThemeSchema>;

/**
 * Input request schema for agent prompts
 */
export const InputRequestSchema = z.object({
  id: z.string(),
  prompt: z.string(),
  timestamp: z.number(),
});

export type InputRequestFromSchema = z.infer<typeof InputRequestSchema>;

// ============================================================================
// Configuration Schemas (persisted to .loom/config.json)
// ============================================================================

/**
 * Role configuration schema for agent settings
 */
export const RoleConfigSchema = z.record(z.string(), z.unknown());

/**
 * Terminal configuration schema (committed to git)
 */
export const TerminalConfigSchema = z.object({
  /** Stable terminal ID (e.g., "terminal-1") */
  id: z.string().min(1, "Terminal ID cannot be empty"),
  /** User-assigned terminal name */
  name: z.string().min(1, "Terminal name cannot be empty"),
  /** Optional role type (worker, reviewer, architect, etc.) */
  role: z.string().optional(),
  /** Role-specific configuration */
  roleConfig: RoleConfigSchema.optional(),
  /** Theme ID (e.g., "ocean", "forest") or "default" */
  theme: z.string().optional(),
  /** Custom color theme configuration */
  customTheme: ColorThemeSchema.optional(),
});

export type TerminalConfigFromSchema = z.infer<typeof TerminalConfigSchema>;

/**
 * Root configuration structure for Loom workspace (v2)
 */
export const LoomConfigSchema = z.object({
  /** Configuration format version */
  version: z.literal("2"),
  /** Array of terminal configurations */
  terminals: z.array(TerminalConfigSchema),
  /** Offline mode flag */
  offlineMode: z.boolean().optional(),
});

export type LoomConfigFromSchema = z.infer<typeof LoomConfigSchema>;

/**
 * Raw configuration schema (accepts any version for migration)
 */
export const RawLoomConfigSchema = z.object({
  version: z.string().optional(),
  terminals: z.array(z.record(z.string(), z.unknown())).optional(),
  agents: z.array(z.record(z.string(), z.unknown())).optional(),
  offlineMode: z.boolean().optional(),
});

// ============================================================================
// State Schemas (gitignored, machine-specific)
// ============================================================================

/**
 * Pending input request schema for terminal state
 */
export const PendingInputRequestSchema = z.object({
  id: z.string(),
  prompt: z.string(),
  timestamp: z.number(),
});

/**
 * Terminal state schema (runtime/ephemeral data)
 */
export const TerminalStateSchema = z.object({
  /** Stable terminal ID */
  id: z.string().min(1),
  /** Current runtime status */
  status: TerminalStatusSchema,
  /** Whether this terminal is currently focused */
  isPrimary: z.boolean(),
  /** Active git worktree path */
  worktreePath: z.string().optional(),
  /** Running agent process ID */
  agentPid: z.number().optional(),
  /** Agent lifecycle state */
  agentStatus: AgentStatusSchema.optional(),
  /** Unix timestamp (ms) of last autonomous interval execution */
  lastIntervalRun: z.number().optional(),
  /** Queue of pending input requests */
  pendingInputRequests: z.array(PendingInputRequestSchema).optional(),
  /** Total milliseconds spent in busy state */
  busyTime: z.number().optional(),
  /** Total milliseconds spent in idle state */
  idleTime: z.number().optional(),
  /** Unix timestamp (ms) of last status change */
  lastStateChange: z.number().optional(),
});

export type TerminalStateFromSchema = z.infer<typeof TerminalStateSchema>;

/**
 * Root state structure for Loom workspace
 */
export const LoomStateSchema = z.object({
  /** Running daemon process ID */
  daemonPid: z.number().optional(),
  /** Counter for terminal numbering */
  nextAgentNumber: z.number().min(1),
  /** Array of terminal runtime states */
  terminals: z.array(TerminalStateSchema),
});

export type LoomStateFromSchema = z.infer<typeof LoomStateSchema>;

// ============================================================================
// Role Metadata Schemas (from .loom/roles/*.json)
// ============================================================================

/**
 * Git identity for role-specific commits
 */
export const GitIdentitySchema = z.object({
  name: z.string().min(1, "Git name cannot be empty"),
  email: z.string().email("Invalid email format"),
});

export type GitIdentityFromSchema = z.infer<typeof GitIdentitySchema>;

/**
 * Role metadata schema from role JSON files
 */
export const RoleMetadataSchema = z.object({
  /** Display name of the role */
  name: z.string().optional(),
  /** Brief description of the role */
  description: z.string().optional(),
  /** Default interval in milliseconds for autonomous operation */
  defaultInterval: z.number().min(0).optional(),
  /** Default prompt for autonomous intervals */
  defaultIntervalPrompt: z.string().optional(),
  /** Whether autonomous mode is recommended */
  autonomousRecommended: z.boolean().optional(),
  /** Suggested worker type (claude, etc.) */
  suggestedWorkerType: z
    .enum(["claude", "none", "github-copilot", "gemini", "deepseek", "grok"])
    .optional(),
  /** Git identity for commits */
  gitIdentity: GitIdentitySchema.optional(),
});

export type RoleMetadataFromSchema = z.infer<typeof RoleMetadataSchema>;

// ============================================================================
// Activity Entry Schemas
// ============================================================================

/**
 * Activity entry schema for terminal history
 */
export const ActivityEntrySchema = z.object({
  /** Unique database ID for this input */
  inputId: z.number(),
  /** ISO 8601 timestamp of when the input was sent */
  timestamp: z.string(),
  /** Type of input */
  inputType: z.enum(["manual", "autonomous", "system", "user_instruction"]),
  /** The full prompt/command text */
  prompt: z.string(),
  /** Agent role at time of input */
  agentRole: z.string().nullable(),
  /** Git branch at time of input */
  gitBranch: z.string().nullable(),
  /** Preview of terminal output */
  outputPreview: z.string().nullable(),
  /** Exit code from command */
  exitCode: z.number().nullable(),
  /** ISO 8601 timestamp of output */
  outputTimestamp: z.string().nullable(),
});

export type ActivityEntryFromSchema = z.infer<typeof ActivityEntrySchema>;
