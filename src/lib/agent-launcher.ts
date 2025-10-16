import { invoke } from "@tauri-apps/api/tauri";

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
 * NOTE: Worktrees are now created on-demand when claiming issues, not automatically.
 * Agents start in the main workspace and create worktrees using `pnpm worktree <issue>`.
 *
 * @param terminalId - The terminal ID to launch the agent in
 * @param roleFile - The role file to use (e.g., "worker.md")
 * @param workspacePath - The workspace path (used for reading role files)
 * @param worktreePath - The worktree path (empty string if agent should use main workspace)
 * @returns Promise that resolves when the agent is launched
 */
export async function launchAgentInTerminal(
  terminalId: string,
  roleFile: string,
  workspacePath: string,
  worktreePath: string
): Promise<void> {
  console.log(
    `[launchAgentInTerminal] START - terminalId=${terminalId}, roleFile=${roleFile}, workspacePath=${workspacePath}, worktreePath=${worktreePath}`
  );

  // Use worktree path if provided, otherwise use main workspace
  const agentWorkingDir = worktreePath || workspacePath;
  const location = worktreePath ? `worktree at ${worktreePath}` : "main workspace";
  console.log(`[launchAgentInTerminal] Using ${location}`);

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

  // Build Claude CLI command
  // Note: We send the role as the first message instead of writing CLAUDE.md
  // This prevents conflicts with the main workspace CLAUDE.md (project instructions)
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

  // Wait for Claude Code to be ready to receive input
  console.log(`[launchAgentInTerminal] Waiting 2000ms for Claude Code to initialize...`);
  await new Promise((resolve) => setTimeout(resolve, 2000));

  // Send the role prompt as the first message
  console.log(`[launchAgentInTerminal] Sending role prompt as first message...`);
  await invoke("send_terminal_input", {
    id: terminalId,
    data: processedPrompt,
  });

  // Press Enter to submit the prompt
  console.log(`[launchAgentInTerminal] Sending Enter to submit role prompt`);
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });

  console.log(`[launchAgentInTerminal] COMPLETE - agent launched in ${agentWorkingDir}`);
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

/**
 * Launch Codex agent in a terminal with system prompt
 *
 * This launches Codex with configuration similar to Claude Code:
 * - Loads role file and writes to CODEX.md (system prompt)
 * - Uses --full-auto for autonomous execution
 * - Workspace-write sandbox with on-failure approval
 * - Permissions configured in .codex/config.toml
 *
 * NOTE: Worktrees are now created on-demand when claiming issues, not automatically.
 * Agents start in the main workspace and create worktrees using `pnpm worktree <issue>`.
 *
 * @param terminalId - The terminal ID to launch the agent in
 * @param roleFile - The role file to use (e.g., "worker.md")
 * @param workspacePath - The workspace path (used for reading role files)
 * @param worktreePath - The worktree path (empty string if agent should use main workspace)
 * @returns Promise that resolves when the agent is launched
 */
export async function launchCodexAgent(
  terminalId: string,
  roleFile: string,
  workspacePath: string,
  worktreePath: string
): Promise<void> {
  console.log(
    `[launchCodexAgent] START - terminalId=${terminalId}, roleFile=${roleFile}, worktreePath=${worktreePath}`
  );

  // Use worktree path if provided, otherwise use main workspace
  const agentWorkingDir = worktreePath || workspacePath;
  const location = worktreePath ? `worktree at ${worktreePath}` : "main workspace";
  console.log(`[launchCodexAgent] Using ${location}`);

  // Read role file content from workspace
  console.log(`[launchCodexAgent] Reading role file ${roleFile}...`);
  const roleContent = await invoke<string>("read_role_file", {
    workspacePath,
    filename: roleFile,
  });
  console.log(`[launchCodexAgent] Role file read successfully, length=${roleContent.length}`);

  // Replace template variables in role content
  const processedPrompt = roleContent.replace(/\{\{workspace\}\}/g, agentWorkingDir);
  console.log(`[launchCodexAgent] Processed prompt with workspace=${agentWorkingDir}`);

  // Build Codex CLI command with autonomous configuration
  // Note: We send the prompt directly in the command instead of via a file
  // This prevents conflicts with files in the main workspace
  // --full-auto: Combines -a on-failure and --sandbox workspace-write
  // Additional config from .codex/config.toml (sandbox_permissions, etc.)
  // Using single quotes around the heredoc to prevent shell expansion
  const command = `codex --full-auto "$(cat <<'ROLE_EOF'\n${processedPrompt}\nROLE_EOF\n)"`;
  console.log(
    `[launchCodexAgent] Sending command to terminal (prompt length: ${processedPrompt.length})`
  );

  // Wait for any previous commands to fully complete
  console.log(`[launchCodexAgent] Waiting 500ms for previous commands to complete...`);
  await new Promise((resolve) => setTimeout(resolve, 500));

  // Send command to terminal
  await invoke("send_terminal_input", {
    id: terminalId,
    data: command,
  });
  console.log(`[launchCodexAgent] Command sent`);

  // Press Enter to execute
  console.log(`[launchCodexAgent] Sending Enter to execute`);
  await invoke("send_terminal_input", {
    id: terminalId,
    data: "\r",
  });

  console.log(`[launchCodexAgent] COMPLETE - Codex agent launched in ${agentWorkingDir}`);
}
