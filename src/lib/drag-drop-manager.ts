import type { AppState } from "./state";

/**
 * Drag-and-drop manager for reordering terminals in the mini terminal row
 *
 * This module encapsulates all drag-and-drop logic for terminal cards,
 * managing drag state and providing visual feedback during drag operations.
 *
 * Usage:
 *   import { setupDragAndDrop } from './lib/drag-drop-manager';
 *   const miniRow = document.getElementById('mini-terminal-row');
 *   setupDragAndDrop(miniRow, state, saveCurrentConfig);
 */

// Drag state (uses configId for stable identification)
let draggedConfigId: string | null = null;
let dropTargetConfigId: string | null = null;
let dropInsertBefore = false;
let isDragging = false;

/**
 * Set up drag-and-drop event handlers on the mini terminal row
 *
 * @param miniRow - The mini terminal row DOM element
 * @param state - Application state instance
 * @param saveConfig - Function to save configuration after reorder
 */
export function setupDragAndDrop(
  miniRow: HTMLElement | null,
  state: AppState,
  saveConfig: () => Promise<void>
): void {
  if (!miniRow) return;

  // HTML5 drag events for visual feedback
  miniRow.addEventListener("dragstart", (e) => {
    const target = e.target as HTMLElement;
    const card = target.closest(".terminal-card") as HTMLElement;

    if (card) {
      isDragging = true;
      draggedConfigId = card.getAttribute("data-terminal-id"); // Will be configId after Phase 3
      card.classList.add("dragging");

      if (e.dataTransfer) {
        e.dataTransfer.effectAllowed = "move";
        e.dataTransfer.setData("text/html", card.innerHTML);
      }
    }
  });

  miniRow.addEventListener("dragend", (e) => {
    // Perform reorder if valid (uses configId for state operation)
    if (draggedConfigId && dropTargetConfigId && dropTargetConfigId !== draggedConfigId) {
      state.reorderTerminal(draggedConfigId, dropTargetConfigId, dropInsertBefore);
      saveConfig();
    }

    // Select the terminal that was dragged (uses configId for state operation)
    if (draggedConfigId) {
      state.setPrimary(draggedConfigId);
    }

    // Cleanup
    const target = e.target as HTMLElement;
    const card = target.closest(".terminal-card");
    if (card) {
      card.classList.remove("dragging");
    }

    document.querySelectorAll(".drop-indicator").forEach((el) => el.remove());
    draggedConfigId = null;
    dropTargetConfigId = null;
    dropInsertBefore = false;
    isDragging = false;
  });

  // dragover for tracking position and showing indicator
  miniRow.addEventListener("dragover", (e) => {
    e.preventDefault();
    if (e.dataTransfer) {
      e.dataTransfer.dropEffect = "move";
    }

    if (!isDragging || !draggedConfigId) return;

    const target = e.target as HTMLElement;
    const card = target.closest(".terminal-card") as HTMLElement;

    if (card && card.getAttribute("data-terminal-id") !== draggedConfigId) {
      const targetId = card.getAttribute("data-terminal-id"); // Will be configId after Phase 3

      // Remove old indicators
      document.querySelectorAll(".drop-indicator").forEach((el) => el.remove());

      // Calculate if we should insert before or after
      const rect = card.getBoundingClientRect();
      const midpoint = rect.left + rect.width / 2;
      const insertBefore = e.clientX < midpoint;

      // Store drop target info (configId)
      dropTargetConfigId = targetId;
      dropInsertBefore = insertBefore;

      // Create and position insertion indicator - insert at wrapper level
      const wrapper = card.parentElement;
      const indicator = document.createElement("div");
      indicator.className =
        "w-1 h-32 my-1 bg-blue-500 rounded flex-shrink-0 pointer-events-none animate-pulse";
      wrapper?.parentElement?.insertBefore(indicator, insertBefore ? wrapper : wrapper.nextSibling);
    } else if (!card) {
      // In empty space - find all cards and determine position
      const allCards = Array.from(miniRow.querySelectorAll(".terminal-card")) as HTMLElement[];
      const lastCard = allCards[allCards.length - 1];

      if (lastCard && !lastCard.classList.contains("dragging")) {
        const lastId = lastCard.getAttribute("data-terminal-id"); // Will be configId after Phase 3
        if (lastId && lastId !== draggedConfigId) {
          // Remove old indicators
          document.querySelectorAll(".drop-indicator").forEach((el) => el.remove());

          // Drop after the last card
          dropTargetConfigId = lastId;
          dropInsertBefore = false;

          // Create and position insertion indicator after last card - insert at wrapper level
          const wrapper = lastCard.parentElement;
          const indicator = document.createElement("div");
          indicator.className =
            "w-1 h-32 my-1 bg-blue-500 rounded flex-shrink-0 pointer-events-none animate-pulse";
          wrapper?.parentElement?.insertBefore(indicator, wrapper?.nextSibling || null);
        }
      }
    }
  });
}
