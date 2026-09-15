import { applyD1Migrations, env, reset } from "cloudflare:test";
import { afterEach } from "vitest";

// Runs once before the test suite: applies migrations/0001_init.sql (and any
// future migration) to the isolated in-memory D1 instance vitest-pool-workers
// spins up per test file, so every test starts from the real schema.
await applyD1Migrations(env.DB, env.TEST_MIGRATIONS);

// `@cloudflare/vitest-pool-workers@0.22.0` (vitest v4) runs every test file
// against a single, long-lived Miniflare/workerd instance rather than
// spinning up a fresh one per test — per-test storage isolation is no longer
// automatic (the old push/pop "stacked storage" snapshot mechanism doesn't
// exist in this version). `reset()` from `cloudflare:test` is the documented
// v4 replacement ("[d]eletes all data from all attached bindings ... useful
// for resetting state between test blocks") — it wipes D1, the FLEET_STATE
// Durable Object, and any other bindings back to empty, so we re-run
// migrations immediately after to restore the schema before the next test.
afterEach(async () => {
  await reset();
  await applyD1Migrations(env.DB, env.TEST_MIGRATIONS);
});
