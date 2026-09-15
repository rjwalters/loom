// Declaration-merges this project's `Env` (src/index.ts) plus the
// test-only `TEST_MIGRATIONS` binding (see vitest.config.ts) into the
// ambient `Cloudflare.Env` namespace interface, which is what
// `import { env } from "cloudflare:test"` is typed against as of
// `@cloudflare/vitest-pool-workers@0.22.0` — the previous
// `declare module "cloudflare:test" { interface ProvidedEnv }` merge point
// was dropped in that version's types (`env` is now typed `Cloudflare.Env`
// directly; see `@cloudflare/workers-types`' `declare namespace Cloudflare`
// for the intended per-project augmentation pattern), see issue #7663.
import type { Env as WorkerEnv } from "../src/index";

// This file has a top-level `import`, making it a module — augmenting the
// ambient global `Cloudflare` namespace from inside a module requires an
// explicit `declare global` block, or the `declare namespace` below would
// just create a new, module-local `Cloudflare` namespace that never merges
// with the real one.
declare global {
  namespace Cloudflare {
    interface Env extends WorkerEnv {
      TEST_MIGRATIONS: D1Migration[];
    }
  }
}
