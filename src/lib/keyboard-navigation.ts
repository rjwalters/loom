/**
 * Keyboard Navigation Support
 *
 * Provides keyboard navigation for terminal tabs and other interactive elements
 */

import type { AppState } from "./state";

/**
 * Initialize keyboard navigation for terminal tabs
 * Enables arrow key navigation between terminal cards
 */
export function initializeKeyboardNavigation(state: AppState): void {
  document.addEventListener("keydown", (e) => {
    // Only handle arrow keys when focused on terminal cards
    const activeElement = document.activeElement;
    if (!activeElement || !activeElement.classList.contains("terminal-card")) {
      return;
    }

    const terminalId = activeElement.getAttribute("data-terminal-id");
    if (!terminalId) return;

    const terminals = state.getTerminals();
    const currentIndex = terminals.findIndex((t) => t.id === terminalId);

    if (currentIndex === -1) return;

    let newIndex = currentIndex;

    // Arrow key navigation
    switch (e.key) {
      case "ArrowLeft":
        e.preventDefault();
        newIndex = currentIndex > 0 ? currentIndex - 1 : terminals.length - 1;
        break;
      case "ArrowRight":
        e.preventDefault();
        newIndex = currentIndex < terminals.length - 1 ? currentIndex + 1 : 0;
        break;
      case "Home":
        e.preventDefault();
        newIndex = 0;
        break;
      case "End":
        e.preventDefault();
        newIndex = terminals.length - 1;
        break;
      case "Enter":
      case " ":
        // Activate terminal on Enter or Space
        e.preventDefault();
        state.setPrimary(terminalId);
        return;
      default:
        return;
    }

    // Focus new terminal card
    const newTerminalId = terminals[newIndex]?.id;
    if (newTerminalId) {
      const newCard = document.querySelector(
        `.terminal-card[data-terminal-id="${newTerminalId}"]`
      ) as HTMLElement;
      if (newCard) {
        newCard.focus();
      }
    }
  });
}

/**
 * Handle Escape key to close modals
 * This is a global handler that closes the topmost modal
 */
export function initializeModalEscapeHandler(): void {
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;

    // Find all visible modals
    const modals = Array.from(document.querySelectorAll('[role="dialog"]')).filter(
      (modal) => !modal.classList.contains("hidden") && modal.getAttribute("aria-modal") === "true"
    );

    // Close the topmost modal
    if (modals.length > 0) {
      const topModal = modals[modals.length - 1] as HTMLElement;
      const closeBtn = topModal.querySelector(
        '[id$="-close-btn"], [class*="close"]'
      ) as HTMLElement;
      if (closeBtn) {
        closeBtn.click();
      } else {
        // If no close button, remove the modal's parent container
        topModal.closest(".fixed")?.remove();
      }
    }
  });
}
