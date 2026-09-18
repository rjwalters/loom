//! Token selection algorithm — 3-tier priority, ported from
//! `loom_tools.tokens.select`.
//!
//! Selection order:
//!   1. Ranking file (`.ranking`, fresh < 10 min): rotate one-per-account
//!      across accounts whose probe status is a known-good status (#3991),
//!      using the persistent rotation cursor ([`super::rotation`]) so a
//!      burst of N concurrent dispatches spreads across `min(N, available)`
//!      distinct accounts (#3909). `LOOM_TOKEN_SPREAD_TOP_N` /
//!      `tokens.spreadTopN` optionally caps the rotation window. The preferred
//!      pass additionally excludes accounts at/above the 5h-window load
//!      threshold (`name|status|5h_util` third field, `LOOM_TOKEN_5H_LOAD_GATE`,
//!      default 0.70) — a soft eligibility gate layered on the rotation cursor
//!      (#4195); the fallback pass readmits them so the pool never hard-fails on
//!      load alone.
//!   2. Allowlist file (`.allowlist`): random pick from allowed accounts.
//!   3. Random pick from all `.token` files.
//!
//! In all tiers, bad-marked tokens ([`super::bad_tokens::is_bad`]) are
//! skipped. When the caller names a model class ([`select_token_for_class`],
//! `tokens select --model`), that skip narrows to
//! [`super::bad_tokens::is_bad_for_class`]: an account marked bad only for a
//! *different* class stays selectable, while every class-less mark keeps
//! blocking exactly as before (#8058). The `.ranking` file contributes two distinct exclusion sets to
//! tiers 2/3:
//!
//! - **Hard** ([`is_hard_excluded`]: `exhausted` unconditionally; `blocked`
//!   unless the account's `.bad_tokens` history positively shows an
//!   already-expired *session-limit* mark, issue #7522)
//!   — applied at *any* ranking age, and never dropped by the fail-safe
//!   retry's advisory readmission (issue #5629). Tier 1 has always
//!   hard-excluded these in every pass; before #5629 the knowledge stopped
//!   there unless the ranking happened to be stale, so a fresh ranking whose
//!   only rows were `exhausted` fell through to a tier-3 `mode=random` pick of
//!   exactly the account it had just ruled out.
//! - **Advisory** (any other non-healthy status, e.g. `rate_limited`) — only
//!   sourced from a *stale* `.ranking` (issue #3894), and dropped by a
//!   fail-safe retry if the exclusions would otherwise empty the pool.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::account_registry::AccountProvider;
use super::bad_tokens::{
    blocking_entry, blocking_entry_in_dir, exhaustion_cooldown_secs, is_bad_for_class,
    latest_block_was_session_limit, EXHAUSTION_COOLDOWN_ENV,
};
use super::bootstrap::{read_manifest_rows, ManifestRow};
use super::paths::{has_token_files, resolve_tokens_dir, shared_tokens_dir};
use super::rng::Rng;
use super::rotation::next_rotation_index;

/// Ranking file is considered fresh for this many seconds.
const RANKING_FRESH_SECONDS: u64 = 600; // 10 min

/// Exit code when no token is available (matches sysexits.h EX_CONFIG).
pub const EX_CONFIG: i32 = 78;

/// Default 5h-window load threshold for the tier-1 *preferred* pass (issue
/// #4195). An account at/above this fraction of its 5h rate-limit window is
/// excluded from the preferred pass (a soft eligibility gate layered on top of
/// the #3909 rotation-cursor spread) and readmitted in the fallback pass, so a
/// fully-loaded pool still dispatches. Overridable via `LOOM_TOKEN_5H_LOAD_GATE`
/// (a value > 1.0 disables the gate). A missing/unparseable utilization is
/// treated as unknown → never gated. Mirrors `select.py:_DEFAULT_5H_LOAD_GATE`.
const DEFAULT_5H_LOAD_GATE: f64 = 0.70;

/// Statuses considered positively healthy (issue #3991 — allowlist of
/// known-good statuses, not a denylist of known-bad ones). The empty string
/// is included: a ranking line with no status field means "probe recorded no
/// adverse signal".
fn is_healthy_status(status: &str) -> bool {
    status == "available" || status.is_empty()
}

/// Whether `status` durably excludes `name` from every tier/pass (#5629),
/// including the tier-1 empty-pool fallback pass and the tier-2/3 fail-safe
/// retry.
///
/// `exhausted` (a hit weekly/monthly ceiling) is unconditional — it comes
/// straight from a probe's own 7d-utilization reading, not from
/// `.bad_tokens`, so there is nothing further to check: handing it out
/// "because the pool would otherwise be empty" does not produce a working
/// spawn, it produces a spawn that burns its retry budget and dies.
///
/// `blocked` has **three** producers, and only one of them is a `.bad_tokens`
/// snapshot:
///
/// | Cause | `.bad_tokens` entry | Site |
/// |---|---|---|
/// | already bad-marked (no network call) | yes | `check::probe_account_with_blocking` |
/// | probe returned **401** (`error: auth_401`) | **no** | `check::dispatch_probe` |
/// | credential **shape mismatch** (#5608) | **no** | `check::dispatch_probe` |
///
/// So the row is re-verified against live `.bad_tokens` state, but readmission
/// requires **positive evidence** that the block came from a session-limit mark
/// (issue #7522):
///
/// - a live blocking entry → still hard-excluded (unchanged pre-#7522
///   behavior);
/// - no live entry, but the account's latest historical entry names the 5h
///   session window ([`latest_block_was_session_limit`]) → readmitted. This is
///   the #7522 case: a session-limit `.bad_tokens` entry kept hard-excluding
///   its account long after the 5h window reset and
///   [`super::bad_tokens::is_bad`]/[`blocking_entry_in_dir`] already agreed it was selectable
///   again, until the next `tokens check --ranking` happened to run. It also
///   covers a `tokens unblock`'d session entry.
/// - no `.bad_tokens` history at all → hard-excluded, exactly as before #7522.
///   This is the 401 / shape-mismatch shape, and it must stay excluded: a
///   revoked or mis-bound credential never self-heals, so readmitting it lets
///   the fail-safe retry ([`select_token`]'s second pass, which filters only on
///   [`super::bad_tokens::is_bad`] plus this set) hand out a permanently dead account, loses
///   #4643's empty-pool diagnostic on an all-dead pool, and makes
///   [`has_usable_account`] report that pool as worth failing over to. That is
///   the regression #5629's hard set exists to prevent.
fn is_hard_excluded(tokens_dir: &Path, name: &str, status: &str) -> bool {
    match status {
        "exhausted" => true,
        "blocked" => {
            blocking_entry_in_dir(tokens_dir, name).is_some()
                || !latest_block_was_session_limit(tokens_dir, name)
        }
        _ => false,
    }
}

