/**
 * The `DispatchSweep` IPC payload the `dispatch_sweep` tool sends. A sibling
 * of `sweeps.ts` (which is over the file-size ratchet,
 * `.loom/docs/file-size-policy.md`) so the wire shape is unit-testable
 * without a daemon socket.
 */

import type { SweepKind } from "./sweeps.js";
import { dispatchOptionBodyFields, type DispatchOptions } from "./sweep-dispatch-options.js";

/** The `dispatch_sweep` tool's arguments. */
export interface DispatchSweepArgs extends DispatchOptions {
  kind: SweepKind;
  idempotency_key?: string;
  model?: string;
  effort?: string;
  depends_on?: number;
  workspace_root?: string;
}

/**
 * The `DispatchSweep` IPC payload for the `dispatch_sweep` tool's arguments.
 * Exported so the wire shape is unit-testable without a daemon socket.
 */
export function buildDispatchSweepPayload(args: DispatchSweepArgs): Record<string, unknown> {
  return {
    kind: args.kind,
    idempotency_key: args.idempotency_key ?? null,
    // Issue #3477 (Phase 1): optional model override. `null` (the
    // default) means the daemon emits NO --model flag and the spawned
    // child inherits the session/CLI default.
    model: args.model ?? null,
    // Issue #3716: optional reasoning-effort override, mirroring `model`.
    // `null` (the default) means the daemon emits NO --effort flag and
    // the spawned child inherits the session-default effort.
    effort: args.effort ?? null,
    // Issue #3729 (stacked-PR v1): optional single parent issue. `null`
    // (the default) means the daemon emits NO --depends-on flag and the
    // child branches off the default branch as usual. When set, the child
    // branches its worktree/PR off `feature/issue-<depends_on>`.
    depends_on: args.depends_on ?? null,
    // Issue #3929: optional target managed-workspace root. `null` (the
    // default) dispatches into the daemon's default workspace, exactly as
    // before; a value routes the sweep into that managed repo's registry.
    workspace_root: args.workspace_root ?? null,
    // `force` (#4317) and runtime/model_profile (#11284): see
    // sweep-dispatch-options.ts. As there, `force` MUST default to `false`
    // (never `null`/omitted: `null` fails serde deserialization on every
    // dispatch); `runtime` / `model_profile` default to `null`, keeping the
    // role policy.
    ...dispatchOptionBodyFields(args),
    // Issue #11444: a person (or the agent session acting for one) called
    // this tool, so the sweep records `dispatch_origin=operator`. The daemon
    // records an absent origin as `unknown`, never `operator`, so this must
    // be explicit.
    dispatch_origin: "operator",
  };
}
