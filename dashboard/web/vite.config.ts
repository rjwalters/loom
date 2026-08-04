// `vitest/config`'s `defineConfig` is Vite's own, widened with the `test`
// block below — one config file drives dev server, production build, and the
// test runner rather than three that can silently drift apart.
import { defineConfig } from "vitest/config";

/**
 * Build + dev-server config for the fleet dashboard UI.
 *
 * `build.outDir` is `dist/`, which is exactly what the sibling Worker's
 * `wrangler.toml` `[assets] directory = "./web/dist"` uploads — so
 * `npm run build` here is the only step between a source edit and
 * `wrangler deploy` in `../`.
 *
 * The dev server proxies the API routes to a locally running `wrangler dev`
 * (`npm run dev` in `../`, default port 8787) so `fetch("/api/fleet-state")`
 * is same-origin in development exactly as it is in production. That keeps
 * `API_BASE` empty in both, which is why the UI never needs CORS handling or
 * credential forwarding.
 */
export default defineConfig({
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // A tiny internal dashboard: readable output beats a few saved bytes when
    // someone has to debug it from a browser devtools pane on a live deploy.
    sourcemap: true,
  },
  server: {
    port: 5173,
    proxy: {
      "/api": { target: "http://127.0.0.1:8787", changeOrigin: true },
      "/public": { target: "http://127.0.0.1:8787", changeOrigin: true },
    },
  },
  test: {
    environment: "happy-dom",
    include: ["test/**/*.test.ts"],
  },
});
