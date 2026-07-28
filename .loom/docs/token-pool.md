# Multi-Account Token Pool & Rotation

Loom can rotate among multiple Claude OAuth accounts so load spreads across
accounts and a single weekly limit does not stall the pipeline. This document is
the full reference for provisioning, importing, health-probing, selecting, and
operating the token pool. `CLAUDE.md` carries only the operating summary and
points here.

> **Secrets**: `~/.claude-monitor/accounts.env`, the opt-in `~/.loom/accounts.env`,
> and the repo-local `.loom/accounts.env` all hold raw OAuth keys. The repo-local
> file and `.loom/tokens/` are gitignored (installer- and `loom-daemon init`–managed);
> keep any home-level master `0600` and outside any repo.

## Bootstrapping the pool

For environments that rotate among multiple Claude OAuth accounts, Loom can
bootstrap a per-account token pool at `.loom/tokens/` from numbered
`ACCOUNT_EMAIL_N` / `ACCOUNT_KEY_N` / `ACCOUNT_TOKEN_FILE_N` triples:

```env
ACCOUNT_EMAIL_1=user1@example.com
ACCOUNT_KEY_1=sk-ant-oat01-...
ACCOUNT_TOKEN_FILE_1=user1.token
```

Run `loom-tokens bootstrap` to materialize the pool:

```bash
loom-tokens bootstrap            # Idempotent — only writes new/missing tokens.
loom-tokens bootstrap --dry-run  # Preview + print the effective merged account set.
loom-tokens bootstrap --force    # Overwrite on-disk tokens that have drifted from source.
loom-tokens bootstrap --shared   # Provision the shared machine-level pool at ~/.loom/tokens
```

Each account becomes `.loom/tokens/<file>.token` (mode `0600`). An `index.json`
manifest is written alongside with sha256 fingerprints (8 chars) for drift
detection plus each account's `source` (home/repo) — **no secret material is
stored in the manifest**. Numbering gaps are allowed; partial triples are skipped
with a warning.

`.loom/tokens/` is gitignored. The pool is consumed by external rotation logic
(e.g. a `claude-wrapper.sh` that picks the least-used token); only the bootstrap
step is provided here.

## Account sources: claude-monitor-first + per-repo (#3695, #3698, #3704)

Rather than re-declaring the same account triples in every repo's `.env`, declare
them **once** in the shared claude-monitor master and let each workspace add or
override on top of it. Sources are merged by account email in precedence order:

