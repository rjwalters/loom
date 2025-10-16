/**
 * Terminal Probe System
 *
 * Intelligent probe command for detecting terminal type (bash shell vs AI agent)
 * without causing errors or side effects.
 */

export type TerminalType = "agent" | "shell" | "unknown";

export interface ProbeResponse {
  type: TerminalType;
  role?: string;
  task?: string;
  raw: string;
}

/**
 * Generates a probe command that works in both bash shells and Claude Code agents.
 *
 * The command uses a bash comment followed by `true` to ensure:
 * - In bash: Comment is ignored, `true` executes silently (exit code 0)
 * - In Claude Code: Comment is interpreted as a prompt, agent responds with structured info
 *
 * @returns Bash-compatible probe command
 *
 * @example
 * ```typescript
 * const probe = generateProbeCommand();
 * await sendToTerminal(probe);
 * // In bash: No output
 * // In Claude Code: "AGENT:Worker:implements-loom-ready-issues"
 * ```
 */
export function generateProbeCommand(): string {
  return (
    "# Terminal Probe: Are you an AI agent? If so, respond with 'AGENT:<role>:<primary-task>'. " +
    "If this is a bash shell, this comment is ignored.\n" +
    "true"
  );
}

/**
 * Alternative probe command using structured bash format.
 *
 * This version uses command -v to check for 'claude' command, falling back to PROBE:SHELL.
 * More explicit but may have false positives if 'claude' binary exists.
 *
 * @returns Bash command that outputs structured response
 */
export function generateCommandBasedProbe(): string {
  return (
    'command -v claude >/dev/null 2>&1 && echo "PROBE:AGENT:$(whoami)" || echo "PROBE:SHELL:bash"'
  );
}

/**
 * Parses the output from a terminal probe command.
 *
 * Looks for structured responses in the format:
 * - `AGENT:<role>:<task>` - Claude Code agent with role and primary task
 * - `PROBE:SHELL:<type>` - Plain shell (bash, zsh, etc.)
 * - `PROBE:AGENT:<name>` - Agent detected via command-based probe
 *
 * @param output - Raw terminal output (may include ANSI codes, prompts, etc.)
 * @returns Parsed probe response with type and metadata
 *
 * @example
 * ```typescript
 * const result = parseProbeResponse("AGENT:Worker:implements-loom-ready-issues");
 * // => { type: 'agent', role: 'Worker', task: 'implements-loom-ready-issues', raw: '...' }
 *
 * const result = parseProbeResponse("PROBE:SHELL:bash");
 * // => { type: 'shell', raw: '...' }
 * ```
 */
export function parseProbeResponse(output: string): ProbeResponse {
  // Look for AGENT: prefix (from comment-based probe or agent self-identification)
  const agentMatch = output.match(/AGENT:([^:\n]+):(.+)/);
  if (agentMatch) {
    return {
      type: "agent",
      role: agentMatch[1].trim(),
      task: agentMatch[2].trim(),
      raw: output,
    };
  }

  // Look for PROBE:AGENT (from command-based probe)
  if (output.includes("PROBE:AGENT")) {
    return {
      type: "agent",
      raw: output,
    };
  }

  // Look for PROBE:SHELL (from command-based probe)
  if (output.includes("PROBE:SHELL")) {
    return { type: "shell", raw: output };
  }

  // Check for common shell indicators if no structured response
  // This handles cases where bash executes `true` with no output
  const shellIndicators = [
    /^\s*$/,  // Empty output (bash executed `true` silently)
    /\$\s*$/,  // Bash prompt
    /%\s*$/,   // Zsh prompt
    />\s*$/,   // Fish prompt
  ];

  for (const indicator of shellIndicators) {
    if (indicator.test(output.trim())) {
      return { type: "shell", raw: output };
    }
  }

  // No clear response - unknown type
  return { type: "unknown", raw: output };
}

/**
 * Validates that a probe response is from an AI agent.
 *
 * @param response - Parsed probe response
 * @returns True if response indicates an AI agent
 */
export function isAgentResponse(response: ProbeResponse): boolean {
  return response.type === "agent";
}

/**
 * Validates that a probe response is from a bash shell.
 *
 * @param response - Parsed probe response
 * @returns True if response indicates a bash shell
 */
export function isShellResponse(response: ProbeResponse): boolean {
  return response.type === "shell";
}

/**
 * Gets a human-readable description of the probe response.
 *
 * @param response - Parsed probe response
 * @returns Formatted description string
 *
 * @example
 * ```typescript
 * const desc = getProbeDescription(response);
 * // "AI Agent - Worker (implements-loom-ready-issues)"
 * // "Bash Shell"
 * // "Unknown Terminal Type"
 * ```
 */
export function getProbeDescription(response: ProbeResponse): string {
  switch (response.type) {
    case "agent":
      if (response.role && response.task) {
        return `AI Agent - ${response.role} (${response.task})`;
      }
      return "AI Agent";

    case "shell":
      return "Bash Shell";

    case "unknown":
      return "Unknown Terminal Type";
  }
}
