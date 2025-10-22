import { afterEach, describe, expect, test } from "vitest";
import { appLevelState, resetDragState } from "./app-state";

describe("appLevelState", () => {
  // Reset state after each test to avoid test pollution
  afterEach(() => {
    appLevelState.currentAttachedTerminalId = null;
    resetDragState();
  });

  describe("currentAttachedTerminalId", () => {
    test("should initialize as null", () => {
      expect(appLevelState.currentAttachedTerminalId).toBeNull();
    });

    test("should allow setting and getting terminal ID", () => {
      appLevelState.currentAttachedTerminalId = "terminal-1";
      expect(appLevelState.currentAttachedTerminalId).toBe("terminal-1");
    });

    test("should allow clearing by setting to null", () => {
      appLevelState.currentAttachedTerminalId = "terminal-1";
      appLevelState.currentAttachedTerminalId = null;
      expect(appLevelState.currentAttachedTerminalId).toBeNull();
    });

    test("should allow updating to different terminal", () => {
      appLevelState.currentAttachedTerminalId = "terminal-1";
      appLevelState.currentAttachedTerminalId = "terminal-2";
      expect(appLevelState.currentAttachedTerminalId).toBe("terminal-2");
    });
  });

  describe("dragState", () => {
    test("should initialize all drag state as null/false", () => {
      expect(appLevelState.dragState.draggedConfigId).toBeNull();
      expect(appLevelState.dragState.dropTargetConfigId).toBeNull();
      expect(appLevelState.dragState.dropInsertBefore).toBe(false);
      expect(appLevelState.dragState.isDragging).toBe(false);
    });

    test("should allow setting drag properties directly", () => {
      appLevelState.dragState.draggedConfigId = "terminal-2";
      appLevelState.dragState.dropTargetConfigId = "terminal-3";
      appLevelState.dragState.dropInsertBefore = true;
      appLevelState.dragState.isDragging = true;

      expect(appLevelState.dragState.draggedConfigId).toBe("terminal-2");
      expect(appLevelState.dragState.dropTargetConfigId).toBe("terminal-3");
      expect(appLevelState.dragState.dropInsertBefore).toBe(true);
      expect(appLevelState.dragState.isDragging).toBe(true);
    });

    test("should reset all drag state with resetDragState()", () => {
      // Set up drag state
      appLevelState.dragState.draggedConfigId = "terminal-1";
      appLevelState.dragState.dropTargetConfigId = "terminal-2";
      appLevelState.dragState.dropInsertBefore = true;
      appLevelState.dragState.isDragging = true;

      // Reset
      resetDragState();

      // Verify all cleared
      expect(appLevelState.dragState.draggedConfigId).toBeNull();
      expect(appLevelState.dragState.dropTargetConfigId).toBeNull();
      expect(appLevelState.dragState.dropInsertBefore).toBe(false);
      expect(appLevelState.dragState.isDragging).toBe(false);
    });

    test("should handle null values for dragged and drop target", () => {
      appLevelState.dragState.draggedConfigId = "terminal-1";
      appLevelState.dragState.dropTargetConfigId = "terminal-2";

      appLevelState.dragState.draggedConfigId = null;
      appLevelState.dragState.dropTargetConfigId = null;

      expect(appLevelState.dragState.draggedConfigId).toBeNull();
      expect(appLevelState.dragState.dropTargetConfigId).toBeNull();
    });
  });

  describe("integration scenarios", () => {
    test("should handle complete drag and drop workflow", () => {
      // Start drag
      appLevelState.dragState.isDragging = true;
      appLevelState.dragState.draggedConfigId = "terminal-1";
      expect(appLevelState.dragState.isDragging).toBe(true);
      expect(appLevelState.dragState.draggedConfigId).toBe("terminal-1");

      // Drag over target
      appLevelState.dragState.dropTargetConfigId = "terminal-2";
      appLevelState.dragState.dropInsertBefore = false;
      expect(appLevelState.dragState.dropTargetConfigId).toBe("terminal-2");
      expect(appLevelState.dragState.dropInsertBefore).toBe(false);

      // End drag
      resetDragState();
      expect(appLevelState.dragState.isDragging).toBe(false);
      expect(appLevelState.dragState.draggedConfigId).toBeNull();
      expect(appLevelState.dragState.dropTargetConfigId).toBeNull();
      expect(appLevelState.dragState.dropInsertBefore).toBe(false);
    });

    test("should handle terminal switching workflow", () => {
      // Attach first terminal
      appLevelState.currentAttachedTerminalId = "terminal-1";
      expect(appLevelState.currentAttachedTerminalId).toBe("terminal-1");

      // Switch to second terminal
      appLevelState.currentAttachedTerminalId = "terminal-2";
      expect(appLevelState.currentAttachedTerminalId).toBe("terminal-2");

      // Detach terminal
      appLevelState.currentAttachedTerminalId = null;
      expect(appLevelState.currentAttachedTerminalId).toBeNull();
    });

    test("should maintain independence between terminal ID and drag state", () => {
      appLevelState.currentAttachedTerminalId = "terminal-1";
      appLevelState.dragState.draggedConfigId = "terminal-2";

      expect(appLevelState.currentAttachedTerminalId).toBe("terminal-1");
      expect(appLevelState.dragState.draggedConfigId).toBe("terminal-2");

      resetDragState();
      expect(appLevelState.currentAttachedTerminalId).toBe("terminal-1");
      expect(appLevelState.dragState.draggedConfigId).toBeNull();
    });
  });
});
