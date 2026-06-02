# Architect / Hermit Work-Generation Cadence After Daemon Deprecation

**Status:** Design memo (memo-only). Implementation deferred to follow-up issue.
**Authors:** Loom Builder (#3381)
**Tracks:** Epic #3372 (shepherd/daemon deprecation), Phase 2d.
**Related:** #3317 (original sweep-replaces-daemon proposal, Gap B), #3374 (Phase 1
spawn loop), #3375 (Phase 2a support-role workflows), #3378 (Phase 3 hard deletion).

---

## 1. Problem

The Loom daemon (`loom_tools.daemon_v2`) currently fires Architect and Hermit on a
**threshold-and-cooldown** model:

| Generator   | Fire condition                                                                                                        |
|-------------|------------------------------------------------------------------------------------------------------------------------|
| Architect   | `ready_count < ISSUE_THRESHOLD` AND `now - last_architect_trigger > ARCHITECT_COOLDOWN` AND `open_architect_proposals < MAX_PROPOSALS` |
| Hermit      | `ready_count < ISSUE_THRESHOLD` AND `now - last_hermit_trigger > HERMIT_COOLDOWN`   AND `open_hermit_proposals    < MAX_PROPOSALS` |

Defaults (from `loom_tools/daemon_v2/config.py`):

```
ISSUE_THRESHOLD      = 3       (LOOM_ISSUE_THRESHOLD)
MAX_PROPOSALS        = 5       (LOOM_MAX_PROPOSALS)
ARCHITECT_COOLDOWN   = 1800 s  (LOOM_ARCHITECT_COOLDOWN, = 30 min)
HERMIT_COOLDOWN      = 1800 s  (LOOM_HERMIT_COOLDOWN,    = 30 min)
```

