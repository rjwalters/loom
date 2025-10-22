/**
 * Screen Reader Announcements
 *
 * Provides ARIA live regions for announcing dynamic content changes
 */

/**
 * Initialize the screen reader announcement system
 * Creates a hidden aria-live region for announcements
 */
export function initializeScreenReaderAnnouncer(): void {
  // Create live region if it doesn't exist
  let liveRegion = document.getElementById("sr-announcements");

  if (!liveRegion) {
    liveRegion = document.createElement("div");
    liveRegion.id = "sr-announcements";
    liveRegion.setAttribute("role", "status");
    liveRegion.setAttribute("aria-live", "polite");
    liveRegion.setAttribute("aria-atomic", "true");
    liveRegion.className = "sr-only";
    document.body.appendChild(liveRegion);
  }
}

/**
 * Announce a message to screen readers
 * @param message - The message to announce
 * @param priority - "polite" (default) or "assertive" for urgent announcements
 */
export function announce(message: string, priority: "polite" | "assertive" = "polite"): void {
  const liveRegion = document.getElementById("sr-announcements");
  if (!liveRegion) {
    console.warn("Screen reader announcer not initialized");
    return;
  }

  // Update aria-live priority
  liveRegion.setAttribute("aria-live", priority);

  // Clear and set new message
  liveRegion.textContent = "";
  setTimeout(() => {
    liveRegion.textContent = message;
  }, 100);
}

/**
 * Announce terminal status changes
 */
export function announceTerminalStatusChange(terminalName: string, status: string): void {
  announce(`Terminal ${terminalName} is now ${status}`);
}

/**
 * Announce workspace changes
 */
export function announceWorkspaceChange(workspacePath: string, loaded: boolean): void {
  if (loaded) {
    announce(`Workspace loaded: ${workspacePath}`);
  } else {
    announce(`Workspace unloaded`);
  }
}

/**
 * Announce terminal selection
 */
export function announceTerminalSelection(terminalName: string): void {
  announce(`Selected terminal: ${terminalName}`);
}

/**
 * Announce terminal creation
 */
export function announceTerminalCreated(terminalName: string): void {
  announce(`Terminal created: ${terminalName}`);
}

/**
 * Announce terminal removal
 */
export function announceTerminalRemoved(terminalName: string): void {
  announce(`Terminal removed: ${terminalName}`);
}