/// A token chosen by [`select_token`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedToken {
    /// Basename without the `.token` extension.
    pub name: String,
    /// Absolute path to the `.token` file.
    pub file: PathBuf,
    /// Token contents (whitespace-stripped).
    pub key: String,
    /// `"ranked"` | `"allowlist"` | `"random"`.
    pub mode: &'static str,
    /// This account's `index.json` `upstream_id`, when the pool has a
    /// manifest row for it (design D1/D2/D9 of
    /// `docs/design/token-pool-provider-identity.md`). `None` for a name
    /// with no manifest row at all — a hand-provisioned pool never had one
    /// to carry, so this is never fabricated (issue #5609).
    pub upstream_id: Option<String>,
}

/// No tokens available — bootstrap has not been run, or all are bad.
#[derive(Debug, Clone)]
pub struct EmptyTokenPoolError(pub String);

impl std::fmt::Display for EmptyTokenPoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for EmptyTokenPoolError {}

/// Identity of the binary that evaluated this selection — version, build
/// commit, and build timestamp (#4643).
///
/// The 2026-07-30 incident (13h-old exhaustion entries appearing to block
/// every spawn) could not be diagnosed from the sweep log because the failure
/// text never said *which* binary decided. `spawn-claude.sh` resolves the
/// daemon binary independently of any running daemon (`$LOOM_DAEMON_BIN` →
/// PATH → build-output candidates), so a stale binary at the selection site is
/// a real, recurring hypothesis — and now a directly checkable one: this string
/// is stamped into the empty-pool error itself.
#[must_use]
pub fn deciding_binary_identity() -> String {
    format!("loom-daemon {}", crate::self_update::BUILD_IDENTITY)
}

