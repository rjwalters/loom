import { invoke } from "@tauri-apps/api/tauri";

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
 * @returns Promise that resolves when the command is sent
 */
export async function launchAgentInTerminal(
  terminalId: string,
  roleFile: string,
  workspacePath: string,
  worktreePath?: string
): Promise<void> {
  // Read role file content from workspace
  const roleContent = await invoke<string>("read_role_file", {
    workspacePath,
    filename: roleFile,
  });

  // Use worktree path if provided, otherwise use workspace
  const agentWorkingDir = worktreePath || workspacePath;

  // TODO: Set up git worktree before launching Claude
  // Currently, worktree setup is handled in Rust daemon via create_terminal_with_worktree.
  // We should investigate moving this to TypeScript for consistency with our terminal-first approach.
  // See issue #58 for investigation and refactor plan.

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
