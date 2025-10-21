/**
 * Vitest setup file - runs before all tests
 *
 * Configures:
 * - WebCrypto polyfill for happy-dom (required by Tauri API)
 * - Tauri API mocks for testing IPC calls
 */

import { webcrypto } from "node:crypto";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll } from "vitest";

// Add WebCrypto to global scope (required by Tauri API)
// happy-dom doesn't provide crypto.subtle, but Node.js does
beforeAll(() => {
  // Use defineProperty because global.crypto is read-only in happy-dom
  Object.defineProperty(global, "crypto", {
    value: webcrypto,
    writable: true,
    configurable: true,
  });
});

// Initialize Tauri IPC mocks globally
// This allows all tests to use mocked Tauri commands
beforeAll(() => {
  mockIPC((cmd, _payload) => {
    // Default mock responses for common Tauri commands
    // Individual tests can override with mockIPC in beforeEach

    switch (cmd) {
      case "validate_git_repo":
        // Default: assume valid git repo for testing
        return Promise.resolve(true);

      case "read_text_file":
        // Default: return empty config
        return Promise.resolve("{}");

      case "write_text_file":
        // Default: successful write
        return Promise.resolve();

      case "exists":
        // Default: file exists
        return Promise.resolve(true);

      case "create_dir":
        // Default: successful directory creation
        return Promise.resolve();

      default:
        // For unknown commands, return null
        // (Unmocked commands will return null silently)
        return Promise.resolve(null);
    }
  });
});
