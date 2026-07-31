/**
 * Tests for `buildTerminalListFallbackNote` (Issue #4794).
 *
 * The daemon-side registry (`TerminalManager::prune_dead_terminals`,
 * `loom-daemon/src/terminal.rs`) garbage-collects terminals whose backing
 * tmux session no longer exists on every `ListTerminals` request, so the
 * `"daemon"`-sourced path is always tmux-verified. The `.loom/state.json`
 * fallback path (used only when the daemon itself is unreachable) has no
 * such verification, so `list_terminals` must flag it rather than present a
 * possibly-stale snapshot as confirmed-live.
 */

import { describe, expect, it } from "vitest";
import { buildTerminalListFallbackNote } from "./terminals.js";

describe("buildTerminalListFallbackNote", () => {
  it("is empty for the tmux-verified daemon source", () => {
    expect(buildTerminalListFallbackNote("daemon")).toBe("");
  });

  it("flags the state-file fallback as unverified against live tmux sessions", () => {
    const note = buildTerminalListFallbackNote("state-fallback");
    expect(note).toContain("UNVERIFIED");
    expect(note).toContain("loom-daemon did not respond");
    expect(note.toLowerCase()).toContain("tmux");
  });
});
