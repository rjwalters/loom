// Declaration-merges this project's `Env` (src/index.ts) plus the
// test-only `TEST_MIGRATIONS` binding (see vitest.config.ts) into
// `cloudflare:test`'s `ProvidedEnv`, which is what `import { env } from
// "cloudflare:test"` is typed against.
import type { Env as WorkerEnv } from "../src/index";

declare module "cloudflare:test" {
  interface ProvidedEnv extends WorkerEnv {
    TEST_MIGRATIONS: D1Migration[];
  }
}