/// Render a whole-second duration compactly (`5h48m`, `48m12s`, `12s`) for
/// the cooldown-remaining field of the empty-pool detail.
fn format_secs(secs: i64) -> String {
    let secs = secs.max(0);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// One line of per-token exclusion detail for the empty-pool error (#4643):
/// account name, exclusion cause, reason class (auth = permanent vs
/// exhaustion = TTL), the entry's own timestamp, and the cooldown remaining.
///
/// Computed in the error path by re-walking the pool rather than threading
/// state through the three tier functions: reaching this point means every
/// tier — including the fail-safe retry that drops the stale-`.ranking`
/// advisory exclusions — came up empty, so the surviving causes are exactly
/// "bad-marked", "`.ranking`-hard-excluded" (#5629), "empty", and
/// "unreadable", all of which are re-derivable per token without disturbing
/// the selection hot path.
fn describe_exclusion(
    workspace: &Path,
    token_file: &Path,
    hard_excluded: &HashMap<String, String>,
    manifest: &HashMap<String, ManifestRow>,
) -> String {
    let name = stem(token_file);
    if let Some(status) = hard_excluded.get(&name) {
        // The re-probe advice is load-bearing, not decorative: since #7420,
        // `tokens check --ranking` genuinely re-probes a monitor-sourced row
        // whose own reset instant has already passed (before that it
        // short-circuited on `ranking.json` and copied the frozen row
        // forward, so the advice named a command that could not clear the
        // exclusion it was offered for). `--source probe` is the escalation
        // for a row whose reset is still in the future but is suspected
        // stale anyway.
        return format!(
            "{name}: hard-excluded by .ranking status ({status}) — never readmitted by the \
             fail-safe; re-probe with `loom-daemon tokens check --ranking` (this re-probes an \
             account whose reset time has already passed, #7420; use `--source probe` to force \
             a probe of every account)"
        );
    }
    if is_non_claude(manifest, &name) {
        let provider = manifest
            .get(&name)
            .map_or_else(|| "unknown".to_string(), |row| row.provider.to_string());
        return format!(
            "{name}: hard-excluded — index.json names provider {provider:?}, not claude (#5609)"
        );
    }
    if let Some(entry) = blocking_entry(workspace, &name) {
        let class = entry.class.label();
        let permanence = entry.class.permanence();
        let clears = match entry.cooldown_remaining_secs {
            Some(remaining) => format!("clears in {}", format_secs(remaining)),
            None => format!("needs `loom-daemon tokens unblock {name}`"),
        };
        return format!(
            "{name}: bad-marked [{class}, {permanence}] at {} — \"{}\"; {clears}",
            entry.timestamp, entry.reason
        );
    }
    match read_token_file(token_file) {
        Ok(key) if key.is_empty() => format!("{name}: empty .token file"),
        Ok(_) => format!("{name}: no usable tier admitted it (not bad-marked, key non-empty)"),
        Err(e) => format!("{name}: unreadable .token file ({e})"),
    }
}

fn shared_pool_hint() -> String {
    match shared_tokens_dir() {
        Some(dir) => format!(" (shared machine-level pool {} also checked)", dir.display()),
        None => String::new(),
    }
}

/// Count of `.token` files in `dir` that are **usable**: neither bad-marked
/// ([`super::bad_tokens::is_bad`]'s underlying check, via [`blocking_entry_in_dir`] so this
/// works against an arbitrary already-resolved directory rather than
/// re-deriving one from a workspace root) nor hard-excluded by that
/// directory's own `.ranking` file ([`is_hard_excluded`]: `exhausted`
/// unconditionally, `blocked` unless a session-limit mark provably expired, at
/// any ranking age, per #5629/#7522). A count of `0`
/// means "no usable account" — the boolean check `shadowed_shared_pool_hint`
/// used to make on its own.
///
/// This is deliberately stricter than [`has_token_files`] (presence-only) —
/// it is the check [`shadowed_shared_pool_hint`] needs so it never tells an
/// operator to retire a repo-local pool in favor of a shared pool that is
/// itself fully dead (issue #6758): presence alone cannot distinguish a
/// shared pool worth failing over to from one in the exact same exhausted
/// state as the repo-local pool that shadowed it. The exact count (rather
/// than just yes/no) is what [`shadowed_shared_pool_hint`]'s "spawnable
/// accounts" diagnostic reports (issue #7527).
pub(crate) fn usable_account_count(dir: &Path) -> usize {
    let hard = ranking_hard_exclusions(dir, &dir.join(".ranking"));
    list_token_files(dir)
        .into_iter()
        .filter(|f| {
            let name = stem(f);
            !hard.contains_key(&name) && blocking_entry_in_dir(dir, &name).is_none()
        })
        .count()
}

// ---------------------------------------------------------------------------
// Preflight spawnable-count snapshot (issue #7607)
// ---------------------------------------------------------------------------

/// Snapshot of the pool [`select_token`] would actually resolve to for
/// `workspace_root` (issue #7607) — the same repo-local/shared resolution
/// [`resolve_tokens_dir`] performs (#3938/#7527), with the total `*.token`
/// file count and the [`usable_account_count`] subset that would actually
/// survive a real selection attempt (bad-marked + `.ranking`-hard-excluded
/// accounts removed).
///
/// The role runner's pre-spawn preflight
/// (`role_runner::ScriptRoleInvocationRunner::invoke`) is the primary
/// consumer: `usable == 0` with `total > 0` is the "pool present but fully
/// exhausted" state a real spawn would otherwise discover ~10s into
/// `spawn-claude.sh`'s own token-selection preflight (`EX_CONFIG`, exit 78) —
/// this snapshot lets the caller detect that BEFORE spawning anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnablePoolState {
    /// The resolved pool directory (repo-local shadow pool when it holds
    /// `.token` files, else the shared machine-level pool — #3938/#7527).
    pub dir: PathBuf,
    /// Total `*.token` files in `dir`.
    pub total: usize,
    /// The subset of `total` that is neither bad-marked nor
    /// `.ranking`-hard-excluded — i.e. would actually be selectable.
    pub usable: usize,
}

/// Resolve `workspace_root`'s effective pool and snapshot its spawnable
/// account count (issue #7607). See [`SpawnablePoolState`] for field
/// semantics.
#[must_use]
pub fn spawnable_pool_state(workspace_root: &Path) -> SpawnablePoolState {
    let dir = resolve_tokens_dir(workspace_root);
    let total = list_token_files(&dir).len();
    let usable = usable_account_count(&dir);
    SpawnablePoolState { dir, total, usable }
}

/// Cap, in seconds, on how far into the future [`pool_clear_estimate`] will
/// ever report (issue #7607) — mirrors the `900 s` ceiling named in the
/// issue's backoff proposal. Purely a presentation bound: the role runner
/// re-checks [`spawnable_pool_state`] fresh on every tick regardless of this
/// estimate, so a too-early or too-late guess here never delays (or
/// artificially extends) a real readmission — it only shapes the
/// operator-facing log/detail text.
const POOL_CLEAR_ESTIMATE_CAP_SECS: i64 = 900;

/// Best-effort estimate of when the exhausted pool at `dir` might regain at
/// least one spawnable account (issue #7607): the earliest of every blocking
/// `.bad_tokens` exhaustion-cooldown clear instant
/// ([`super::bad_tokens::BlockingEntry::cooldown_remaining_secs`]) and every
/// `.ranking` hard-excluded row's `limit_reset` instant found in the pool,
/// capped at [`POOL_CLEAR_ESTIMATE_CAP_SECS`] from now (and never before
/// now). Diagnostic only — see [`POOL_CLEAR_ESTIMATE_CAP_SECS`]'s doc comment
/// for why this is never used as an actual gate.
#[must_use]
pub fn pool_clear_estimate(dir: &Path) -> chrono::DateTime<chrono::Utc> {
    let now = chrono::Utc::now();
    let cap = now + chrono::Duration::seconds(POOL_CLEAR_ESTIMATE_CAP_SECS);
    let mut best = cap;

    for file in list_token_files(dir) {
        let name = stem(&file);
        if let Some(entry) = blocking_entry_in_dir(dir, &name) {
            if let Some(remaining) = entry.cooldown_remaining_secs {
                let candidate = now + chrono::Duration::seconds(remaining.max(0));
                if candidate < best {
                    best = candidate;
                }
            }
        }
    }

    if let Ok(text) = std::fs::read_to_string(dir.join(".ranking")) {
        for row in text.lines().filter_map(parse_ranking_line) {
            if !is_hard_excluded(dir, &row.name, &row.status) {
                continue;
            }
            if let Some(reset) = row.limit_reset.as_deref() {
                if let Ok(naive) =
                    chrono::NaiveDateTime::parse_from_str(reset, "%Y-%m-%dT%H:%M:%SZ")
                {
                    let candidate = naive.and_utc();
                    if candidate < best {
                        best = candidate;
                    }
                }
            }
        }
    }

    best.max(now)
}

