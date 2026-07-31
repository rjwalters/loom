import { defineConfig } from "vitest/config";

// Default environment is plain `node` — the SSE client and timeline builder
// are DOM-free logic modules. Component tests that touch `document` opt in
// to jsdom per-file via a `// @vitest-environment jsdom` pragma, matching
// vitest's documented per-test-file environment override.
export default defineConfig({
  test: {
    environment: "node",
  },
});