| Source | Default location | Override |
|--------|------------------|----------|
| **claude-monitor master** (primary) | `~/.claude-monitor/accounts.env` | `LOOM_CLAUDE_MONITOR_DIR` env var (directory) |
| **Repo-local** | `<repo>/.loom/accounts.env` if present, else legacy `<repo>/.env` | `--env <path>` on `bootstrap` |
| **Home master** (opt-in only, #3704) | *no default location* — read **only** when explicitly pointed at | `LOOM_ACCOUNTS_ENV` env var (a path enables it, `""` disables); `--home-env <path>` / `--no-home` on `bootstrap` |

**Default resolution is claude-monitor → repo `.env`.** The `~/.loom/accounts.env`
home master is **no longer auto-read** (#3704 retired the default location): it is
consulted only when an operator opts in via `LOOM_ACCOUNTS_ENV=<path>`
(conventionally `~/.loom/accounts.env`) or `--home-env <path>`. This retires the
default *location*, not the *capability*.

`loom-tokens bootstrap` reads the available sources and **merges them by account
email** (`ACCOUNT_EMAIL`), with the higher-precedence source winning:

- An email present **only in a lower-precedence source** is inherited into the pool.
- An email present **only in a higher-precedence source** is added.
- An email present in **both** → the higher-precedence entry overrides (e.g. to
  rotate a key or repoint the token file).

To *exclude* an inherited account from one repo, pin the subset you want with
`loom-tokens pin` — the merge only ever adds/overrides, never subtracts. The
effective merged set (and where each account came from) is printed by `bootstrap`
and `bootstrap --dry-run`. A repo with only a legacy `.env` and no other source
behaves exactly as before.

## Importing live tokens from claude-monitor (#4006)

`accounts.env` is a **snapshot** — a file someone wrote by hand at some point.
claude-monitor keeps the **live** credentials in its SQLite store
(`~/.claude-monitor/usage.db` → `oauth_credentials`) and refreshes them as
accounts are re-authenticated. The two drift, and the drift is silent and total:

```text
401 {"type":"authentication_error","message":"OAuth access token has been revoked."}
```

When that happens to every account at once, `loom-tokens check` reports all
accounts `blocked`, the daemon's dynamic concurrency cap collapses to
`min(healthy 0 × per-token N, …) = 0`, and dispatch stops entirely. Crucially
**`bootstrap --force` does not fix it** — it faithfully rewrites the same revoked
tokens, because the snapshot itself is what went stale.

**`bootstrap` now detects this condition (#4030).** When `usage.db` is present and
the tokens `bootstrap` is about to write disagree with the live store (same email,
different fingerprint), it prints a warning naming the diverging accounts and
pointing at `import-from-monitor` — so the stale snapshot is caught automatically
instead of by hand-comparing fingerprints. The check is read-only, warns but never
auto-switches sources, and is silent when no `usage.db` is present or it is
unreadable; it prints emails and 8-char fingerprints only, never secret material.

`loom-tokens import-from-monitor` reads the live store directly and is **the
standard way to populate a new host's pool** (it replaces hand-copying a pool
between machines):

```bash
loom-tokens import-from-monitor                  # into <repo>/.loom/tokens
loom-tokens import-from-monitor --shared         # into the machine-level pool (#3938)
loom-tokens import-from-monitor --force          # apply ROLLED tokens (see below)
loom-tokens import-from-monitor --dry-run        # preview
loom-tokens import-from-monitor --prune          # drop accounts the monitor no longer reports
```

**`--force` is what applies a token roll.** Every rolled token legitimately
differs from what is on disk, so without `--force` each one is reported as drift
and left alone — deliberately, so a hand-pinned token is never silently clobbered.
The command exits `2` when drift was found and not applied, so a script can detect
"pool is still stale". After importing, refresh the ranking so the daemon sees the
recovered capacity:

```bash
loom-tokens import-from-monitor --force && loom-tokens check --ranking
```

Behavior notes:

- **Read-only** on `usage.db` (opened `mode=ro`) — the store belongs to
  claude-monitor; Loom never writes or migrates it.
- Only `is_active = 1` rows are imported; `expires_at` is **not** used as a filter
  (observed rows carry stale timestamps while still authenticating — health comes
  from `loom-tokens check`).
- Token filenames use the same derivation as `bootstrap` (`robb@2amlogic.com` →
  `robb-2amlogic.token`), so an account keeps one identity across both paths and
  re-importing overwrites in place.
- Idempotent: unchanged tokens are left untouched. `index.json` records
  `source: monitor-db` (distinct from the `monitor` snapshot) and, as always,
  fingerprints only — never secret material.
- `--prune` removes only `*.token` files; pool state (`.ranking`, `.bad_tokens`,
  `.failure_counts`, `.allowlist`) is never touched.
- The importer takes **claude-monitor as authoritative for pool membership**, so
  it imports every active account — including any that `accounts.env` omitted. Use
  `loom-tokens pin` to restrict which accounts the selector may actually pick.
- Absent claude-monitor, an absent `usage.db`, or an older schema without
  `oauth_credentials` all exit `1` with a message naming the path tried.

## Account health probe + ranking

Once bootstrapped, `loom-tokens check` probes each account for current rate-limit
headers and (optionally) writes a JSON ranking that the spawn-time selector can
consume:

```bash
loom-tokens check                  # Probe + print human table
loom-tokens check --ranking        # Probe + write .loom/tokens/.ranking atomically
loom-tokens check --json           # Emit full JSON report to stdout
loom-daemon tokens check --json    # Native Rust equivalent (issue #4108)
./.loom/scripts/probe-tokens.sh    # Cron-friendly wrapper for periodic invocation
```

**`probe-tokens.sh` delegates to `loom-daemon tokens check`, not Python (#4080).**
It resolves a `loom-daemon` binary (`$LOOM_DAEMON_BIN` → `loom-daemon` on PATH →
build-output-relative candidates under the repo), capability-probes it with
`tokens check --help` to detect a stale pre-#4108 binary, and `exec`s `tokens
check "$@"` on success — the flags and exit codes above are unchanged either
way. It falls back to `loom-tokens` on PATH (with a stderr warning) only when
the resolved daemon binary predates the `tokens` subcommand, and exits `1` with
an actionable message (naming `loom-daemon-start.sh` / `cargo build`) when
neither is available. The historical `python3 -m loom_tools.tokens.cli`
fallback tier has been removed entirely.

The probe sends a minimal `POST /v1/messages` request (1 input, 1 output token)
and parses rate-limit response headers. The header parser matches by **suffix**
(`-5h-utilization`, `-7d-utilization`, `-7d-reset`) so future renames of the
`anthropic-ratelimit-tokens-*` prefix still work; the full header set is logged on
the first probe of each run.

Status assignment: `available` (utilizations < 95%), `exhausted`
(`7d_utilization >= 0.95`), `rate_limited` (current 429), `blocked` (401 auth
failure or token listed in `.bad_tokens`). Probe failures (network, timeout, 5xx)
are logged and skipped — one bad account does not abort the run.

OAuth tokens shaped `sk-ant-oat01-*` are sent with `Authorization: Bearer` +
`anthropic-beta: oauth-2025-04-20`; plain API keys use `x-api-key`.

**The running `loom-daemon` self-refreshes `.ranking` (#3969)** — it invokes
**its own binary** (`std::env::current_exe()`) with `tokens check --ranking
--workspace <repo_root>` on its own periodic loop (default every 10 minutes,
`autonomous.tokenRankingRefresh` / `LOOM_TOKEN_RANKING_REFRESH*`, on by default
since it is read-only probing with no dispatch side effect) — as of #4080 this
is a direct daemon-to-daemon subcommand invocation, not a shell out to
`probe-tokens.sh`, so a standing cron for this is no longer required when the
daemon is running. See
[Token-ranking self-refresh](daemon-reference.md#token-ranking-self-refresh-3969)
for the config knobs.

A cron entry is now only needed as a **fallback for setups that don't run
`loom-daemon`** (e.g. pure `/loom:sweep` subagent dispatch with no daemon
process). Cron example (probe every 10 minutes):

```cron
*/10 * * * * cd /path/to/repo && ./.loom/scripts/probe-tokens.sh --ranking >> .loom/logs/probe-tokens.log 2>&1
```

## Token rotation setup (per-task spawn)

For Pro/Max plans, Loom supports rotating between multiple Claude Code OAuth
tokens. This spreads load across accounts and recovers automatically when a single
token hits its weekly limit.

1. Declare account credentials in a default source — the shared claude-monitor
   master `~/.claude-monitor/accounts.env` (primary) or per-repo in
   `<repo>/.loom/accounts.env` (falls back to legacy `<repo>/.env`). The
   `~/.loom/accounts.env` home master is **opt-in only** since #3704 (no longer
   auto-read); point `LOOM_ACCOUNTS_ENV=~/.loom/accounts.env` (or `--home-env
   <path>`) at it to enable:
   ```env
   ACCOUNT_EMAIL_1=account-one@example.com
   ACCOUNT_KEY_1=sk-ant-oat01-...
   ACCOUNT_TOKEN_FILE_1=account-one.token
   ACCOUNT_EMAIL_2=account-two@example.com
   ACCOUNT_KEY_2=sk-ant-oat01-...
   ACCOUNT_TOKEN_FILE_2=account-two.token
   ```
   The claude-monitor, repo-local, and (opt-in) home sources are **merged by
   email**, with the higher-precedence source overriding/adding. Keep any
   home-level master `0600` and outside any repo.
2. Run `loom-tokens bootstrap` to materialize the merged set into per-account
   `.token` files in `.loom/tokens/` (mode 0600, parent dir 0700). See issues
   #3234, #3695. **If claude-monitor runs on this host, prefer `loom-tokens
   import-from-monitor`** — it reads claude-monitor's live credential store instead
   of the `accounts.env` snapshot, so a new host needs no account file of its own
   and a token roll is picked up automatically (add `--force` to apply rolled
   tokens).
3. Spawn agents through `.loom/scripts/spawn-claude.sh` instead of invoking
   `claude` directly. The wrapper selects a token using a 3-tier algorithm
   (ranking → allowlist → random), exports `CLAUDE_CODE_OAUTH_TOKEN`, then `exec`s
   `claude` (or pass `--use-wrapper` to layer on top of `claude-wrapper.sh` for
   retry behavior).

## Selection algorithm (`loom_tools.tokens.select`)

Three tiers, falling through to the next when the current tier yields nothing:

1. **Ranking** — `.loom/tokens/.ranking` (pipe-delimited `name|status`, refreshed
   every <10 min). Picks the first non-`exhausted`/non-`blocked` token.
2. **Allowlist** — `.loom/tokens/.allowlist` (one name per line). Random pick from
   allowed accounts.
3. **Random** — uniform pick from all `*.token` files.

Tokens marked bad in `.loom/tokens/.bad_tokens` are skipped at every tier.

## Bad-token tracking (`loom_tools.tokens.bad_tokens`)

When a token returns `TOKEN_EXPIRED` or `TOKEN_EXHAUSTED`, callers append an entry
to `.loom/tokens/.bad_tokens`. Writes are guarded with a `mkdir`-based lock
(POSIX-atomic, macOS-compatible — `flock` is **not** used because it isn't
available on stock macOS). Reads use word-boundary regex so `agent-1` and
`agent-10` don't collide.

## Error classification (`.loom/scripts/lib/classify-error.sh`)

The `classify_error <output> <exit_code>` function returns one of `SUCCESS`,
`TIMEOUT`, `CWD_DELETED`, `TOKEN_EXPIRED`, `TOKEN_EXHAUSTED`, `RECOVERABLE`.
Critical fix from #3233: exit code is checked **before** output substring
matching — clean exits (`exit_code == 0`) always return `SUCCESS` regardless of
stdout content.

## Worktree handling

When invoked from a worktree, `spawn-claude.sh` resolves the canonical repo root
via `git rev-parse --git-common-dir` and locates `.loom/tokens/` there — never in
the worktree's path. This avoids each worktree maintaining its own bad-tokens
list.

## Shared machine-level pool fallback (#3938)

Token selection resolves the effective pool as: the **per-repo** pool
`<repo>/.loom/tokens/` when it holds `*.token` files, else the **shared
machine-level pool** `~/.loom/tokens/` (override `LOOM_SHARED_TOKENS_DIR`; set it
empty to disable the fallback). This lets a consumer repo the daemon dispatches
into — which has no pool of its own — spawn against the shared pool instead of
hard-failing with `EX_CONFIG`. Crucially, the pool **state** files (`.bad_tokens`,
`.failure_counts`, `.ranking`, `.allowlist`) are read/written in whichever pool was
selected, so state is **never forked per repo** (token-capacity backpressure sees
one truth). Provision the shared pool once per machine with `loom-tokens bootstrap
--shared`. See [daemon-reference.md → Token pool provisioning for managed
repos](daemon-reference.md#token-pool-provisioning-for-managed-repos-3938).

**Package-path fallback for consumer-repo dispatches (#3949)**: `#3938` fixed the
pool *location*, but token *selection* still shells into `python3 -m
loom_tools.tokens.select`, and `spawn-claude.sh` locates that Python package via
(1) `LOOM_PACKAGE_PATH` env, (2) script-relative `../../loom-tools/src`, (3)
`$WORKSPACE/loom-tools/src`. `loom-daemon`'s `spawn_child` now resolves and
forwards `LOOM_PACKAGE_PATH` automatically on every dispatch: an ambient override
on the daemon's own environment always wins, otherwise it derives
`<loom-checkout>/loom-tools/src` from the source tree the running `loom-daemon`
binary was compiled from (`CARGO_MANIFEST_DIR`, baked in at build time) when that
directory still exists and contains `loom_tools/tokens`. A consumer repo with no
loom checkout and no `LOOM_PACKAGE_PATH` env now selects a token successfully with
zero manual configuration.

## Hard-fail on missing pool

`spawn-claude.sh` exits `78` (`EX_CONFIG`) with a message instructing the user to
run `loom-tokens bootstrap` (or `loom-tokens bootstrap --shared` for the
machine-level pool) when **neither** the per-repo nor the shared pool has usable
tokens (absent, empty, or all bad). It does **not** silently fall back to
keychain — that path belongs in `loom-daemon` (#3236), and only when token
rotation has not been configured at all.

## Operator CLI (`loom-tokens pin/unpin/unblock`)

Operators can restrict the rotation pool to a subset of accounts (an "allowlist")
and manually un-blacklist accounts marked bad. Auto-recovery prevents pin-induced
lockouts.

```bash
loom-tokens pin agent-3 agent-7   # Set allowlist to exactly these
loom-tokens pin add agent-2       # Append (idempotent)
loom-tokens pin remove agent-3    # Remove
loom-tokens pin status            # Show current allowlist
loom-tokens unpin                 # Delete allowlist (back to full pool)

loom-tokens unblock agent-1       # Remove one entry from .bad_tokens
loom-tokens unblock --all         # Clear .bad_tokens entirely
```

**Validation**: `pin` accepts only exact bootstrapped account names —
substring/fuzzy matches are rejected. The allowlist is sorted, deduplicated, and
`mkdir`-lock guarded so concurrent operator commands don't drop entries.

**Reason-aware bad-token TTL**: bad-tokens entries with reason `auth` (401) ignore
`LOOM_TOKENS_BAD_TTL` (default 21600s = 6h) and persist until `loom-tokens
unblock`. Other reasons expire automatically.

**Auto-unpin** (`failure_counts`): the wrapper tracks consecutive
`TOKEN_EXHAUSTED` failures per account in `.loom/tokens/.failure_counts` (JSON).
When **every** account in the allowlist hits the threshold (default 5), the
wrapper auto-clears `.allowlist` and `.failure_counts` with a loud stderr log
line. Operators can re-pin afterwards. The threshold is `>= 5`, so a 6th failure
does not silently exceed; it still triggers (idempotent at-or-above).

Counters are reset on:
- a successful spawn for that account, or
- any operator allowlist mutation (`pin`, `unpin`, `add`, `remove`).

**Empty-pool guard**: if the selector finds the allowlist minus `.bad_tokens` is
empty, `spawn-claude.sh` exits `78` (`EX_CONFIG`) with operator instructions. It
refuses to silently auto-clear `.bad_tokens` — that masks real auth problems.

## Tests

```bash
PYTHONPATH=loom-tools/src python3 -m pytest loom-tools/tests/tokens/ -v
bash .loom/scripts/tests/test-spawn-claude.sh
```