/// Shared-pool hint for the *all-excluded* error path (issue #6614).
///
/// The dir-missing and no-`.token`-files error paths above both call
/// [`shared_pool_hint`], whose "also checked" wording is accurate there:
/// [`resolve_tokens_dir`] only probes the shared pool when the repo-local one
/// holds no token files, which is exactly those two cases. Reaching the
/// all-excluded path means the opposite — the resolved pool *did* hold
/// `.token` files, so the shared pool was **never consulted**, and reusing
/// "also checked" here would assert something false.
///
/// That silence is what made the #6614 incident hard to diagnose: a stale
/// repo-local `.loom/tokens/` (weeks older than the shared pool's last
/// bootstrap) shadowed a healthy shared pool, every one of its accounts
/// accumulated a `.bad_tokens` entry, and selection reported "empty pool" with
/// no mention that several healthy accounts sat one directory away.
/// [`resolve_tokens_dir`] prefers a repo-local pool merely for *having* token
/// files, regardless of their health, so this is a reachable steady state, not
/// a transient.
///
/// Returns an empty string only when the shared pool is disabled
/// (`LOOM_SHARED_TOKENS_DIR=""`) — there is genuinely nothing to point at.
///
/// Three remaining outcomes (issue #6758 added the third): the shared pool
/// holds no `.token` files at all (not an alternative); it holds files and at
/// least one is usable (a genuine shadowing — "SHADOWED POOL", recoverable by
/// retiring the repo-local copy); or it holds files but none are usable
/// (equally exhausted — retiring the repo-local pool would not help, since
/// re-auth is needed either way).
///
/// Both "at least one pool has files" branches also name each pool's
/// **spawnable account count** (`usable/total`, issue #7527) — before this,
/// the message only said "usable: yes/no", which could not distinguish "the
/// shared pool has 4 spawnable accounts" from "it has exactly 1", the
/// difference between "readmit one account and you're fine" and "you're one
/// bad probe away from the same trap on the shared pool too".
fn shadowed_shared_pool_hint(tokens_dir: &Path) -> String {
    let Some(shared) = shared_tokens_dir() else {
        return String::new();
    };
    if shared == tokens_dir {
        return "\n  pool identity: this IS the shared machine-level pool (no repo-local pool \
                shadowed it) — the exhaustion above is genuine, not a stale-copy artifact."
            .to_string();
    }
    if !has_token_files(&shared) {
        return format!(
            "\n  shared machine-level pool {} holds no .token files either, so it is not an \
             alternative here.",
            shared.display()
        );
    }
    let this_total = list_token_files(tokens_dir).len();
    let this_usable = usable_account_count(tokens_dir);
    let shared_total = list_token_files(&shared).len();
    let shared_usable = usable_account_count(&shared);
    if shared_usable > 0 {
        return format!(
            "\n  SHADOWED POOL: a shared machine-level pool at {} also holds .token files and \
             was NOT consulted — a repo-local pool wins on merely HAVING token files, \
             regardless of health. Spawnable accounts: this pool {this_usable}/{this_total} \
             usable, shared pool {shared_usable}/{shared_total} usable. If the pool above is a \
             stale copy, re-bootstrap or remove it (`loom-daemon tokens bootstrap --force`) so \
             the shared pool is used.",
            shared.display()
        );
    }
    format!(
        "\n  shared machine-level pool {} holds .token files too, but none are currently usable \
         either (all bad-marked or .ranking-excluded) — no usable accounts anywhere \
         (this pool {this_usable}/{this_total} usable, shared pool {shared_usable}/{shared_total} \
         usable). Retiring the repo-local pool would not help; both pools need re-auth \
         (`loom-daemon tokens unblock <name>` or a fresh `loom-daemon tokens bootstrap`).",
        shared.display()
    )
}

fn read_token_file(path: &Path) -> std::io::Result<String> {
    let raw = std::fs::read_to_string(path)?;
    Ok(raw.split_whitespace().collect::<Vec<_>>().join(""))
}

fn file_age_seconds(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    std::time::SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|d| d.as_secs())
}

fn list_token_files(tokens_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(tokens_dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("token"))
        .collect();
    out.sort();
    out
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

// ---------------------------------------------------------------------------
// Provider filtering + upstream-id lookup (issue #5609, design D8/D9)
// ---------------------------------------------------------------------------

/// `index.json` rows keyed by account name — the map this selector consults
/// both to skip a non-Claude account (D4/D8: no such row should exist once
/// #5607 lands, but this is the assertion that makes the defect
/// unrepresentable even if a stale pool directory survives an upgrade) and to
/// carry `upstream_id` through to the caller (D1/AC 6). A pool with no
/// `index.json` at all — a hand-provisioned pool, or one bootstrapped before
/// #5607 — yields an empty map, so every name below falls through the
/// fail-open default: treated as `claude`, `upstream_id = None`.
fn read_manifest_index(tokens_dir: &Path) -> HashMap<String, ManifestRow> {
    read_manifest_rows(&tokens_dir.join("index.json"))
        .into_iter()
        .map(|row| (row.name.clone(), row))
        .collect()
}

/// A name whose manifest row exists and names a provider other than Claude.
/// **Not** included: a name with no manifest row at all (fail-open, D6/D8).
fn is_non_claude(manifest: &HashMap<String, ManifestRow>, name: &str) -> bool {
    manifest
        .get(name)
        .is_some_and(|row| row.provider != AccountProvider::Claude)
}

fn upstream_id_for(manifest: &HashMap<String, ManifestRow>, name: &str) -> Option<String> {
    manifest.get(name).and_then(|row| row.upstream_id.clone())
}

fn strip_comment(line: &str) -> String {
    line.split('#').next().unwrap_or("").trim().to_string()
}

/// Parse a ranking line's optional 5h-utilization field (issue #4195). An
/// empty or unparseable field yields `None` ("unknown") — never coerced to
/// `0.0`, so an unmeasured account is never load-gated (see #4164).
fn parse_util(field: &str) -> Option<f64> {
    let field = field.trim();
    if field.is_empty() {
        return None;
    }
    field.parse::<f64>().ok()
}

/// One parsed `.ranking` row.
///
/// A struct rather than a tuple (it grew a 4th field in issue #4874): every
/// reader names the fields it wants, so adding a 5th cannot silently shift a
/// positional binding the way the #4243/#4344 drift did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RankingRow {
    pub name: String,
    pub status: String,
    /// 5h-window utilization (issue #4195), when the row carries one.
    pub util_5h: Option<f64>,
    /// When the account's **binding** limit window resets (issue #4874), when
    /// the row carries one. Which window that is depends on the row's status —
    /// the writer already resolved it via
    /// [`crate::tokens_pool::check::limit_reset`]. Kept as the raw ISO-8601
    /// text the writer emitted; this parser does no time arithmetic.
    pub limit_reset: Option<String>,
}

