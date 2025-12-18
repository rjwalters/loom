import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    globals: true,
    environment: "happy-dom",
    include: ["src/**/*.test.ts"],
    setupFiles: ["./src/test/setup.ts"],
    coverage: {
      provider: "v8",
      reporter: ["text", "json", "html"],
      include: ["src/lib/**/*.ts"],
      exclude: [
        "src/**/*.test.ts",
        "src/test/**",
        // UI/Modal files - difficult to unit test, should be tested via E2E
        "src/lib/*-modal.ts",
        "src/lib/ui.ts",
        "src/lib/tarot-cards.ts",
        "src/lib/tooltip.ts",
        "src/lib/keyboard-navigation.ts",
        // Lifecycle/initialization - tested via integration
        "src/lib/config-initializer.ts",
        "src/lib/terminal-lifecycle.ts",
        "src/lib/workspace-lifecycle.ts",
        "src/lib/workspace-start.ts",
        "src/lib/workspace-reset.ts",
        "src/lib/desktop-manager.ts",
        // Logging utilities
        "src/lib/console-logger.ts",
        // Test utilities
        "src/lib/test-utils/**",
      ],
      thresholds: {
        // Set to current coverage levels to prevent regressions
        // TODO: Gradually increase these as coverage improves
        lines: 40,
        functions: 75,
        branches: 90,
        statements: 40,
      },
    },
  },
});
