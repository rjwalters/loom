/**
 * Simple toast notification system
 *
 * Provides non-blocking toast notifications as a replacement for blocking alert() calls.
 * Supports success, error, and info message types with automatic dismiss.
 */

import { TOAST_FADEOUT_DELAY_MS } from "./timing-constants";

export type ToastType = "success" | "error" | "info";

/**
 * Show a toast notification
 *
 * @param message - The message to display
 * @param type - The type of toast (success, error, or info)
 * @param duration - How long to show the toast in milliseconds (default: 3000)
 */
export function showToast(
  message: string,
  type: ToastType = "info",
  duration: number = 3000
): void {
  // Create toast element
  const toast = document.createElement("div");
  toast.className = `toast toast-${type}`;
  toast.textContent = message;
  toast.setAttribute("role", "status");
  toast.setAttribute("aria-live", "polite");

  // Add to DOM
  document.body.appendChild(toast);

  // Trigger show animation (need small delay for CSS transition)
  setTimeout(() => toast.classList.add("show"), 10);

  // Auto-dismiss after duration
  setTimeout(() => {
    toast.classList.remove("show");
    // Remove from DOM after fade-out animation completes
    setTimeout(() => toast.remove(), TOAST_FADEOUT_DELAY_MS);
  }, duration);
}
