# Autonomous mode — end-to-end acceptance playbook (#3813)

This playbook demonstrates the headline goal of the autonomous daemon (epic
#3809): **"focus on creating issues, watch it build."** It walks a single
throwaway issue from a fresh `loom:triage` filing all the way to a merged PR with
the operator only ever creating the issue — every intermediate transition
(`loom:triage` → `loom:curated` → `loom:issue` → `loom:building` → PR →
merged) is driven by Loom, not by hand.

It is a **repeatable, mostly-scripted** procedure. It cannot be a hermetic CI
test because it depends on live forge state, real dispatch, and the support-role
crons — but the label-transition wait in Step 5 is a scripted assertion so the
"did it actually work" check is not a manual eyeball.

## Prerequisites

- A built `loom-daemon` binary (`cargo build --release -p loom-daemon`), or set
  `LOOM_DAEMON_BIN`.
- `gh` authenticated against the target repo (`gh auth status`).
- A multi-account token pool bootstrapped (`loom-tokens bootstrap`) if you want
  the daemon dispatch path to rotate accounts; a single token also works.
- **A fresh token ranking, refreshed on a `<10`-min cadence (#3894).** With a
  multi-account pool, wire `./.loom/scripts/probe-tokens.sh --ranking` on a cron
  (e.g. `*/5 * * * *`) — or run `loom-tokens check --ranking` before starting —
  so `.loom/tokens/.ranking` stays inside its 10-minute freshness window. A
  stale/absent ranking makes the burst-dispatching work finder repeatedly select
  exhausted accounts, wedging sweeps at startup. The selector has a stale-ranking
  fail-safe (it excludes known-exhausted accounts from an aged ranking) but that
  is a safety net, not a substitute for the probe — see
  [`.loom/docs/daemon-reference.md`](../.loom/docs/daemon-reference.md)
  §Operability → "Prerequisite: a fresh token ranking".
- The `buildGate` block configured in `.loom/config.json` if you want the
  main-health gate active (optional for the loop itself).
- The support roles reachable: either the GitHub Actions cron workflows enabled
  (`.github/workflows/loom-*.yml`), or you run Curator / Judge / Champion
  manually per step (the playbook shows the manual triggers).

## Step 1 — Enable autonomous mode in config

Add the `autonomous` block to `.loom/config.json` (see
[`.loom/docs/daemon-reference.md`](../.loom/docs/daemon-reference.md) §Operability):

```json
{
  "autonomous": {
    "workFinder": { "enabled": true, "intervalSecs": 60, "maxConcurrent": 3 },
    "mainHealthGate": { "enabled": true }
  }
}
```

## Step 2 — Start the daemon in autonomous mode

```bash
./.loom/scripts/cli/loom-daemon-start.sh --from-config
# or force the loops on regardless of config (a bare start is FLAGS-OFF, #3911,
# so opt in explicitly — it does NOT enable autonomy on its own):
./.loom/scripts/cli/loom-daemon-start.sh --work-finder --health-gate
```

Confirm the work finder is ticking:

```bash
grep 'work_finder: enabled' ~/.loom/daemon.log | tail -1
```

## Step 3 — File a throwaway triage issue (the ONLY operator action)

```bash
ISSUE=$(gh issue create \
  --title "E2E canary $(date +%s): no-op doc touch" \
  --body "Autonomous-mode E2E canary. Curator should enrich; work finder should build. Safe to close." \
  --label loom:triage \
  | grep -oE '[0-9]+$')
echo "Filed canary issue #$ISSUE"
```

From here on, **do not** run any `gh issue edit` / dispatch commands by hand —
the whole point is that Loom carries it.

## Step 4 — Let the support roles + work finder run

- **Curator** enriches the issue and marks it `loom:curated`.
- A human (or Champion in full-autonomy mode) promotes `loom:curated` →
  `loom:issue`.
- The **work finder** (daemon) sees the open `loom:issue`, flips it to
  `loom:building`, and dispatches a `/loom:sweep` child.
- The sweep runs Builder → Judge → (Doctor) → opens a PR (`loom:review-requested`
  → `loom:pr`).
- **Champion** auto-merges the approved PR.

If the crons are not enabled, trigger the roles manually (still zero
issue-editing by you):

```bash
claude -p "/curator"  --dangerously-skip-permissions
claude -p "/champion" --dangerously-skip-permissions   # promotes loom:curated → loom:issue, later auto-merges
claude -p "/judge"    --dangerously-skip-permissions
```

## Step 5 — Scripted assertion: wait for the label sequence

Poll the issue until it closes (its PR merged) or a timeout fires. This is the
load-bearing pass/fail check — it asserts the transitions happened without
operator dispatch:

```bash
#!/usr/bin/env bash
# assert-e2e.sh <issue-number> [timeout-secs]
set -uo pipefail
ISSUE="${1:?usage: assert-e2e.sh <issue> [timeout]}"
TIMEOUT="${2:-3600}"      # default 1h
INTERVAL=30
elapsed=0
declare -A seen
while (( elapsed < TIMEOUT )); do
    state=$(gh issue view "$ISSUE" --json state,labels \
        --jq '{state: .state, labels: [.labels[].name]}')
    issue_state=$(echo "$state" | jq -r '.state')
    labels=$(echo "$state" | jq -r '.labels | join(",")')
    for l in loom:curated loom:issue loom:building; do
        if [[ ",$labels," == *",$l,"* && -z "${seen[$l]:-}" ]]; then
            seen[$l]=1
            echo "[$(date +%T)] reached: $l"
        fi
    done
    if [[ "$issue_state" == "CLOSED" ]]; then
        echo "PASS: issue #$ISSUE closed (PR merged) after ${elapsed}s"
        echo "observed transitions: ${!seen[*]}"
        exit 0
    fi
    sleep "$INTERVAL"; elapsed=$((elapsed + INTERVAL))
done
echo "FAIL: issue #$ISSUE did not close within ${TIMEOUT}s (labels: $labels)" >&2
exit 1
```

```bash
bash assert-e2e.sh "$ISSUE" 3600
```

A `PASS` line means the full `loom:triage → merged` chain completed autonomously.
Record the observed transitions and elapsed time in the PR/run notes.

## Step 6 — Tear down

```bash
./.loom/scripts/cli/loom-daemon-stop.sh
# If the canary PR did not auto-merge, close it and the issue:
gh issue close "$ISSUE" 2>/dev/null || true
```

Stopping the daemon leaves any still-running sweep child alive by design (see
daemon-reference §"Shutdown decision"); the next start reconciles it, or cancel
it explicitly with `mcp__loom__cancel_sweep` before stopping.

## What "green" looks like

| Signal | Where |
|--------|-------|
| `work_finder: enabled (...)` | `~/.loom/daemon.log` |
| `work_finder: tick — ... N dispatched` | `~/.loom/daemon.log` |
| Issue reaches `loom:building` then closes | `assert-e2e.sh` PASS |
| Zero operator `gh issue edit` between Step 3 and merge | your shell history |
