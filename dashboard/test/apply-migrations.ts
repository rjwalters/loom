import { applyD1Migrations, env } from "cloudflare:test";

// Runs once before the test suite: applies migrations/0001_init.sql (and any
// future migration) to the isolated in-memory D1 instance vitest-pool-workers
// spins up per test file, so every test starts from the real schema.
await applyD1Migrations(env.DB, env.TEST_MIGRATIONS);
