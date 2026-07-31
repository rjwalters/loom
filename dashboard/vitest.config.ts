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
            },
          },
        },
      },
    },
  };
});
