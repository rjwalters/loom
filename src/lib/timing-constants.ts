/**
 * Timing Constants
 *
 * Centralized definitions for all timeout/delay values used throughout the app.
 * Having named constants makes code more readable and easier to adjust.
 */

/**
 * Delay after sending terminal input before sending next command.
 * Prevents command concatenation in the terminal buffer.
 */
export const TERMINAL_COMMAND_DELAY_MS = 300;

/**
 * Delay to allow terminal output to stabilize before reading.
 * Used after launching processes or sending commands that produce output.
 */
export const TERMINAL_OUTPUT_STABILIZATION_MS = 500;

/**
 * Short polling interval for checking terminal state changes.
 * Used in tight loops checking for specific conditions.
 */
export const TERMINAL_POLL_INTERVAL_MS = 100;

/**
 * Delay to allow Claude Code worker to fully initialize.
 * Gives the process time to start, load configuration, and be ready for input.
 */
export const WORKER_INITIALIZATION_DELAY_MS = 2000;

/**
 * Delay before removing toast notifications.
 * Allows fade-out animation to complete smoothly.
 */
export const TOAST_FADEOUT_DELAY_MS = 300;

/**
 * Delay to allow modal to render before interacting with elements.
 * Prevents race conditions with DOM element availability.
 */
export const MODAL_RENDER_DELAY_MS = 1000;
