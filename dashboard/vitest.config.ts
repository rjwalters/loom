import path from "node:path";
import { defineWorkersConfig, readD1Migrations } from "@cloudflare/vitest-pool-workers/config";

export default defineWorkersConfig(async () => {
  // Load the same migrations `wrangler d1 migrations apply` would run
  // against a real database, so tests exercise the real schema — see
  // `test/apply-migrations.ts`.
  const migrationsPath = path.join(__dirname, "migrations");
  const migrations = await readD1Migrations(migrationsPath);

  return {
    test: {
      // Backend tests only. Without this, Vitest's default glob also picks up
      // `web/test/**` — the browser UI suite — and tries to run it inside the
      // Workers runtime, where there is no DOM. The UI has its own runner
      // (`web/vite.config.ts`, happy-dom): `npm run test:web`, or
      // `npm run check:all` for both.
      include: ["test/**/*.test.ts"],
      setupFiles: ["./test/apply-migrations.ts"],
      poolOptions: {
        workers: {
          wrangler: { configPath: "./wrangler.toml" },
          miniflare: {
            bindings: {
              // Exposed to the test runtime as `env.TEST_MIGRATIONS`, applied
              // in the setup file.
              TEST_MIGRATIONS: migrations,
              // `ADMIN_TOKEN` is a real deployment's `wrangler secret` (never
              // committed) — this fixed value only exists in the test
              // runtime so admin-route tests have something deterministic
              // to authenticate against.
              ADMIN_TOKEN: "test-admin-token",
              // Single-URL dashboard root (issue #4795, src/accessAuth.ts).
              // Fixed test-only values so `GET /` integration tests can
              // exercise the authenticated branch — see test/index.test.ts,
              // which signs a JWT for this exact team domain/aud and mocks
              // the JWKS fetch to this exact team domain's certs URL.
              CF_ACCESS_TEAM_DOMAIN: "test-team.cloudflareaccess.com",
              CF_ACCESS_AUD: "test-login-app-aud-tag",
            },
          },
        },
      },
    },
  };
});
