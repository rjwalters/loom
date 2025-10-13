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
 * @param useWorktree - Whether to create a worktree for isolation (default: false)
 * @returns Promise that resolves with the working directory path that was used
 */
export async function launchAgentInTerminal(
  terminalId: string,
  roleFile: string,
  workspacePath: string,
  worktreePath?: string,
  useWorktree = false
): Promise<string> {
  // Set up worktree if requested
  let agentWorkingDir = workspacePath;
  if (useWorktree && !worktreePath) {
    const { setupWorktreeForAgent } = await import("./worktree-manager");
    agentWorkingDir = await setupWorktreeForAgent(terminalId, workspacePath);
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
