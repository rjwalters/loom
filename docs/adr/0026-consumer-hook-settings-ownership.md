# ADR-0026: Consumer Hook Settings Ownership: Non-Mutating Surfaces Only

## Status

Proposed (2026-10-15). Stays Proposed until the Claude Code settings-merge
semantics in "Open verification" below are cited or proven by a hermetic
fixture. Records the decision of #11514, split from #11110.

## Context

Resync (`defaults/scripts/resync-installed.sh`) refreshes the installed
`.loom/hooks/` and `.loom/scripts/` of a consumer repo, but never manages the
consumer-owned `.claude/settings.json`. PR #11082 (#11049) avoided a roll-pause
gap by injecting the hook wiring when a daemon-dispatched session is launched.
That fixed roll-pause itself and left the general boundary undecided for any
future hook that the launch-time path cannot carry.

Loom already runs three surfaces, each with one owner:

| Surface | Owner | Where |
|---|---|---|
| Launch-time `--settings <json>` for daemon-dispatched sessions, never persisted | Loom | `defaults/scripts/spawn-claude.sh` (roll-pause wiring, #11049) |
| User-scope `~/.claude/settings.json` entries, identified by the `/defaults/hooks/<name>` command substring | Loom, machine-level | `scripts/install/provision-hooks.sh`: `provision_loom_hooks`, `deprovision_loom_hooks`, `provision_operator_presence`; backup via `_phook_backup_settings` with bounded retention |
| Project `.claude/settings.json` | The consumer repo. The installer asserts `.loom/hooks/<name>` entries only for repos that still carry per-repo hook copies (`ensure_project_hook_wiring`); resync only warns (`report_unwired_hooks`, "repo-owned and was NOT modified") | `scripts/install/provision-hooks.sh`, `defaults/scripts/resync-installed.sh` |

Line numbers drift; re-grep each symbol above rather than trusting a number.

## Decision

**A Loom hook that cannot be delivered by a non-mutating surface is not
delivered. Resync never writes the consumer's `.claude/settings.json`.**

- **Project scope** is consumer-owned. Loom reports drift
  (`report_unwired_hooks`) and does not edit.
- **Loom-owned delivery** is the launch-time `--settings` path (per session,
  nothing persisted) and the user-scope entries (per machine). Both are
  Loom's to write and remove.
- The installer's existing additive project wiring stays as it is, for repos
  that still carry per-repo hook copies. This ADR adds no new project-scope
  writer.

### Required behaviours

- **Conflict handling.** A Loom entry is recognised only by the
  `/defaults/hooks/` substring (user scope) or `.loom/hooks/` (project scope).
  A consumer entry with the same event and matcher is left alone, and both run.
- **Convergence.** Provisioning is additive and idempotent: an entry already
  present is not duplicated.
- **Uninstall and removal.** User scope is removed only by a machine-level
  teardown (`deprovision_loom_hooks`), because the entries are shared by every
  repo on the machine. A per-repo uninstall strips only `.loom/hooks/`-prefixed
  project entries (`scripts/uninstall-loom.sh`, the `.claude/settings.json`
  smart-removal step) and leaves every other hook and permission.
- **Malformed settings.** Installer paths refuse to touch a file that is not
  valid JSON, and a backup precedes any mutation. The consumer file is never
  rewritten from scratch, so there is no data-loss path. Known gap:
  `report_unwired_hooks` runs `jq -e` per hook and counts a jq failure as
  "unwired", so a malformed file prints a misleading "unwired" list instead of
  "malformed". This was read in the code and not run.
- **Concurrent edits.** Loom's writers (backup, then read-modify-write of the
  user-scope file) take no lock against an editor. The backup is the recovery
  path. Because the project file is never written by resync, a consumer edit
  there cannot race a resync.
- **Upgrade from installer-written entries.** `_phook_merge_one` with
  `upgrade_marker` rewrites a stale Loom wrapper in place (#4806, #6544) and
  leaves hand-authored entries alone.

## Consequences

### Positive

- Resync never dirties a committed consumer file, and cannot clobber
  consumer edits.
- The boundary matches what the code does today, so the ADR needs no migration.
- A new hook has a clear rule: ship it through `--settings` or user scope, or
  report it as unwired.

### Negative

- A hook that only works when registered in the project file is not delivered
  automatically; the consumer must add it after reading the resync warning.
- Correctness depends on Claude Code combining hooks across user, project and
  `--settings` sources (see below).
- Interactive sessions that bypass `spawn-claude.sh` get only user-scope hooks.

## Alternatives Considered

1. **Fully unmanaged consumer settings with runtime injection.** Chosen as the
   rule for project scope. It is what `report_unwired_hooks` and `--settings`
   already do.
2. **Resync-managed delimited block.** Rejected. JSON has no comments, so the
   only delimiter is the command-substring marker, which makes "preserve every
   consumer entry" depend on a string match (`ensure_project_hook_wiring`
   already documents a false-positive rewrite of hand-authored entries).
   Resync would dirty a committed file on every release, and a consumer edit
   during resync is a read-modify-write race with no lock.
3. **A separate generated surface merged by Claude Code.** Adopted: it already
   exists twice, as `--settings` (session) and user-scope settings (machine).

## Open verification

The repo's design (the "transition dedup" in `provision-hooks.sh`) assumes
Claude Code combines, rather than overrides, hooks from the user, project and
`--settings` sources. That has been inferred from the repo and not tested. Before
this ADR is marked Accepted, cite Claude Code's settings documentation for the
merge semantics, or add a hermetic fixture showing that hooks from all three
sources fire.

## Possible follow-ups (not part of this change)

- Make `report_unwired_hooks` distinguish "malformed settings" from "unwired".

## References

- Related GitHub Issues: #11514, #11110 (parent), #11049 / PR #11082
  (roll-pause launch-time wiring), #11117 (umbrella), #11033 (drift report),
  #4806, #6544
- Related docs: [Runtime Adapter Contract](../../defaults/docs/runtime-adapters.md)
  § "Hook settings ownership"