/// Parse a single `.ranking` line into a [`RankingRow`].
///
/// This is the **shared** parser for the `name|status|5h_util|limit_reset`
/// format: `status` is the *second* pipe-delimited field, the third (5h
/// utilization, issue #4195) and fourth (binding-window reset instant, issue
/// #4874) are optional, so a legacy `name|status` or `name|status|5h_util` line
/// still parses with the absent fields as `None` (backward compatible). A row
/// that knows its reset but not its utilization writes an empty third field
/// (`name|status||limit_reset`), which parses back to `util_5h = None` — never
/// coerced to `0.0`. `#` comments are stripped; a blank/comment-only line, or
/// one whose name is empty, yields `None`.
///
/// Both the selector ([`read_ranking`]) and the daemon's capacity reader
/// ([`crate::capacity::read_ranking_at`]) consume the ranking through this one
/// function so the two readers can never de-sync on field positions again —
/// the #4243/#4344 drift where capacity treated the *last* field as the status
/// and mis-read every 3-field row's `5h_util` as the status word. A
/// format-drift conformance test (`capacity::tests`) pins them together.
#[must_use]
pub(crate) fn parse_ranking_line(line: &str) -> Option<RankingRow> {
    let stripped = strip_comment(line);
    if stripped.is_empty() {
        return None;
    }
    // `splitn(4, ..)` — NOT `splitn(3, ..)`: with a 3-way split a 4th segment
    // is swallowed into the utilization field and silently fails to parse as a
    // float, which is exactly how the reset instant would have been lost.
    let mut parts = stripped.splitn(4, '|');
    let name = parts.next().unwrap_or("").trim().to_string();
    let status = parts.next().unwrap_or("").trim().to_string();
    let util_5h = parts.next().and_then(parse_util);
    let limit_reset = parts.next().and_then(parse_reset);
    if name.is_empty() {
        return None;
    }
    Some(RankingRow {
        name,
        status,
        util_5h,
        limit_reset,
    })
}

/// Parse a ranking line's optional reset field (issue #4874). An empty
/// field yields `None` ("unknown") — the row is never given a fabricated
/// reset instant. The text is not date-validated here; the writer emits a
/// canonical `%Y-%m-%dT%H:%M:%SZ` instant and consumers that need a real
/// `DateTime` (the telemetry collector) parse it themselves and drop it on
/// failure.
fn parse_reset(field: &str) -> Option<String> {
    let field = field.trim();
    if field.is_empty() {
        return None;
    }
    Some(field.to_string())
}

/// Yield `(name, status, util_5h)` triples from the ranking file, one per
/// parseable line via the shared [`parse_ranking_line`] parser. Format:
/// `name|status|5h_util|limit_reset` per line; the third and fourth fields are
/// optional, so a legacy `name|status` line yields `util_5h = None` (backward
/// compatible). Malformed/empty lines are skipped; `status` defaults to `""`.
/// Selection ignores the reset instant — it is telemetry, not an input to the
/// tiered pick — so this projects the row down to the triple selection uses.
fn read_ranking(ranking_file: &Path) -> Vec<(String, String, Option<f64>)> {
    let Ok(text) = std::fs::read_to_string(ranking_file) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(parse_ranking_line)
        .map(|row| (row.name, row.status, row.util_5h))
        .collect()
}

