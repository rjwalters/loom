import { defineConfig, devices } from "@playwright/test";

/**
 * Playwright configuration for Loom E2E tests.
 *
 * Tests run against the Vite dev server (not the full Tauri app) to enable
 * faster iteration. For full Tauri integration tests, use tauri:preview.
 *
 * @see https://playwright.dev/docs/test-configuration
 */
export default defineConfig({
  testDir: "./e2e",
  fullyParallel: false, // Sequential execution for Tauri/stateful tests
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: 1, // Single worker to avoid race conditions
  reporter: [["html", { open: "never" }], ["list"]],

  use: {
    baseURL: "http://localhost:1420",
    trace: "on-first-retry",
    screenshot: "only-on-failure",
    video: "on-first-retry",
  },

  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],

  webServer: {
    command: "pnpm dev",
    url: "http://localhost:1420",
    reuseExistingServer: !process.env.CI,
    timeout: 120 * 1000, // 2 minutes for initial build
    stdout: "pipe",
    stderr: "pipe",
  },

  // Increase default timeout for E2E tests
  timeout: 60 * 1000, // 1 minute per test
  expect: {
    timeout: 10 * 1000, // 10 seconds for assertions
  },
});