Phase 2a (#3375) shipped GitHub Actions workflows for the **stateless** support roles
(Champion, Curator, Judge, Auditor, Guide) on simple cron schedules. Phase 2a
*deliberately* did not cover Architect/Hermit because their cadence is not stateless:
firing them on every cron tick would flood the backlog with proposals.

Phase 3 (#3378) will delete `loom_tools/daemon_v2/`. Without a replacement, proposal
generation silently stops working — the backlog can only grow via humans filing issues
or via Auditor on bug discovery. This memo specifies the replacement.

**Out of scope** (per issue #3381):

- Redesigning the gating model itself. Threshold-and-cooldown is preserved as-is.
- Replacing Architect or Hermit with a different proposal-generation mechanism.
- Champion / Auditor cadence (Phase 2a).

---

## 2. Options Evaluated

### Option A — Spawn loop owns the trigger

Bolt a per-tick threshold check onto `defaults/scripts/spawn-loop.sh` (Phase 1). After
the spawn step, read backlog count + cooldown state from `.loom/sweep-history.json` and
optionally `claude -p "/architect"` / `claude -p "/hermit"`.

```bash
# Pseudocode added to tick():
ready_count=$(list_ready_issues | wc -l)
if (( ready_count < ISSUE_THRESHOLD )); then
    last_arch=$(history_read last_architect_trigger)
    arch_props=$(gh issue list --label loom:architect --state open --json number --jq length)
    if (( now - last_arch > ARCHITECT_COOLDOWN )) && (( arch_props < MAX_PROPOSALS )); then
        nohup "$SPAWN_CLAUDE" -p "/architect" >> "$LOG/architect.log" 2>&1 &
        history_write last_architect_trigger "$now"
    fi
    # same shape for hermit
fi
```

**Pros**

- Single new control surface; no extra workflow files to maintain.
- Reuses the spawn loop's existing poll cycle (`POLL_INTERVAL=30s`) and token-rotation
  path (`spawn-claude.sh` picks an OAuth token; Architect/Hermit runs benefit from the
  same multi-account load distribution that sweep enjoys).
- Trigger fires exactly when capacity opens up — no fixed-cron offset between "backlog
  drained" and "next decision tick."
- Zero new infrastructure dependency: works on a laptop, no GitHub Actions secret
  required.

**Cons**

- **Mission creep.** The spawn loop's job today is "spawn one `/loom:sweep` per ready
  issue." Adding "decide whether more proposals are needed" makes the loop have an
  opinion about backlog depth. Phase 1's design note explicitly excluded this:

  > *"What this script does NOT do (Phase 2 territory): Work generation triggers
  > (Architect/Hermit/Auditor cadence)"* — `spawn-loop.sh:21-25`

- Couples work-generation cadence to whether the spawn loop is running. If an operator
  uses only the GitHub Actions support roles (no local spawn loop), proposals never
  generate.
- Per-tick `gh issue list --label loom:architect` API calls add cost to every spawn
  loop iteration, even when the backlog is full and no trigger is possible.

### Option B — Threshold-aware GitHub Actions workflows

Two new workflows mirroring Phase 2a's pattern (`.github/workflows/loom-architect.yml`,
`.github/workflows/loom-hermit.yml`). Each runs on a frequent schedule, gates on backlog
count + cooldown + open-proposal count, and only invokes the role when all three pass.

```yaml
# loom-architect.yml (sketch)
on:
  # schedule:
  #   - cron: "*/15 * * * *"      # commented by default like Phase 2a
  workflow_dispatch:

jobs:
  architect:
    runs-on: ubuntu-latest
    timeout-minutes: 10
    permissions:
      issues: write
      pull-requests: read
    steps:
      - uses: actions/checkout@v4
      - name: Gate on backlog + cooldown + proposal count
        id: gate
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          ISSUE_THRESHOLD: "3"
          ARCHITECT_COOLDOWN: "1800"
          MAX_PROPOSALS: "5"
        run: ./.loom/scripts/check-work-gen-gate.sh architect
      - name: Install Claude CLI
        if: steps.gate.outputs.should_fire == 'true'
        run: npm install -g @anthropic-ai/claude-code
      - name: Run architect
        if: steps.gate.outputs.should_fire == 'true'
        env:
          ANTHROPIC_API_KEY: ${{ secrets.CLAUDE_API_KEY }}
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: claude -p "/architect" --dangerously-skip-permissions
      - name: Persist last-trigger timestamp
        if: steps.gate.outputs.should_fire == 'true'
        run: ./.loom/scripts/check-work-gen-gate.sh architect --commit
```

**Pros**

- Architecturally clean: keeps the spawn loop pure (no backlog opinions) and aligns
  exactly with Phase 2a's already-shipped pattern. Operators learn one paradigm:
  "support-role cadence = workflow file."
- Decouples work generation from the spawn loop. A fork that only runs GitHub Actions
  (no local spawn loop) still gets proposals.
- Workflows are independently togglable: a fork can opt-in to Architect but not Hermit,
  or vice versa.
- `workflow_dispatch` button gives operators a "fire me now" trigger that respects the
  gate — useful for testing without waiting for the next tick.
- All workflows are **disabled by default** (schedule block commented) — matches
  Phase 2a's safety posture so forks don't burn Actions minutes accidentally.

**Cons**

- Two new workflow files to maintain. Total ship surface grows by ~80 LOC YAML +
  ~120 LOC bash (`check-work-gen-gate.sh`).
- State file (`.loom/sweep-history.json`) needs to be commit-and-pushed by the workflow
  to persist across runs (workflows are stateless). This adds a `git commit` step and
  requires write access — or an alternative state store (see §5).
- Cron-driven means there's a fixed-latency floor between "backlog drained" and
  "Architect fires." With `*/15 * * * *` that's up to 15 min — acceptable for proposal
  generation since proposals already take humans hours to react to anyway.
- More moving parts: `gh issue list` calls happen in the workflow runner (not the
  operator's laptop), which is a minor positive (no local rate-limit cost) but a minor
  negative (workflow runs cost Actions minutes even on no-op ticks).

---

## 3. Recommendation: **Option B**

**Rationale:**

1. **Consistency with Phase 2a.** Phase 2a already established the pattern — disabled
   cron workflows in `.github/workflows/loom-*.yml`. Operators have learned to look
   there for periodic support roles. Adding two more files extends a familiar paradigm
   rather than introducing a second one.

2. **Decoupling.** A fork that *only* uses GitHub Actions support roles (no local
   spawn loop) should still get proposal generation. Option A makes work generation
   depend on a long-running local process, which is the daemon's failure mode Phase 1
   explicitly set out to fix.

3. **Mission creep is real.** Phase 1's spawn-loop.sh:21-25 deliberately scopes out
   work-generation triggers as "Phase 2 territory." Reversing that decision in
   Phase 2d would re-couple two concerns the deprecation epic just disentangled.

4. **Cooldown latency is acceptable.** Proposal generation is not latency-sensitive
   — humans read proposals over the course of hours or days. A 15-minute cron tick is
   well within tolerance.

5. **Token rotation is not a regression.** Architect/Hermit in the daemon ran as
   single-account processes (the daemon used the operator's keychain token). Running
   them in a GitHub Action under a single `CLAUDE_API_KEY` secret is the same
   single-account behavior — no regression. Operators who want multi-account proposal
   generation can run a self-hosted runner with `spawn-claude.sh` invoked manually.

The Option A downside (mission creep) is structural; the Option B downside (workflow
file count) is cosmetic. Choose structure over cosmetics.

---

## 4. State File Schema

File path: `.loom/sweep-history.json` (gitignored — see §5).

```jsonc
{
  "version": 1,
  "last_architect_trigger": "2026-06-02T14:30:00Z",   // ISO8601 UTC, or null
  "last_hermit_trigger":    "2026-06-02T13:00:00Z",   // ISO8601 UTC, or null
  "last_architect_skip_reason": "max_proposals_reached", // optional, for debugging
  "last_hermit_skip_reason":    null,
  "history": [
    // Optional append-only audit trail (last 50 entries, FIFO).
    // Each entry records WHY a tick fired or skipped.
    {
      "timestamp": "2026-06-02T14:30:00Z",
      "role": "architect",
      "decision": "fire",
      "ready_count": 1,
      "open_proposals": 2
    },
    {
      "timestamp": "2026-06-02T14:15:00Z",
      "role": "architect",
      "decision": "skip",
      "reason": "cooldown_not_elapsed",
      "seconds_remaining": 1730
    }
  ]
}
```

**Schema notes:**

- `version: 1` lets future schema evolutions migrate gracefully.
- Same-file-separate-keys answers Open Question #2 (DRY: one read/write helper handles
  both roles).
- `history` is optional. If the gating script wants observability, it appends to this
  array and trims to the last 50 entries on every write. Drops it entirely if the file
  grows too large; the two `last_*_trigger` keys are sufficient for gating.
- Field absence is treated as "never fired" (cooldown automatically elapsed).

**Read/write contract:**

- All writes are atomic via `tmp + os.replace` (mirrors spawn-loop-state.json's pattern,
  see `spawn-loop.sh:120-136`).
- Reads tolerate a missing file (return defaults).
- No locking required — a single workflow run is the only writer per role; concurrent
  runs are prevented by GitHub Actions `concurrency:` groups in the workflow YAML.

---

## 5. State Persistence Across Workflow Runs

GitHub Actions runners are stateless. The state file must survive between ticks. Two
options:

### 5a. Commit-and-push to a `loom-state` orphan branch

```bash
# After updating .loom/sweep-history.json
git config user.email "loom-bot@users.noreply.github.com"
git config user.name "loom-bot"
git fetch origin loom-state || git checkout --orphan loom-state
git add .loom/sweep-history.json
git commit -m "chore(loom): update sweep history" || true   # no-op if no change
git push origin loom-state
```

**Pros:** zero external state. Audit trail in git history. Survives repo cloning.
**Cons:** Pollutes git with a state branch. Requires `contents: write` permission. Race
conditions if two workflow runs commit simultaneously (mitigated by `concurrency:`).

### 5b. GitHub Actions cache (`actions/cache`)

```yaml
- uses: actions/cache@v4
  with:
    path: .loom/sweep-history.json
    key: loom-sweep-history-${{ github.run_id }}
    restore-keys: |
      loom-sweep-history-
```

**Pros:** purpose-built for this. No extra branch. No commit noise.
**Cons:** Cache eviction policies could lose the state. Cache is best-effort — first
fire after eviction would skip the cooldown check (acceptable: it just means one
"missed" cooldown, not a regression).

**Recommendation:** **5a (orphan branch).** The state is small (~2 KB), evolves slowly
(every 30 min at most), and benefits from an explicit audit trail. Cache eviction
silently failing the cooldown check is a worse failure mode than a tiny `loom-state`
branch.

If 5a is too heavy for a fork, 5b is an acceptable fallback — the gate script can
read both and use whichever it finds, treating a missing file as "never fired."

**Gitignore:** `.loom/sweep-history.json` MUST be gitignored on the main branch (it's
runtime state, not source). The orphan branch is the only place it lives. Update
`.gitignore` and `defaults/.gitignore`.

---

## 6. Configuration Surface (Open Question #3)

Defaults are encoded in the gate script (`./.loom/scripts/check-work-gen-gate.sh`).
Operators can override per-workflow via env vars in the YAML:

```yaml
- name: Gate
  env:
    ISSUE_THRESHOLD: "5"          # override default 3
    ARCHITECT_COOLDOWN: "3600"    # override default 1800 (1 hour cooldown)
    MAX_PROPOSALS: "10"           # override default 5
  run: ./.loom/scripts/check-work-gen-gate.sh architect
```

This matches the daemon's `LOOM_*` env var convention while staying inside the workflow
file (no separate config file to keep in sync). The gate script honors:

- `ISSUE_THRESHOLD` (default 3)
- `ARCHITECT_COOLDOWN` (default 1800)
- `HERMIT_COOLDOWN` (default 1800)
- `MAX_PROPOSALS` (default 5)

For shared overrides across both workflows, an `.env`-style file or repo variable
(`vars.LOOM_ISSUE_THRESHOLD`) is a Phase 4 nice-to-have. Phase 2d ships with workflow-
local env blocks only.

---

## 7. Files Touched (when implementation lands)

| Path                                          | Change         |
|-----------------------------------------------|----------------|
| `.github/workflows/loom-architect.yml`        | **new** (~50 LOC YAML) |
| `.github/workflows/loom-hermit.yml`           | **new** (~50 LOC YAML, mirror of architect) |
| `.loom/scripts/check-work-gen-gate.sh`        | **new** (~100 LOC bash) |
| `defaults/scripts/check-work-gen-gate.sh`     | **new** (mirror of .loom/scripts/) |
| `defaults/scripts/tests/test-work-gen-gate.sh`| **new** (~80 LOC bash, mirrors test-spawn-loop.sh style) |
| `.gitignore`                                  | add `.loom/sweep-history.json` |
| `defaults/.gitignore`                         | add `.loom/sweep-history.json` |
| `CLAUDE.md`                                   | add §4 entry under "Scheduled Support Roles" describing the new workflows |
| `defaults/CLAUDE.md`                          | mirror of CLAUDE.md change |

Estimated total: ~280 LOC + 2 YAML files + docs. Within the issue's "~30-60 LOC of
trigger logic + ~50 LOC of state file management" envelope (the YAML and tests are
boilerplate, not algorithmic complexity).

---

## 8. Manual End-to-End Test Plan

This is the acceptance criterion's e2e test, restated for the chosen Option B.

**Preconditions:**
- A fork with `CLAUDE_API_KEY` secret set.
- `.github/workflows/loom-architect.yml` shipped with the `schedule:` block uncommented
  and `cron: "*/5 * * * *"` (5 min for faster feedback during the test).
- `loom:issue` count starts at 0 (empty backlog).
- `loom:architect` open proposal count starts at 0.
- `.loom/sweep-history.json` does not exist on `loom-state` branch (first run).

**Steps:**

1. **Empty backlog → Architect fires.**
   - Trigger `workflow_dispatch` on `loom-architect.yml`.
   - **Expect:** gate passes (`ready_count=0 < 3 && cooldown=elapsed && proposals=0 < 5`).
     Architect runs. New issue with `loom:architect` label appears within 10 min.
     `.loom/sweep-history.json` on `loom-state` branch has
     `last_architect_trigger: <now>`.

2. **Within cooldown → second tick does NOT fire.**
   - Wait 5 min (next scheduled tick) without filing any `loom:issue` issues.
   - **Expect:** workflow runs but gate fails on `cooldown_not_elapsed`
     (`now - last_architect_trigger < 1800`). No new proposal. Architect step skipped.
     History entry records the skip with `seconds_remaining`.

3. **Cooldown elapses → third tick fires.**
   - Wait 30 min total since the first fire (need 5 more scheduled ticks at `*/5`).
   - **Expect:** gate passes again. Architect runs. Second `loom:architect` proposal
     appears.

4. **Backlog fills → ticks stop firing.**
   - Manually promote 4+ `loom:curated` issues to `loom:issue` (or file 4 new ones).
   - **Expect:** next workflow tick gate fails on `ready_count_too_high`
     (`ready_count=4 >= 3`). No new proposal.

5. **MAX_PROPOSALS reached → tick stops firing even with empty backlog.**
   - Re-empty the backlog. Wait until 5 `loom:architect` proposals are open (file or
     let them accumulate).
   - **Expect:** next tick gate fails on `max_proposals_reached`
     (`open_proposals=5 >= 5`). No new proposal.

6. **Repeat steps 1-5 for `loom-hermit.yml`.**
   - Same gating mechanics, separate `last_hermit_trigger` and `loom:hermit` count.
   - Verify the same state file is used (both roles update
     `.loom/sweep-history.json` on the `loom-state` branch).

7. **Smoke: gate-only run without firing.**
   - Set `ISSUE_THRESHOLD=0` in the workflow env block (forces the gate to always
     fail).
   - Trigger via `workflow_dispatch`.
   - **Expect:** workflow completes successfully; gate logs `skip_reason=ready_count_too_high`;
     Architect step skipped; no state file write; total runtime < 30 seconds.

**Failure observability:**
- Every gate decision is logged to the workflow's job log AND appended to
  `.loom/sweep-history.json` `history[]` array (last 50 entries).
- The skip reason is visible via `gh run view <run-id> --log` and via reading the
  state file directly: `git show loom-state:.loom/sweep-history.json | jq .history`.

---

## 9. Why Not Implement Now?

This issue allows "design memo OR implementation." Memo is preferred for these reasons:

1. **Cross-cutting state file location.** Option 5a (orphan branch) requires a
   `contents: write` permission and orphan-branch lifecycle that deserves explicit
   review before shipping. Worth a separate PR.

2. **Schedule defaults are non-obvious.** The cron interval (`*/15 * * * *`?
   `*/10`? `*/5`?) sets the effective floor on proposal latency vs Actions-minutes
   cost. This is a tunable that benefits from a few weeks of operator feedback
   before locking in.

3. **MAX_PROPOSALS configurability surface.** Per §6, we ship workflow-local env
   defaults now and re-evaluate after observing real usage. Coupling that decision
   with the workflow scaffolding in one PR creates pressure to ship both quickly.

4. **Phase 3 blocker question.** Issue #3378 (Phase 3 hard deletion) needs to know
   whether the deletion can proceed with Option B in place. The memo gives Phase 3 a
   target to plan against; the implementation only needs to land before Phase 3's
   merge commit.

5. **Concurrent work.** Another builder is implementing #3377 (Phase 2c inventory).
   The memo establishes shared vocabulary for the inventory work; the implementation
   PR can land after #3377 closes without rebase churn.

A follow-up issue will track the implementation work. The follow-up's checklist:

- [ ] Ship `.github/workflows/loom-architect.yml` and `loom-hermit.yml` (schedule
      commented out by default, `workflow_dispatch` enabled).
- [ ] Ship `.loom/scripts/check-work-gen-gate.sh` + `defaults/scripts/` mirror.
- [ ] Ship `defaults/scripts/tests/test-work-gen-gate.sh`.
- [ ] Decide between §5a (orphan branch) and §5b (cache) — prototype both, pick
      whichever the smoke test handles more cleanly.
- [ ] Update `.gitignore`, `defaults/.gitignore`.
- [ ] Document in `CLAUDE.md` and `defaults/CLAUDE.md` (extend the Phase 2a
      "Scheduled Support Roles" section with an Architect/Hermit subsection).
- [ ] Verify Phase 3 (#3378) deletion of `daemon_v2/` doesn't leave dangling references
      to `last_architect_trigger` / `last_hermit_trigger` keys in `daemon-state.json`
      consumers.

---

## 10. Open Questions Resolved

| # | Question | Resolution |
|---|----------|------------|
| 1 | Option A vs Option B | **Option B** (this memo). |
| 2 | Same state file or separate per generator | **Same file, separate keys** (`last_architect_trigger`, `last_hermit_trigger`). DRY: one read/write helper handles both. |
| 3 | MAX_PROPOSALS configurability | **Workflow-local env vars** matching the daemon's `LOOM_*` convention. Repo-variable shared-config support deferred to Phase 4. |

---

## 11. Non-Goals (Confirmed Out of Scope)

- Replacing Architect or Hermit with a different proposal-generation mechanism. They
  keep their existing role definitions (`.loom/roles/architect.md`,
  `.loom/roles/hermit.md`) and prompts.
- Quality-of-backlog gating (e.g., "fire Hermit if too many proposals are technical
  debt"). The threshold-and-cooldown semantics are preserved as-is.
- Champion / Auditor / Curator / Judge / Guide cadence — handled by Phase 2a.
- Token rotation for Architect/Hermit (single `CLAUDE_API_KEY` is sufficient; multi-
  account is a sweep-only concern).
