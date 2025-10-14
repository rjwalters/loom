import { invoke } from "@tauri-apps/api/tauri";
import type { GitIdentity } from "./worktree-manager";

/**
 * Launch a Claude agent in a terminal by sending the Claude CLI command
 *
 * This uses the existing terminal input mechanism to send a Claude command
 * with the appropriate role prompt and configuration. The agent runs visibly
 * in the terminal where users can see output and interact if needed.
 *
 * @param terminalId - The terminal ID to launch the agent in
 * @param roleFile - The role file to use (e.g., "worker.md")
 * @param workspacePath - The workspace path for the agent
 * @param worktreePath - Optional worktree path for isolated work (defaults to workspace)
 * @param useWorktree - Whether to create a worktree for isolation (default: false)
 * @param gitIdentity - Optional git identity to configure in the worktree
 * @returns Promise that resolves with the working directory path that was used
 */
export async function launchAgentInTerminal(
  terminalId: string,
  roleFile: string,
  workspacePath: string,
  worktreePath?: string,
  useWorktree = false,
  gitIdentity?: GitIdentity
): Promise<string> {
  // Set up worktree if requested
  let agentWorkingDir = workspacePath;
  if (useWorktree && !worktreePath) {
    const { setupWorktreeForAgent } = await import("./worktree-manager");
    agentWorkingDir = await setupWorktreeForAgent(terminalId, workspacePath, gitIdentity);
  } else if (worktreePath) {
    agentWorkingDir = worktreePath;
  }

  // Read role file content from workspace
  const roleContent = await invoke<string>("read_role_file", {
    workspacePath,
    filename: roleFile,
  });

  // Replace template variables in role content
  const processedPrompt = roleContent.replace(/\{\{workspace\}\}/g, agentWorkingDir);

  // Escape quotes for shell (double quotes need to be escaped)
  const escapedPrompt = processedPrompt.replace(/"/g, '\\"');

  // Build Claude CLI command
  // Note: Using double quotes around prompt, so internal quotes are escaped above
  const command = `claude --system-prompt "${escapedPrompt}" --permission-mode bypassPermissions --session-id ${terminalId}`;

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });

  // Press Enter to execute
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });

  // Return the working directory that was used
  return agentWorkingDir;
}

/**
 * Stop a Claude agent running in a terminal by sending Ctrl+C
 *
 * @param terminalId - The terminal ID to stop the agent in
 * @returns Promise that resolves when the stop signal is sent
 */
export async function stopAgentInTerminal(terminalId: string): Promise<void> {
  // Send Ctrl+C (character code 3)
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\u0003",
  });
}

/**
 * Send a prompt to an agent running in a terminal
 *
 * This is used for autonomous mode to send interval prompts to agents.
 *
 * @param terminalId - The terminal ID to send the prompt to
 * @param prompt - The prompt text to send
 * @returns Promise that resolves when the prompt is sent
 */
export async function sendPromptToAgent(terminalId: string, prompt: string): Promise<void> {
  // Send the prompt text
  await invoke("send_terminal_input", {
    id: terminalId,
    data: prompt,
  });

  // Press Enter to submit
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });
}

/**
 * Launch GitHub Copilot in a terminal
 *
 * This uses the gh CLI's copilot extension to start an interactive chat session
 * in the terminal. The agent runs visibly where users can see output and interact.
 *
 * @param terminalId - The terminal ID to launch Copilot in
 * @returns Promise that resolves when Copilot is launched
 */
export async function launchGitHubCopilotAgent(terminalId: string): Promise<void> {
  // Build GitHub Copilot CLI command
  // Using 'gh copilot' for interactive chat mode
  const command = "gh copilot";

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });

  // Press Enter to execute
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });
}

/**
 * Launch Google Gemini CLI in a terminal
 *
 * This uses the Gemini CLI to start an interactive coding session
 * in the terminal. The agent runs visibly where users can see output and interact.
 *
 * @param terminalId - The terminal ID to launch Gemini in
 * @returns Promise that resolves when Gemini is launched
 */
export async function launchGeminiCLIAgent(terminalId: string): Promise<void> {
  // Build Gemini CLI command for interactive mode
  const command = "gemini chat";

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });

  // Press Enter to execute
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });
}

/**
 * Launch DeepSeek CLI in a terminal
 *
 * This uses the DeepSeek CLI to start an interactive coding session
 * in the terminal. The agent runs visibly where users can see output and interact.
 *
 * @param terminalId - The terminal ID to launch DeepSeek in
 * @returns Promise that resolves when DeepSeek is launched
 */
export async function launchDeepSeekAgent(terminalId: string): Promise<void> {
  // Build DeepSeek CLI command for interactive mode
  const command = "deepseek chat";

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });

  // Press Enter to execute
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });
}

/**
 * Launch xAI Grok CLI in a terminal
 *
 * This uses the Grok CLI to start an interactive coding session
 * in the terminal. The agent runs visibly where users can see output and interact.
 *
 * @param terminalId - The terminal ID to launch Grok in
 * @returns Promise that resolves when Grok is launched
 */
export async function launchGrokAgent(terminalId: string): Promise<void> {
  // Build Grok CLI command for interactive mode
  const command = "grok chat";

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });

  // Press Enter to execute
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });
}