fn read_allowlist_lines(allowlist_file: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(allowlist_file) else {
        return Vec::new();
    };
    text.lines()
        .map(strip_comment)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Soft-fail read of `.loom/config.json` -> `tokens.spreadTopN` through
/// [`crate::config_resolver`] (so the `.loom-project/` tier is honored like
/// every other migrated config surface, #4058/#4241). Missing file, parse
/// error, missing key, non-int, or bool all resolve to `None`. Shape copied
/// from [`crate::token_ranking_refresh::read_token_ranking_refresh_config`].
fn read_config_spread_top_n(workspace: &Path) -> Option<i64> {
    let effective = crate::config_resolver::resolve_effective_config(workspace);
    let spread = crate::config_resolver::get_path(&effective, "tokens.spreadTopN")?;
    // Reject bool (serde_json's Value::Bool is distinct from Number, so this
    // is naturally excluded) and non-integers.
    spread.as_i64()
}

/// Resolve the rotation-window cap: env > config > default (unbounded).
/// A configured/env value `<= 0` also means unbounded. `Some(1)` restores
/// the historical greedy first-eligible behavior.
fn resolve_spread_top_n(workspace: &Path) -> Option<usize> {
    if let Ok(raw) = std::env::var("LOOM_TOKEN_SPREAD_TOP_N") {
        let n: i64 = raw.trim().parse().unwrap_or(0);
        return if n >= 1 { Some(n as usize) } else { None };
    }
    if let Some(n) = read_config_spread_top_n(workspace) {
        return if n >= 1 { Some(n as usize) } else { None };
    }
    None
}

/// Resolve the tier-1 5h-load threshold (issue #4195): `LOOM_TOKEN_5H_LOAD_GATE`
/// env var (parsed as a float) → the constant default [`DEFAULT_5H_LOAD_GATE`].
/// An unset or unparseable env value falls back to the default. Mirrors
/// `select.py:_resolve_load_gate` so both implementations gate identically.
///
/// `pub(crate)` (rather than private) so [`super::bad_tokens`]'s ambiguous-entry
/// early-release check (#7538) reuses this exact resolution — including the env
/// override — instead of duplicating the threshold.
pub(crate) fn resolve_load_gate() -> f64 {
    if let Ok(raw) = std::env::var("LOOM_TOKEN_5H_LOAD_GATE") {
        if let Ok(v) = raw.trim().parse::<f64>() {
            return v;
        }
    }
    DEFAULT_5H_LOAD_GATE
}

// Eight parameters, one over clippy's default. Collapsing them into a struct
// would add a type whose only purpose is to carry this call's locals two frames
// — `try_ranking` calls this twice, with one field flipped — so the threshold is
// waived rather than worked around.
#[allow(clippy::too_many_arguments)]
fn collect_ranked_candidates(
    tokens_dir: &Path,
    ranking_file: &Path,
    workspace: &Path,
    cap: Option<usize>,
    healthy_only: bool,
    load_gate: f64,
    manifest: &HashMap<String, ManifestRow>,
    model_class: Option<&str>,
) -> Vec<SelectedToken> {
    let mut out = Vec::new();
    for (name, status, util) in read_ranking(ranking_file) {
        if is_hard_excluded(tokens_dir, &name, &status) {
            continue;
        }
        if healthy_only && !is_healthy_status(&status) {
            continue;
        }
        // #5609: a manifest row naming a non-Claude provider is never
        // selected by the Claude selector, regardless of ranking status.
        if is_non_claude(manifest, &name) {
            continue;
        }
        // Load gate (issue #4195): the preferred pass additionally excludes
        // accounts at/above the 5h-window load threshold. An unknown
        // (unmeasured) utilization is never gated. The fallback pass drops the
        // gate so a fully-loaded pool still dispatches. The rotation cursor
        // then rotates across the load-eligible set, so no per-spawn in-burst
        // bump is needed — the cursor already prevents intra-burst stacking.
        if healthy_only {
            if let Some(u) = util {
                if u >= load_gate {
                    continue;
                }
            }
        }
        let token_file = tokens_dir.join(format!("{name}.token"));
        if !token_file.is_file() {
            continue;
        }
        if is_bad_for_class(workspace, &name, model_class) {
            continue;
        }
        let Ok(key) = read_token_file(&token_file) else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        let upstream_id = upstream_id_for(manifest, &name);
        out.push(SelectedToken {
            name,
            file: token_file,
            key,
            mode: "ranked",
            upstream_id,
        });
        if let Some(cap) = cap {
            if out.len() >= cap {
                break;
            }
        }
    }
    out
}

/// Strategy 1: read `.ranking`, rotate one-per-account across eligible
/// entries.
fn try_ranking(
    tokens_dir: &Path,
    ranking_file: &Path,
    workspace: &Path,
    rng: &mut Rng,
    manifest: &HashMap<String, ManifestRow>,
    model_class: Option<&str>,
) -> Option<SelectedToken> {
    let age = file_age_seconds(ranking_file)?;
    if age >= RANKING_FRESH_SECONDS {
        return None;
    }

    let cap = resolve_spread_top_n(workspace);
    let load_gate = resolve_load_gate();

    let mut eligible = collect_ranked_candidates(
        tokens_dir,
        ranking_file,
        workspace,
        cap,
        true,
        load_gate,
        manifest,
        model_class,
    );
    if eligible.is_empty() {
        eligible = collect_ranked_candidates(
            tokens_dir,
            ranking_file,
            workspace,
            cap,
            false,
            load_gate,
            manifest,
            model_class,
        );
    }
    if eligible.is_empty() {
        return None;
    }
    let index = next_rotation_index(tokens_dir, eligible.len(), rng);
    Some(eligible.swap_remove(index))
}

/// Hard exclusion set sourced from the `.ranking` file's status field,
/// **regardless of the file's age** (issue #5629): `name -> status` for every
/// row [`is_hard_excluded`] in `tokens_dir`.
///
/// Tier 1 already refuses to rank these in any pass, but before #5629 that
/// knowledge reached tiers 2/3 only via [`stale_ranking_exclusions`], which
/// fires only once the ranking has gone stale. A *fresh* ranking whose only
/// rows were `exhausted` therefore produced no tier-1 candidate **and** an
/// empty exclusion set, so tier 3 (`mode=random`, which consults only
/// `.bad_tokens`) handed back the very account the ranking had ruled out.
///
/// Returned as a map rather than a set so the empty-pool error can name the
/// offending status per account ([`describe_exclusion`]).
fn ranking_hard_exclusions(tokens_dir: &Path, ranking_file: &Path) -> HashMap<String, String> {
    read_ranking(ranking_file)
        .into_iter()
        .filter(|(name, status, _)| is_hard_excluded(tokens_dir, name, status))
        .map(|(name, status, _)| (name, status))
        .collect()
}

/// Advisory exclusion set sourced from a *stale* `.ranking` (issue #3894).
///
/// Hard-excluded statuses are deliberately **not** filtered out here — they are
/// a subset of "non-healthy" and are unioned into the same exclusion set by
/// [`select_token`]; the distinction only matters for the fail-safe retry,
/// which re-runs with the hard set and drops just the advisory remainder.
fn stale_ranking_exclusions(ranking_file: &Path) -> HashSet<String> {
    match file_age_seconds(ranking_file) {
        Some(age) if age >= RANKING_FRESH_SECONDS => read_ranking(ranking_file)
            .into_iter()
            .filter(|(_, status, _)| !is_healthy_status(status))
            .map(|(name, _, _)| name)
            .collect(),
        _ => HashSet::new(),
    }
}

/// Strategy 2: random pick from the allowlist.
fn try_allowlist(
    tokens_dir: &Path,
    allowlist_file: &Path,
    workspace: &Path,
    rng: &mut Rng,
    exclude: &HashSet<String>,
    model_class: Option<&str>,
) -> Option<SelectedToken> {
    if !allowlist_file.is_file() {
        return None;
    }
    let mut eligible: Vec<PathBuf> = read_allowlist_lines(allowlist_file)
        .into_iter()
        .filter(|name| !exclude.contains(name))
        .map(|name| tokens_dir.join(format!("{name}.token")))
        .filter(|f| f.is_file() && !is_bad_for_class(workspace, &stem(f), model_class))
        .collect();
    if eligible.is_empty() {
        return None;
    }
    rng.shuffle(&mut eligible);
    for token_file in eligible {
        let Ok(key) = read_token_file(&token_file) else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        return Some(SelectedToken {
            name: stem(&token_file),
            file: token_file,
            key,
            mode: "allowlist",
            // Patched by the caller (`select_token`) once the manifest is
            // available — `exclude` already strips non-Claude names before
            // this function ever sees them, so no filtering happens here.
            upstream_id: None,
        });
    }
    None
}

/// Strategy 3: random pick from all tokens.
fn try_random(
    tokens_dir: &Path,
    workspace: &Path,
    rng: &mut Rng,
    exclude: &HashSet<String>,
    model_class: Option<&str>,
) -> Option<SelectedToken> {
    let mut candidates: Vec<PathBuf> = list_token_files(tokens_dir)
        .into_iter()
        .filter(|p| {
            !is_bad_for_class(workspace, &stem(p), model_class) && !exclude.contains(&stem(p))
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    rng.shuffle(&mut candidates);
    for token_file in candidates {
        let Ok(key) = read_token_file(&token_file) else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        return Some(SelectedToken {
            name: stem(&token_file),
            file: token_file,
            key,
            mode: "random",
            // Patched by the caller (`select_token`) — see `try_allowlist`.
            upstream_id: None,
        });
    }
    None
}

/// Select an OAuth token using the 3-tier algorithm.
///
/// `workspace` should be the canonical repo root containing `.loom/tokens/`
/// (the *main* checkout root when called from a worktree). Pass `rng: None`
/// for production use (entropy-seeded); tests inject a seeded [`Rng`] for
/// determinism.
///
/// # Errors
/// Returns [`EmptyTokenPoolError`] when `.loom/tokens/` is missing, holds no
/// `.token` files, or every token is marked bad.
pub fn select_token(
    workspace: &Path,
    rng: Option<&mut Rng>,
) -> Result<SelectedToken, EmptyTokenPoolError> {
    select_token_for_class(workspace, rng, None)
}

/// [`select_token`], skipping only accounts bad-marked **for the model class
/// `model` belongs to** (issue #8058).
///
/// `model` is a raw model alias or pinned ID — whatever reached
/// `tokens select --model` / `$LOOM_MODEL` — resolved here through
/// [`super::bad_tokens::model_class_of`], the same classifier the sweep
/// orchestrator's cost ladder uses. `None`, an empty value, **or a model the
/// classifier does not recognize** all reproduce [`select_token`] exactly:
/// every `.bad_tokens` entry blocks, whatever class it names. Selection must
/// never fail closed on a model name it does not know, so an unrecognized
/// value warns on stderr and degrades rather than erroring.
///
/// With a recognized model, an account whose only live mark is scoped to a
/// *different* class stays selectable. This is the Opus-starves-Sonnet fix: an
/// account that hit its Opus ceiling is still able to run Sonnet work.
///
/// The narrowing is confined to `.bad_tokens`. The `.ranking`-sourced hard and
/// advisory exclusion sets ([`is_hard_excluded`], [`stale_ranking_exclusions`])
/// stay account-wide, because `.ranking` carries no per-class state to narrow
/// them with (that is #8058's Phase 3) — so this can only ever readmit an
/// account on class-scoped `.bad_tokens` evidence, never on a guess.
///
/// # Errors
/// Same as [`select_token`].
pub fn select_token_for_model(
    workspace: &Path,
    rng: Option<&mut Rng>,
    model: Option<&str>,
) -> Result<SelectedToken, EmptyTokenPoolError> {
    let model_class = match model.map(str::trim).filter(|m| !m.is_empty()) {
        Some(raw) => match super::bad_tokens::model_class_of(raw) {
            Some(class) => Some(class),
            None => {
                eprintln!(
                    "warning: model '{raw}' is not a recognized Claude model class; \
                     selecting account-wide (no per-class skip)"
                );
                None
            }
        },
        None => None,
    };
    select_token_for_class(workspace, rng, model_class.as_deref())
}

/// [`select_token_for_model`] with an **already-normalized** class
/// (`haiku`/`sonnet`/`opus`/`fable`). Callers that hold a raw model should use
/// [`select_token_for_model`] so the classification happens in exactly one
/// place; this is the seam the tests and the tiers below share.
///
/// # Errors
/// Same as [`select_token`].
pub fn select_token_for_class(
    workspace: &Path,
    rng: Option<&mut Rng>,
    model_class: Option<&str>,
) -> Result<SelectedToken, EmptyTokenPoolError> {
    let tokens_dir = resolve_tokens_dir(workspace);

    if !tokens_dir.is_dir() {
        return Err(EmptyTokenPoolError(format!(
            "Token directory does not exist: {}{}. Run `loom-daemon tokens bootstrap` to populate it \
             (or `loom-daemon tokens bootstrap --shared` for the machine-level pool).",
            tokens_dir.display(),
            shared_pool_hint()
        )));
    }

    let all_tokens = list_token_files(&tokens_dir);
    if all_tokens.is_empty() {
        return Err(EmptyTokenPoolError(format!(
            "No .token files in {}{}. Run `loom-daemon tokens bootstrap` \
             (or `loom-daemon tokens bootstrap --shared` for the machine-level pool).",
            tokens_dir.display(),
            shared_pool_hint()
        )));
    }

    let mut owned_rng;
    let rng: &mut Rng = match rng {
        Some(r) => r,
        None => {
            owned_rng = Rng::from_entropy();
            &mut owned_rng
        }
    };

    let ranking_file = tokens_dir.join(".ranking");
    let allowlist_file = tokens_dir.join(".allowlist");

    // #5609 (design D8/D9): `index.json` rows keyed by name, consulted below
    // both to skip a non-Claude account and to carry `upstream_id` through to
    // the caller. A pool with no manifest is an empty map — fail-open, D6/D8.
    let manifest = read_manifest_index(&tokens_dir);

    if let Some(selected) =
        try_ranking(&tokens_dir, &ranking_file, workspace, rng, &manifest, model_class)
    {
        return Ok(selected);
    }

    // Three exclusion sets, folded into one `hard`/`exclude` pair (#5629,
    // #5609):
    //   hard       — `exhausted`/`blocked` at ANY ranking age; never readmitted.
    //   non_claude — a manifest row naming a provider other than Claude
    //                (D4/D8); durable and permanent like `hard`, so it is
    //                folded into `hard` rather than only `exclude` — the
    //                fail-safe retry below must never readmit it either.
    //   advisory   — other non-healthy statuses from a *stale* ranking
    //                (#3894); readmitted by the fail-safe if they would empty
    //                the pool.
    let hard_map = ranking_hard_exclusions(&tokens_dir, &ranking_file);
    let mut hard: HashSet<String> = hard_map.keys().cloned().collect();
    let non_claude: HashSet<String> = manifest
        .iter()
        .filter(|(_, row)| row.provider != AccountProvider::Claude)
        .map(|(name, _)| name.clone())
        .collect();
    hard.extend(non_claude.iter().cloned());
    let mut exclude = stale_ranking_exclusions(&ranking_file);
    exclude.extend(hard.iter().cloned());

    if let Some(mut selected) =
        try_allowlist(&tokens_dir, &allowlist_file, workspace, rng, &exclude, model_class)
    {
        selected.upstream_id = upstream_id_for(&manifest, &selected.name);
        return Ok(selected);
    }
    if let Some(mut selected) = try_random(&tokens_dir, workspace, rng, &exclude, model_class) {
        selected.upstream_id = upstream_id_for(&manifest, &selected.name);
        return Ok(selected);
    }

    // Fail-safe: the *advisory* exclusions emptied the pool. Retry with only
    // the hard exclusions still in force, so a live pool never hard-fails on
    // stale advice — but an account the ranking positively reports as
    // exhausted/blocked, or that the manifest positively reports as a
    // non-Claude provider, is still never handed out.
    if exclude.len() > hard.len() {
        if let Some(mut selected) =
            try_allowlist(&tokens_dir, &allowlist_file, workspace, rng, &hard, model_class)
        {
            selected.upstream_id = upstream_id_for(&manifest, &selected.name);
            return Ok(selected);
        }
        if let Some(mut selected) = try_random(&tokens_dir, workspace, rng, &hard, model_class) {
            selected.upstream_id = upstream_id_for(&manifest, &selected.name);
            return Ok(selected);
        }
    }

    // Per-token exclusion detail (#4643): say WHY each account was excluded and
    // WHICH binary decided, so a recurrence is diagnosable from the sweep log
    // alone instead of by reading this source file.
    let detail: String = all_tokens
        .iter()
        .map(|f| format!("\n  - {}", describe_exclusion(workspace, f, &hard_map, &manifest)))
        .collect();
    // #8058: name the class the selection was scoped to, so an operator
    // reading an empty-pool failure can tell "every account is dead" apart
    // from "every account is dead *for opus*".
    let class_note = match model_class {
        Some(class) => format!(
            "\n  model class: {class} (accounts bad-marked only for another class were \
             still eligible)"
        ),
        None => String::new(),
    };
    Err(EmptyTokenPoolError(format!(
        "All {} tokens in {} are marked bad, empty, or .ranking-excluded.{detail}{class_note}\n  \
         deciding binary: {}\n  \
         exhaustion cooldown: {}s (override {EXHAUSTION_COOLDOWN_ENV}); auth entries never \
         expire — clear them with `loom-daemon tokens unblock <name>` \
         (add --all-reasons to drop non-auth entries too).\n  \
         Inspect .bad_tokens or run `loom-daemon tokens bootstrap --force`.{}",
        all_tokens.len(),
        tokens_dir.display(),
        deciding_binary_identity(),
        exhaustion_cooldown_secs(),
        shadowed_shared_pool_hint(&tokens_dir),
    )))
}

#[cfg(test)]
#[path = "select_tests.rs"]
mod tests;
