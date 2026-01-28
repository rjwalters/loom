/**
 * Vitest setup file - runs before all tests
 *
 * Configures:
 * - WebCrypto polyfill for happy-dom (required by Tauri API)
 * - localStorage mock for happy-dom (required for theme tests)
 * - Tauri API mocks for testing IPC calls
 */

import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll } from "vitest";

// Add WebCrypto to global scope (required by Tauri API)
// happy-dom doesn't provide crypto.subtle, but Node.js does
beforeAll(async () => {
  // @ts-expect-error - node:crypto is available at runtime but not in types
  const { webcrypto } = await import("node:crypto");
  // Use defineProperty because global.crypto is read-only in happy-dom
  Object.defineProperty(globalThis, "crypto", {
    value: webcrypto,
    writable: true,
    configurable: true,
  });
});

// Mock localStorage for happy-dom (required by theme tests)
// happy-dom provides localStorage but with incomplete API - we enhance it
beforeAll(() => {
  const store: Record<string, string> = {};
  const localStorageMock: Storage = {
    getItem: (key: string) => store[key] ?? null,
    setItem: (key: string, value: string) => {
      store[key] = value;
    },
    removeItem: (key: string) => {
      delete store[key];
    },
    clear: () => {
      for (const key of Object.keys(store)) {
        delete store[key];
      }
    },
    get length() {
      return Object.keys(store).length;
    },
    key: (index: number) => Object.keys(store)[index] ?? null,
  };

  Object.defineProperty(globalThis, "localStorage", {
    value: localStorageMock,
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
