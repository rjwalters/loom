import { invoke } from "@tauri-apps/api/core";

/**
 * Wait for Tauri to be ready before initializing the app.
 * Tries to actually invoke a Tauri command to verify IPC works.
 *
 * NOTE: This function runs BEFORE the logging system is initialized,
 * so we intentionally use console.log/error for debugging.
 */
export async function waitForTauri(): Promise<void> {
  const maxAttempts = 30;
  const delayMs = 200;

  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    try {
      // Try to invoke a simple Tauri command
      // This will throw if Tauri IPC isn't ready
      await invoke("get_stored_workspace");
      // biome-ignore lint/suspicious/noConsole: Bootstrap logging before logger is initialized
      console.log(`[Loom] Tauri IPC ready after ${attempt} attempt(s)`);

      // Also test the console logging command specifically
      try {
        await invoke("append_to_console_log", { message: "[TEST] Tauri IPC verified\n" });
        // biome-ignore lint/suspicious/noConsole: Bootstrap logging before logger is initialized
        console.log("[Loom] append_to_console_log command verified working");
      } catch (testError) {
        // biome-ignore lint/suspicious/noConsole: Bootstrap logging before logger is initialized
        console.error("[Loom] append_to_console_log command failed:", testError);
      }

      return;
    } catch (error: unknown) {
      // Log every attempt to help debug
      // biome-ignore lint/suspicious/noConsole: Bootstrap logging before logger is initialized
      console.log(`[Loom] Tauri wait attempt ${attempt}/${maxAttempts}:`, error);

      // Check if it's a "command not found" error (IPC works but command doesn't exist)
      // vs an IPC error (Tauri not ready)
      const errorStr = String(error);
      if (errorStr.includes("command") || errorStr.includes("not found")) {
        // IPC is working, just the command might not exist (shouldn't happen but let's be safe)
        // biome-ignore lint/suspicious/noConsole: Bootstrap logging before logger is initialized
        console.log(`[Loom] Tauri IPC ready after ${attempt} attempt(s) (command exists)`);
        return;
      }

      // If this is the last attempt, throw error
      if (attempt === maxAttempts) {
        const errorMsg = `Tauri IPC not available after ${maxAttempts} attempts (${(maxAttempts * delayMs) / 1000}s)`;
        // biome-ignore lint/suspicious/noConsole: Bootstrap logging before logger is initialized
        console.error(`[Loom] ${errorMsg}`, error);
        throw new Error(errorMsg);
      }

      // Wait before next attempt
      await new Promise((resolve) => setTimeout(resolve, delayMs));
    }
  }
}
