import { beforeEach, describe, expect, test } from "vitest";
import { AppLevelState, getAppLevelState, setAppLevelState } from "./app-state";

describe("AppLevelState", () => {
  let state: AppLevelState;

  beforeEach(() => {
    state = new AppLevelState();
    setAppLevelState(state);
  });

  describe("currentAttachedTerminalId", () => {
    test("should initialize as null", () => {
      expect(state.getCurrentAttachedTerminalId()).toBeNull();
    });

    test("should set and get terminal ID", () => {
      state.setCurrentAttachedTerminalId("terminal-1");
      expect(state.getCurrentAttachedTerminalId()).toBe("terminal-1");
    });

    test("should allow clearing by setting to null", () => {
      state.setCurrentAttachedTerminalId("terminal-1");
      state.setCurrentAttachedTerminalId(null);
      expect(state.getCurrentAttachedTerminalId()).toBeNull();
    });

    test("should allow updating to different terminal", () => {
      state.setCurrentAttachedTerminalId("terminal-1");
      state.setCurrentAttachedTerminalId("terminal-2");
      expect(state.getCurrentAttachedTerminalId()).toBe("terminal-2");
    });
  });

  describe("drag state", () => {
    test("should initialize all drag state as null/false", () => {
      const dragState = state.getDragState();
      expect(dragState.draggedConfigId).toBeNull();
      expect(dragState.dropTargetConfigId).toBeNull();
      expect(dragState.dropInsertBefore).toBe(false);
      expect(dragState.isDragging).toBe(false);
    });

    test("should track dragged element", () => {
      state.setDraggedConfigId("terminal-2");
      expect(state.getDraggedConfigId()).toBe("terminal-2");
    });

    test("should track drop target and insert position", () => {
      state.setDropTargetConfigId("terminal-3");
      state.setDropInsertBefore(true);
      expect(state.getDropTargetConfigId()).toBe("terminal-3");
      expect(state.getDropInsertBefore()).toBe(true);
    });

    test("should track dragging state", () => {
      state.setIsDragging(true);
      expect(state.getIsDragging()).toBe(true);
    });

    test("should reset all drag state", () => {
      // Set up drag state
      state.setDraggedConfigId("terminal-1");
      state.setDropTargetConfigId("terminal-2");
      state.setDropInsertBefore(true);
      state.setIsDragging(true);

      // Reset
      state.resetDragState();

      // Verify all cleared
      const dragState = state.getDragState();
      expect(dragState.draggedConfigId).toBeNull();
      expect(dragState.dropTargetConfigId).toBeNull();
      expect(dragState.dropInsertBefore).toBe(false);
      expect(dragState.isDragging).toBe(false);
    });

    test("getDragState should return all state at once", () => {
      state.setDraggedConfigId("terminal-1");
      state.setDropTargetConfigId("terminal-2");
      state.setDropInsertBefore(true);
      state.setIsDragging(true);

      const dragState = state.getDragState();
      expect(dragState).toEqual({
        draggedConfigId: "terminal-1",
        dropTargetConfigId: "terminal-2",
        dropInsertBefore: true,
        isDragging: true,
      });
    });

    test("getDragState should return a copy, not the original object", () => {
      state.setDraggedConfigId("terminal-1");

      const dragState1 = state.getDragState();
      const dragState2 = state.getDragState();

      // Verify they have the same values
      expect(dragState1).toEqual(dragState2);

      // Verify they are different objects
      expect(dragState1).not.toBe(dragState2);

      // Verify modifying the returned object doesn't affect state
      dragState1.draggedConfigId = "terminal-999";
      expect(state.getDraggedConfigId()).toBe("terminal-1");
    });

    test("should allow setting drop position to false", () => {
      state.setDropInsertBefore(true);
      expect(state.getDropInsertBefore()).toBe(true);

      state.setDropInsertBefore(false);
      expect(state.getDropInsertBefore()).toBe(false);
    });

    test("should allow setting dragging to false", () => {
      state.setIsDragging(true);
      expect(state.getIsDragging()).toBe(true);

      state.setIsDragging(false);
      expect(state.getIsDragging()).toBe(false);
    });

    test("should handle null values for dragged and drop target", () => {
      state.setDraggedConfigId("terminal-1");
      state.setDropTargetConfigId("terminal-2");

      state.setDraggedConfigId(null);
      state.setDropTargetConfigId(null);

      expect(state.getDraggedConfigId()).toBeNull();
      expect(state.getDropTargetConfigId()).toBeNull();
    });
  });

  describe("singleton pattern", () => {
    test("getAppLevelState should return same instance", () => {
      const instance1 = getAppLevelState();
      const instance2 = getAppLevelState();
      expect(instance1).toBe(instance2);
    });

    test("setAppLevelState should override singleton for testing", () => {
      const customState = new AppLevelState();
      setAppLevelState(customState);
      expect(getAppLevelState()).toBe(customState);
    });

    test("singleton should persist state across getAppLevelState calls", () => {
      const instance1 = getAppLevelState();
      instance1.setCurrentAttachedTerminalId("terminal-1");
      instance1.setDraggedConfigId("terminal-2");

      const instance2 = getAppLevelState();
      expect(instance2.getCurrentAttachedTerminalId()).toBe("terminal-1");
      expect(instance2.getDraggedConfigId()).toBe("terminal-2");
    });
  });

  describe("integration scenarios", () => {
    test("should handle complete drag and drop workflow", () => {
      // Start drag
      state.setIsDragging(true);
      state.setDraggedConfigId("terminal-1");
      expect(state.getIsDragging()).toBe(true);
      expect(state.getDraggedConfigId()).toBe("terminal-1");

      // Drag over target
      state.setDropTargetConfigId("terminal-2");
      state.setDropInsertBefore(false);
      expect(state.getDropTargetConfigId()).toBe("terminal-2");
      expect(state.getDropInsertBefore()).toBe(false);

      // End drag
      state.resetDragState();
      expect(state.getIsDragging()).toBe(false);
      expect(state.getDraggedConfigId()).toBeNull();
      expect(state.getDropTargetConfigId()).toBeNull();
      expect(state.getDropInsertBefore()).toBe(false);
    });

    test("should handle terminal switching workflow", () => {
      // Attach first terminal
      state.setCurrentAttachedTerminalId("terminal-1");
      expect(state.getCurrentAttachedTerminalId()).toBe("terminal-1");

      // Switch to second terminal
      state.setCurrentAttachedTerminalId("terminal-2");
      expect(state.getCurrentAttachedTerminalId()).toBe("terminal-2");

      // Detach terminal
      state.setCurrentAttachedTerminalId(null);
      expect(state.getCurrentAttachedTerminalId()).toBeNull();
    });

    test("should maintain independence between terminal ID and drag state", () => {
      state.setCurrentAttachedTerminalId("terminal-1");
      state.setDraggedConfigId("terminal-2");

      expect(state.getCurrentAttachedTerminalId()).toBe("terminal-1");
      expect(state.getDraggedConfigId()).toBe("terminal-2");

      state.resetDragState();
      expect(state.getCurrentAttachedTerminalId()).toBe("terminal-1");
      expect(state.getDraggedConfigId()).toBeNull();
    });
  });
});
