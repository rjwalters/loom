import { invoke } from "@tauri-apps/api/tauri";
import type { GitIdentity } from "./worktree-manager";

/**
 * agent-launcher.ts - Functions for launching AI agents in terminals
 *
 * IMPORTANT: All functions in this module operate on sessionIds (ephemeral tmux session IDs).
 * - terminalId parameters are sessionIds used for IPC operations (send_input, etc.)
 * - Callers should use configId for state management, look up sessionId for launching
 */

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
  console.log(
    `[launchAgentInTerminal] START - terminalId=${terminalId}, roleFile=${roleFile}, workspacePath=${workspacePath}, useWorktree=${useWorktree}`
  );

  // Set up worktree if requested
  let agentWorkingDir = workspacePath;
  if (useWorktree && !worktreePath) {
    console.log(`[launchAgentInTerminal] Setting up worktree for ${terminalId}...`);
    const { setupWorktreeForAgent } = await import("./worktree-manager");
    agentWorkingDir = await setupWorktreeForAgent(terminalId, workspacePath, gitIdentity);
    console.log(
      `[launchAgentInTerminal] Worktree setup complete, agentWorkingDir=${agentWorkingDir}`
    );
  } else if (worktreePath) {
    agentWorkingDir = worktreePath;
    console.log(`[launchAgentInTerminal] Using existing worktreePath=${worktreePath}`);
  }

  // Read role file content from workspace
  console.log(`[launchAgentInTerminal] Reading role file ${roleFile}...`);
  const roleContent = await invoke<string>("read_role_file", {
    workspacePath,
    filename: roleFile,
  });
  console.log(`[launchAgentInTerminal] Role file read successfully, length=${roleContent.length}`);

  // Replace template variables in role content
  const processedPrompt = roleContent.replace(/\{\{workspace\}\}/g, agentWorkingDir);
  console.log(`[launchAgentInTerminal] Processed prompt with workspace=${agentWorkingDir}`);

  // Write the system prompt to CLAUDE.md in the worktree/workspace
  // Claude Code automatically loads CLAUDE.md from the repository root
  const claudeMdPath = `${agentWorkingDir}/CLAUDE.md`;
  console.log(`[launchAgentInTerminal] Writing CLAUDE.md to ${claudeMdPath}...`);
  await invoke("write_file", {
    path: claudeMdPath,
    content: processedPrompt,
  });
  console.log(`[launchAgentInTerminal] CLAUDE.md written successfully`);

  // Build Claude CLI command - CLAUDE.md will be automatically loaded
  // Note: --session-id removed because Claude Code requires UUID format, not our terminal IDs
  // Using --dangerously-skip-permissions to bypass the interactive warning prompt
  const command = "claude --dangerously-skip-permissions";
  console.log(`[launchAgentInTerminal] Sending command to terminal: ${command}`);

  // Wait for any previous commands to fully complete
  // This prevents command concatenation with worktree setup commands
  console.log(`[launchAgentInTerminal] Waiting 500ms for previous commands to complete...`);
  await new Promise((resolve) => setTimeout(resolve, 500));

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });
  console.log(`[launchAgentInTerminal] Command sent`);

  // Press Enter to execute
  console.log(`[launchAgentInTerminal] Sending Enter to execute`);
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });

  // Add initial delay to allow Claude Code to start
  // This prevents the "2" from concatenating with the previous command
  console.log(`[launchAgentInTerminal] Initial wait: 1000ms for Claude Code to start...`);
  await new Promise((resolve) => setTimeout(resolve, 1000));

  // Retry accepting the bypass permissions warning with exponential backoff
  // Claude Code initialization time varies, so we try multiple times with increasing delays
  const maxRetries = 3;
  const baseDelay = 2000; // Base delay: 2 seconds

  for (let attempt = 1; attempt <= maxRetries; attempt++) {
    const delay = baseDelay * attempt; // Exponential backoff: 2s, 4s, 6s
    console.log(
      `[launchAgentInTerminal] Attempt ${attempt}/${maxRetries}: Waiting ${delay}ms for bypass permissions prompt...`
    );
    await new Promise((resolve) => setTimeout(resolve, delay));

    // Accept the bypass permissions warning by selecting option 2
    console.log(`[launchAgentInTerminal] Attempt ${attempt}: Sending "2" to accept warning`);
    await invoke("send_terminal_input", {
      id: terminalId,
      data: "2",
    });

    // Small delay before pressing Enter to ensure "2" is processed
    await new Promise((resolve) => setTimeout(resolve, 100));

    // Press Enter to confirm selection
    console.log(`[launchAgentInTerminal] Attempt ${attempt}: Sending Enter to confirm`);
    await invoke("send_terminal_input", {
      id: terminalId,
      data: "\r",
    });

    // If this isn't the last attempt, wait a bit before retrying
    if (attempt < maxRetries) {
      console.log(`[launchAgentInTerminal] Waiting 500ms before next attempt...`);
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
  }

  console.log(`[launchAgentInTerminal] COMPLETE - returning agentWorkingDir=${agentWorkingDir}`);

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
