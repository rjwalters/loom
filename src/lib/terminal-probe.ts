/**
 * terminal-probe.ts - Terminal type detection utility
 *
 * Architecture:
 * - This module is the CHECKER: implements HOW to detect terminal type
 * - Generates probe commands and parses responses to identify agent vs shell
 * - Is STATELESS - does not schedule periodic checks or track state
 *
 * Separation of Concerns:
 * - terminal-probe.ts (this file): CHECKER - terminal TYPE detection via probe commands
 * - health-monitor.ts: SCHEDULER - periodic checks, activity timestamps, daemon connectivity
 *
 * These modules are intentionally separate:
 * - Terminal probing checks what TYPE a terminal is (AI agent vs plain shell)
 * - Health monitoring checks if terminals are ALIVE (session exists, responding)
 *
 * Design:
 * - Bash shells: Comments are ignored, `true` command succeeds silently
 * - AI agents: See comments as questions and respond with structured data
 *
 * Usage:
 *   const probeCmd = generateProbeCommand();
 *   const output = await sendToTerminal(terminalId, probeCmd);
 *   const response = parseProbeResponse(output);
 *   // response.type === "agent" | "shell" | "unknown"
 *
 * DO NOT add scheduling logic here - that's health-monitor's job.
 * This module should remain stateless and focused on detection.
 *
 * @see health-monitor.ts for periodic health checks
 * @see terminal-state-parser.ts for passive state detection (alternative approach)
 */

export interface ProbeResponse {
  /** The detected terminal type */
  type: "agent" | "shell" | "unknown";
  /** Agent role (if type === 'agent') */
  role?: string;
  /** Agent primary task (if type === 'agent') */
  task?: string;
  /** Raw output from the terminal */
  raw: string;
}

/**
 * Generate a probe command that works in both bash shells and AI agent sessions
 *
 * The command uses a bash comment followed by the `true` builtin:
 * - In bash: Comment is ignored, `true` returns 0 with no output
 * - In AI agents: Comment is interpreted as a question, agent responds with structured data
 *
 * @returns A bash-compatible probe command
 */
export function generateProbeCommand(): string {
  return (
    '# Terminal Probe: Are you an AI agent? If yes, respond with "AGENT:<role>:<primary-task>". ' +
    "If you're a bash shell, this is just a comment.\n" +
    "true"
  );
}

/**
 * Generate an alternative probe using command -v
 *
 * This variant uses a real command that checks if the CLI tool exists:
 * - In bash: Returns empty output or "not found" message
 * - In AI agents: May interpret the command or respond to context
 *
 * Less reliable than comment-based probe, but included as alternative.
 *
 * @returns A command-based probe
 */
export function generateCommandProbe(): string {
  return 'command -v claude >/dev/null 2>&1 && echo "PROBE:AGENT:$(whoami)" || echo "PROBE:SHELL:bash"';
}

/**
 * Parse the output from a terminal probe
 *
 * Looks for structured responses:
 * - "AGENT:<role>:<task>" → Detected agent with role and task
 * - "PROBE:AGENT:*" → Detected agent (command-based probe)
 * - "PROBE:SHELL:*" → Detected shell (command-based probe)
 * - Natural language with AI keywords → Likely an agent
 * - Empty or minimal output → Likely a shell
 *
 * @param output - The raw terminal output after sending probe
 * @returns Parsed probe response with detected type and metadata
 */
export function parseProbeResponse(output: string): ProbeResponse {
  const trimmed = output.trim();

  // Look for structured AGENT: response (primary format)
  // Format: AGENT:<role>:<task>
  // Example: AGENT:Worker:implements-loom-ready-issues
  // IMPORTANT: Must be uppercase AGENT and have both role and task fields
  const agentMatch = trimmed.match(/^AGENT:([^:]+):(.+)/);
  if (agentMatch) {
    return {
      type: "agent",
      role: agentMatch[1].trim(),
      task: agentMatch[2].trim(),
      raw: output,
    };
  }

  // Look for command-based probe responses
  if (trimmed.includes("PROBE:AGENT")) {
    return { type: "agent", raw: output };
  }

  if (trimmed.includes("PROBE:SHELL")) {
    return { type: "shell", raw: output };
  }

  // Empty output or just the prompt echo suggests a shell
  // Bash doesn't produce output for: # comment \n true
  if (trimmed === "" || trimmed === "true" || /^\$\s*$/.test(trimmed)) {
    return { type: "shell", raw: output };
  }

  // If output contains shell prompt patterns, it's likely a shell
  // Check for common patterns: "$", "$ ", "% " (zsh), "# " (root), "$ command\n$", etc.
  if (
    /^[$#%]\s*$/m.test(trimmed) ||
    /^bash-\d+\.\d+\$/.test(trimmed) ||
    /\$[^\n]*\n\$/.test(trimmed)
  ) {
    return { type: "shell", raw: output };
  }

  // Heuristics for natural language responses from agents
  // AI agents tend to use first person and explain their role
  // IMPORTANT: Check these AFTER shell detection to avoid false positives
  const agentKeywords = [
    /\b(I am|I'm)\s+(an?|the)\s+(AI|agent|assistant)/i,
    /\b(working|operating|running)\s+as\s+an?\s+/i,
    /\b(my role|my task|my purpose)\s+is\b/i,
    /\bClaude\s+(Code|AI)/i,
    // Only match role names in context with surrounding words (not standalone)
    /\b(as|a|the)\s+(Worker|Reviewer|Architect|Curator)\b/i,
  ];

  if (agentKeywords.some((regex) => regex.test(trimmed))) {
    // Natural language agent response, but no structured data
    return { type: "agent", raw: output };
  }

  // Ambiguous output - can't reliably determine
  return { type: "unknown", raw: output };
}
