/**
 * Worker Type Registry
 *
 * Maps simple worker types to their launcher functions.
 * This eliminates the if-else chain in terminal-settings-modal.ts by providing
 * O(1) lookup for worker type launchers.
 *
 * Worker types in this registry are "simple" launchers that only need a terminal ID.
 * Complex launchers (Claude, Codex) that require worktree setup are handled separately.
 */

export type WorkerTypeLauncher = (terminalId: string) => Promise<void>;

/**
 * Registry of simple worker type launchers.
 *
 * Each entry maps a worker type to an async launcher function.
 * These launchers use dynamic imports to avoid loading unnecessary dependencies.
 */
export const SIMPLE_WORKER_LAUNCHERS: Record<string, WorkerTypeLauncher> = {
  "github-copilot": async (terminalId: string) => {
    const { launchGitHubCopilotAgent } = await import("./agent-launcher");
    return launchGitHubCopilotAgent(terminalId);
  },
  gemini: async (terminalId: string) => {
    const { launchGeminiCLIAgent } = await import("./agent-launcher");
    return launchGeminiCLIAgent(terminalId);
  },
  deepseek: async (terminalId: string) => {
    const { launchDeepSeekAgent } = await import("./agent-launcher");
    return launchDeepSeekAgent(terminalId);
  },
  grok: async (terminalId: string) => {
    const { launchGrokAgent } = await import("./agent-launcher");
    return launchGrokAgent(terminalId);
  },
};

/**
 * Get a launcher for a simple worker type.
 *
 * @param workerType - The worker type to look up
 * @returns The launcher function if found, undefined otherwise
 */
export function getSimpleWorkerLauncher(workerType: string): WorkerTypeLauncher | undefined {
  return SIMPLE_WORKER_LAUNCHERS[workerType];
}
