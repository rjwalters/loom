/**
 * Tests for the daemon-only `list_terminals` path (Issue #13821).
 *
 * `listTerminals()` used to fall back to a `~/.loom/state.json` snapshot when
 * the daemon was unreachable. That file's only writer was the Tauri desktop
 * app removed in #3353 (and, for the home-directory copy, the now-removed
 * `set_primary_terminal`), so the fallback could only ever return a stale or
 * empty list — the unverified result #4794 had to warn about.
 *
 * The replacement contract, asserted here: an unreachable daemon REJECTS.
 * "The daemon did not answer" must never be rendered as the indistinguishable
 * "there are no terminals" (`.loom/docs/fail-closed.md`).
 */

import { afterEach, describe, expect, it, vi } from "vitest";

const sendDaemonRequest = vi.hoisted(() => vi.fn());
vi.mock("../shared/daemon.js", () => ({ sendDaemonRequest }));

import { handleTerminalTool } from "./terminals.js";

afterEach(() => {
  vi.clearAllMocks();
});

describe("list_terminals (daemon-only since #13821)", () => {
  it("lists the terminals the daemon reports", async () => {
    sendDaemonRequest.mockResolvedValueOnce({
      type: "TerminalList",
      payload: {
        terminals: [
          {
            id: "terminal-1",
            name: "Builder",
            role: "builder",
            working_dir: "/repo",
            tmux_session: "loom-terminal-1",
            created_at: 0,
          },
        ],
      },
    });

    const [{ text }] = await handleTerminalTool("list_terminals");

    expect(text).toContain("=== Active Loom Terminals (1) ===");
    expect(text).toContain("ID: terminal-1");
  });

  it("propagates the connection error when the daemon is unreachable", async () => {
    sendDaemonRequest.mockRejectedValueOnce(new Error("connect ENOENT /tmp/loom-daemon.sock"));

    // Rejecting is the point: index.ts turns this into an `isError` tool
    // result, so the caller is told the daemon did not answer instead of
    // being handed a confident, empty, unverified list.
    await expect(handleTerminalTool("list_terminals")).rejects.toThrow("connect ENOENT");
  });

  it("reports an empty registry only when the daemon actually answered", async () => {
    sendDaemonRequest.mockResolvedValueOnce({ type: "TerminalList", payload: { terminals: [] } });

    const [{ text }] = await handleTerminalTool("list_terminals");

    expect(text).toContain("No active terminals found");
    // The retired state-file fallback appended an "UNVERIFIED" caveat here;
    // a daemon-confirmed empty registry needs no caveat.
    expect(text).not.toContain("UNVERIFIED");
  });
});

describe("removed state.json tools (#13821)", () => {
  it.each(["set_primary_terminal", "get_selected_terminal"])(
    "no longer advertises or handles %s",
    async (name) => {
      await expect(handleTerminalTool(name, { terminal_id: "terminal-1" })).rejects.toThrow(
        `Unknown terminal tool: ${name}`
      );
      expect(sendDaemonRequest).not.toHaveBeenCalled();
    }
  );
});
