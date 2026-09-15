import path from "node:path";
import { cloudflareTest, readD1Migrations } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

export default defineConfig(async () => {
  // Load the same migrations `wrangler d1 migrations apply` would run
  // against a real database, so tests exercise the real schema — see
  // `test/apply-migrations.ts`.
  const migrationsPath = path.join(__dirname, "migrations");
  const migrations = await readD1Migrations(migrationsPath);

  return {
    plugins: [
      cloudflareTest({
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
      }),
    ],
    test: {
      // Backend tests only. Without this, Vitest's default glob also picks up
      // `web/test/**` — the browser UI suite — and tries to run it inside the
      // Workers runtime, where there is no DOM. The UI has its own runner
      // (`web/vite.config.ts`, happy-dom): `npm run test:web`, or
      // `npm run check:all` for both.
      include: ["test/**/*.test.ts"],
      setupFiles: ["./test/apply-migrations.ts"],
      // `@cloudflare/vitest-pool-workers@0.22.0`'s workerd runtime
      // misreports a specific class of already-caught rejection as
      // "unhandled": an async function that `return`s (not `await`s) a
      // promise which then rejects *synchronously*, one level down, before
      // its first await — jose's `verifyCompact()` does exactly this when
      // `jwtVerify()` rejects a JWT for an unaccepted `alg` header
      // (`JOSEAlgNotAllowed`/`ERR_JOSE_ALG_NOT_ALLOWED`). The rejection is
      // properly caught by `validateAccessJwt()`'s own try/catch (every
      // affected test asserts the caught, resolved `null` result and
      // passes) — confirmed absent under the previous
      // `vitest@3.2.4`/`@cloudflare/vitest-pool-workers@0.10.15` pair with
      // an otherwise-identical minimal repro (a two-hop `return`-without-
      // `await` async chain, no jose involved), so this is a runtime/pool
      // regression, not a bug in our code or in jose. Filtering by the
      // specific jose error code (rather than
      // `dangerouslyIgnoreUnhandledErrors: true`) keeps every other class of
      // unhandled error fatal. See issue #7663.
      onUnhandledError(error) {
        if ((error as { code?: string }).code === "ERR_JOSE_ALG_NOT_ALLOWED") {
          return false;
        }
      },
    },
  };
});
