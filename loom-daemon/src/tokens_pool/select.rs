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
//! skipped. The `.ranking` file contributes two distinct exclusion sets to
//! tiers 2/3:
//!
//! - **Hard** ([`is_hard_excluded_status`]: `exhausted` / `blocked`) — applied
//!   at *any* ranking age, and never dropped by the fail-safe (issue #5629).
//!   Tier 1 has always hard-excluded these in every pass; before #5629 the
//!   knowledge stopped there unless the ranking happened to be stale, so a
//!   fresh ranking whose only rows were `exhausted` fell through to a tier-3
//!   `mode=random` pick of exactly the account it had just ruled out.
//! - **Advisory** (any other non-healthy status, e.g. `rate_limited`) — only
//!   sourced from a *stale* `.ranking` (issue #3894), and dropped by a
//!   fail-safe retry if the exclusions would otherwise empty the pool.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::account_registry::AccountProvider;
use super::bad_tokens::{
    blocking_entry, exhaustion_cooldown_secs, is_bad, EXHAUSTION_COOLDOWN_ENV,
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

/// Statuses hard-excluded from **every** tier in *every* pass, including the
/// tier-1 empty-pool fallback pass and the tier-2/3 fail-safe retry (#5629).
///
/// These are durable, account-scoped refusals (a hit weekly/monthly limit, a
/// blocked account) rather than transient load signals: handing one out
/// "because the pool would otherwise be empty" does not produce a working
/// spawn, it produces a spawn that burns its retry budget and dies.
fn is_hard_excluded_status(status: &str) -> bool {
    status == "exhausted" || status == "blocked"
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
        return format!(
            "{name}: hard-excluded by .ranking status ({status}) — never readmitted by the \
             fail-safe; re-probe with `loom-daemon tokens check --ranking`"
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
fn shadowed_shared_pool_hint(tokens_dir: &Path) -> String {
    let Some(shared) = shared_tokens_dir() else {
        return String::new();
    };
    if shared == tokens_dir {
        return "\n  pool identity: this IS the shared machine-level pool (no repo-local pool \
                shadowed it) — the exhaustion above is genuine, not a stale-copy artifact."
            .to_string();
    }
    if has_token_files(&shared) {
        return format!(
            "\n  SHADOWED POOL: a shared machine-level pool at {} also holds .token files and \
             was NOT consulted — a repo-local pool wins on merely HAVING token files, \
             regardless of health. If the pool above is a stale copy, re-bootstrap or remove \
             it (`loom-daemon tokens bootstrap --force`) so the shared pool is used.",
            shared.display()
        );
    }
    format!(
        "\n  shared machine-level pool {} holds no .token files either, so it is not an \
         alternative here.",
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
fn resolve_load_gate() -> f64 {
    if let Ok(raw) = std::env::var("LOOM_TOKEN_5H_LOAD_GATE") {
        if let Ok(v) = raw.trim().parse::<f64>() {
            return v;
        }
    }
    DEFAULT_5H_LOAD_GATE
}

fn collect_ranked_candidates(
    tokens_dir: &Path,
    ranking_file: &Path,
    workspace: &Path,
    cap: Option<usize>,
    healthy_only: bool,
    load_gate: f64,
    manifest: &HashMap<String, ManifestRow>,
) -> Vec<SelectedToken> {
    let mut out = Vec::new();
    for (name, status, util) in read_ranking(ranking_file) {
        if is_hard_excluded_status(&status) {
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
        if is_bad(workspace, &name) {
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
/// row whose status [`is_hard_excluded_status`].
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
fn ranking_hard_exclusions(ranking_file: &Path) -> HashMap<String, String> {
    read_ranking(ranking_file)
        .into_iter()
        .filter(|(_, status, _)| is_hard_excluded_status(status))
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
) -> Option<SelectedToken> {
    if !allowlist_file.is_file() {
        return None;
    }
    let mut eligible: Vec<PathBuf> = read_allowlist_lines(allowlist_file)
        .into_iter()
        .filter(|name| !exclude.contains(name))
        .map(|name| tokens_dir.join(format!("{name}.token")))
        .filter(|f| f.is_file() && !is_bad(workspace, &stem(f)))
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
) -> Option<SelectedToken> {
    let mut candidates: Vec<PathBuf> = list_token_files(tokens_dir)
        .into_iter()
        .filter(|p| !is_bad(workspace, &stem(p)) && !exclude.contains(&stem(p)))
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

    if let Some(selected) = try_ranking(&tokens_dir, &ranking_file, workspace, rng, &manifest) {
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
    let hard_map = ranking_hard_exclusions(&ranking_file);
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
        try_allowlist(&tokens_dir, &allowlist_file, workspace, rng, &exclude)
    {
        selected.upstream_id = upstream_id_for(&manifest, &selected.name);
        return Ok(selected);
    }
    if let Some(mut selected) = try_random(&tokens_dir, workspace, rng, &exclude) {
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
            try_allowlist(&tokens_dir, &allowlist_file, workspace, rng, &hard)
        {
            selected.upstream_id = upstream_id_for(&manifest, &selected.name);
            return Ok(selected);
        }
        if let Some(mut selected) = try_random(&tokens_dir, workspace, rng, &hard) {
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
    Err(EmptyTokenPoolError(format!(
        "All {} tokens in {} are marked bad, empty, or .ranking-excluded.{detail}\n  \
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
mod tests {
    use super::*;
    use std::fs;

    // `SHARED_TOKENS_DIR_ENV` / `LOOM_TOKEN_SPREAD_TOP_N` are process-global;
    // `#[serial]` (serial_test's default unkeyed group) serializes against
    // every other unkeyed `#[serial]` test in the crate, including the
    // `SHARED_TOKENS_DIR_ENV` mutations in `paths.rs`.
    use serial_test::serial;

    fn make_pool(names: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".loom").join("tokens");
        fs::create_dir_all(&dir).unwrap();
        for n in names {
            fs::write(dir.join(format!("{n}.token")), format!("key-{n}")).unwrap();
        }
        tmp
    }

    fn pool_dir(ws: &Path) -> PathBuf {
        ws.join(".loom").join("tokens")
    }

    #[test]
    #[serial]
    fn errors_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");
        let err = select_token(tmp.path(), None).unwrap_err();
        assert!(err.0.contains("does not exist"));
        std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn errors_when_no_token_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(pool_dir(tmp.path())).unwrap();
        std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");
        let err = select_token(tmp.path(), None).unwrap_err();
        assert!(err.0.contains("No .token files"));
        std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
    }

    #[test]
    fn single_token_selected_via_random_tier() {
        let tmp = make_pool(&["only"]);
        let mut rng = Rng::seeded(1);
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "only");
        assert_eq!(sel.mode, "random");
        assert_eq!(sel.key, "key-only");
        // #5609: no index.json manifest at all -> fail-open, no upstream_id.
        assert_eq!(sel.upstream_id, None);
    }

    #[test]
    fn bad_token_is_skipped_in_random_tier() {
        let tmp = make_pool(&["a", "b"]);
        super::super::bad_tokens::mark_bad(tmp.path(), "a", "exhausted").unwrap();
        let mut rng = Rng::seeded(1);
        for _ in 0..10 {
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            assert_eq!(sel.name, "b");
        }
    }

    /// #6030: an auth-dead account (`claude-wrapper.sh`'s
    /// `"auth-dead: ..."` mark-bad reason for a 401/invalid-bearer-token
    /// death) is excluded the same way an exhausted one is — and, unlike
    /// exhaustion, it never times back in on the cooldown.
    #[test]
    fn auth_dead_token_is_skipped_in_random_tier() {
        let tmp = make_pool(&["a", "b"]);
        super::super::bad_tokens::mark_bad(tmp.path(), "a", "auth-dead: 401 Invalid bearer token")
            .unwrap();
        let mut rng = Rng::seeded(1);
        for _ in 0..10 {
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            assert_eq!(sel.name, "b");
        }
    }

    #[test]
    fn allowlist_tier_restricts_selection() {
        let tmp = make_pool(&["a", "b", "c"]);
        fs::write(pool_dir(tmp.path()).join(".allowlist"), "b\n").unwrap();
        let mut rng = Rng::seeded(1);
        for _ in 0..10 {
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            assert_eq!(sel.name, "b");
            assert_eq!(sel.mode, "allowlist");
        }
    }

    #[test]
    fn fresh_ranking_prefers_healthy_status_over_random() {
        let tmp = make_pool(&["a", "b"]);
        fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available\nb|exhausted\n").unwrap();
        let mut rng = Rng::seeded(1);
        for _ in 0..5 {
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            assert_eq!(sel.name, "a");
            assert_eq!(sel.mode, "ranked");
        }
    }

    #[test]
    fn ranking_hard_excludes_exhausted_and_blocked_even_in_fallback() {
        let tmp = make_pool(&["a", "b"]);
        // No healthy entries at all; fallback pass must still exclude
        // exhausted/blocked, leaving nothing ranked -> falls to random/allow.
        fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\nb|blocked\n").unwrap();
        let mut rng = Rng::seeded(1);
        // #5629: the hard exclusion now propagates to tiers 2/3 as well, so
        // there is no eligible account left and selection fails fast instead
        // of handing out an account the ranking already knows is dead.
        let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
        assert!(err.0.contains("a: hard-excluded by .ranking status"), "{}", err.0);
        assert!(err.0.contains("b: hard-excluded by .ranking status"), "{}", err.0);
    }

    // ---- fresh-ranking hard exclusions reach tiers 2/3 (issue #5629) ----

    /// #5629: a **fresh** `.ranking` marking the sole account `exhausted` must
    /// not be handed out by the tier-3 random fallback. Before the fix,
    /// `stale_ranking_exclusions` only produced a non-empty exclusion set for a
    /// *stale* ranking, so `try_random` ran with an empty exclusion set and
    /// happily returned the account the ranking already knew was dead —
    /// exactly the `mode=random` spawn observed on 2026-08-07.
    #[test]
    fn fresh_ranking_exhausted_is_not_handed_out_by_random_tier() {
        let tmp = make_pool(&["a"]);
        fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\n").unwrap();
        let mut rng = Rng::seeded(1);
        let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
        assert!(
            err.0
                .contains("a: hard-excluded by .ranking status (exhausted)"),
            "{}",
            err.0
        );
        // The fail-safe must NOT readmit a hard-excluded account.
        assert!(err.0.contains("loom-daemon tokens check --ranking"), "{}", err.0);
    }

    /// #5629: with one exhausted and one eligible account in a fresh ranking,
    /// the eligible account is selected — the fix must not turn a partially
    /// exhausted pool into a hard failure.
    #[test]
    fn fresh_ranking_exhausted_skipped_in_favor_of_eligible_account() {
        let tmp = make_pool(&["a", "b"]);
        // `b` has no ranking row at all, so tier 1 has no candidate (`a` is
        // hard-excluded) and selection falls through to the random tier.
        fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\n").unwrap();
        let mut rng = Rng::seeded(1);
        for _ in 0..10 {
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            assert_eq!(sel.name, "b");
            assert_eq!(sel.mode, "random");
        }
    }

    /// #5629: the same exclusion applies to tier 2 (`.allowlist`) — an
    /// allowlisted account marked `exhausted` in a fresh ranking is skipped in
    /// favor of another allowlisted account.
    #[test]
    fn fresh_ranking_exhausted_is_excluded_from_allowlist_tier() {
        let tmp = make_pool(&["a", "b"]);
        fs::write(pool_dir(tmp.path()).join(".allowlist"), "a\nb\n").unwrap();
        fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\n").unwrap();
        let mut rng = Rng::seeded(1);
        for _ in 0..10 {
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            assert_eq!(sel.name, "b");
            assert_eq!(sel.mode, "allowlist");
        }
    }

    /// #5629 / #3894 interaction: a *stale* ranking's non-hard statuses stay
    /// **advisory** (the fail-safe readmits them rather than emptying the
    /// pool), while its hard statuses stay hard.
    #[test]
    fn stale_ranking_advisory_exclusion_still_fails_safe_alongside_hard_exclusions() {
        let tmp = make_pool(&["a", "b"]);
        let ranking = pool_dir(tmp.path()).join(".ranking");
        // `a` is hard-excluded (exhausted); `b` is only advisory-excluded
        // (rate_limited is non-healthy but not hard). Excluding both would
        // empty the pool -> the fail-safe must readmit `b` only.
        fs::write(&ranking, "a|exhausted\nb|rate_limited\n").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(700);
        let f = fs::File::open(&ranking).unwrap();
        f.set_modified(old).unwrap();

        let mut rng = Rng::seeded(1);
        for _ in 0..10 {
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            assert_eq!(sel.name, "b");
        }
    }

    #[test]
    fn ranking_fallback_pass_admits_rate_limited_when_nothing_healthy() {
        let tmp = make_pool(&["a"]);
        fs::write(pool_dir(tmp.path()).join(".ranking"), "a|rate_limited\n").unwrap();
        let mut rng = Rng::seeded(1);
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "a");
        assert_eq!(sel.mode, "ranked");
    }

    #[test]
    fn stale_ranking_is_ignored_by_tier1_but_excludes_from_lower_tiers() {
        let tmp = make_pool(&["a", "b"]);
        let ranking = pool_dir(tmp.path()).join(".ranking");
        fs::write(&ranking, "a|exhausted\nb|available\n").unwrap();
        // Backdate the ranking file well past the freshness window.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(700);
        let f = fs::File::open(&ranking).unwrap();
        f.set_modified(old).unwrap();

        let mut rng = Rng::seeded(1);
        for _ in 0..10 {
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            // "a" is advisory-excluded (stale ranking said exhausted); only
            // "b" should ever be picked in the lower tiers.
            assert_eq!(sel.name, "b");
        }
    }

    #[test]
    fn stale_ranking_exclusions_fail_safe_when_pool_would_empty() {
        let tmp = make_pool(&["a"]);
        let ranking = pool_dir(tmp.path()).join(".ranking");
        // `rate_limited` is non-healthy but NOT hard-excluded, so it is only
        // advisory (#3894) — the fail-safe must readmit it.
        fs::write(&ranking, "a|rate_limited\n").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(700);
        let f = fs::File::open(&ranking).unwrap();
        f.set_modified(old).unwrap();

        let mut rng = Rng::seeded(1);
        // Excluding "a" would empty the pool -> fail-safe retry ignoring
        // advisory exclusions must still return "a".
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "a");
    }

    /// #5629: the fail-safe above must NOT readmit a *hard*-excluded account.
    /// `exhausted`/`blocked` are durable, account-scoped refusals — handing
    /// one out "because the pool would otherwise be empty" is what burned
    /// ~17 minutes per role tick on 2026-08-07.
    #[test]
    fn stale_ranking_fail_safe_does_not_readmit_hard_excluded_account() {
        let tmp = make_pool(&["a"]);
        let ranking = pool_dir(tmp.path()).join(".ranking");
        fs::write(&ranking, "a|exhausted\n").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(700);
        let f = fs::File::open(&ranking).unwrap();
        f.set_modified(old).unwrap();

        let mut rng = Rng::seeded(1);
        let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
        assert!(
            err.0
                .contains("a: hard-excluded by .ranking status (exhausted)"),
            "{}",
            err.0
        );
    }

    #[test]
    fn all_bad_tokens_errors() {
        let tmp = make_pool(&["a", "b"]);
        super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();
        super::super::bad_tokens::mark_bad(tmp.path(), "b", "x").unwrap();
        let mut rng = Rng::seeded(1);
        let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
        assert!(err.0.contains("marked bad"));
    }

    // ---- shared-pool hint on the all-excluded path (issue #6614) --------
    //
    // The dir-missing / no-`.token`-files paths have named a possible shared
    // pool since #3938; the all-excluded path — the one actually hit when a
    // stale repo-local pool shadows a healthy shared one — did not, which is
    // how the #6614 incident reported "empty pool" while several healthy
    // shared accounts sat one directory away.

    /// A healthy shared pool that the repo-local pool shadowed must be named
    /// loudly, since [`resolve_tokens_dir`] never consulted it.
    #[test]
    #[serial]
    fn all_excluded_error_names_a_shadowing_shared_pool() {
        let tmp = make_pool(&["a", "b"]);
        super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();
        super::super::bad_tokens::mark_bad(tmp.path(), "b", "x").unwrap();

        let shared = tempfile::tempdir().unwrap();
        fs::write(shared.path().join("healthy.token"), "key-healthy").unwrap();
        std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, shared.path());

        let mut rng = Rng::seeded(1);
        let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
        std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);

        let text = err.0;
        // The pre-existing detail is unchanged …
        assert!(text.contains("marked bad"), "{text}");
        // … and the shadowed healthy pool is now named, with its path.
        assert!(text.contains("SHADOWED POOL"), "{text}");
        assert!(text.contains(&shared.path().display().to_string()), "{text}");
        assert!(text.contains("was NOT consulted"), "{text}");
    }

    /// When the exhausted pool IS the shared one, say so — that is a genuine
    /// exhaustion, not a stale-repo-local-copy artifact, and the operator
    /// should not go hunting for a shadowed alternative.
    #[test]
    #[serial]
    fn all_excluded_error_marks_a_genuinely_exhausted_shared_pool() {
        let tmp = make_pool(&["a"]);
        super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();
        // Point the shared-pool env at the very dir that was resolved.
        std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, pool_dir(tmp.path()));

        let mut rng = Rng::seeded(1);
        let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
        std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);

        let text = err.0;
        assert!(text.contains("this IS the shared machine-level pool"), "{text}");
        assert!(!text.contains("SHADOWED POOL"), "{text}");
    }

    /// A configured-but-empty shared pool is reported as a non-alternative
    /// rather than dangled as a false lead.
    #[test]
    #[serial]
    fn all_excluded_error_reports_an_empty_shared_pool_as_no_alternative() {
        let tmp = make_pool(&["a"]);
        super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();

        let shared = tempfile::tempdir().unwrap();
        std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, shared.path());

        let mut rng = Rng::seeded(1);
        let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
        std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);

        let text = err.0;
        assert!(text.contains("holds no .token files either"), "{text}");
        assert!(!text.contains("SHADOWED POOL"), "{text}");
    }

    /// With the shared pool disabled outright there is nothing to point at,
    /// and the message must not grow a dangling hint.
    #[test]
    #[serial]
    fn all_excluded_error_omits_the_hint_when_shared_pool_is_disabled() {
        let tmp = make_pool(&["a"]);
        super::super::bad_tokens::mark_bad(tmp.path(), "a", "x").unwrap();
        std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");

        let mut rng = Rng::seeded(1);
        let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
        std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);

        let text = err.0;
        assert!(text.contains("marked bad"), "{text}");
        assert!(!text.contains("shared machine-level pool"), "{text}");
    }

    // ---- empty-pool error detail (issue #4643) ------------------------

    #[test]
    fn format_secs_renders_compact_durations() {
        assert_eq!(format_secs(5 * 3600 + 48 * 60), "5h48m");
        assert_eq!(format_secs(48 * 60 + 12), "48m12s");
        assert_eq!(format_secs(12), "12s");
        assert_eq!(format_secs(-5), "0s");
    }

    /// #4643: the empty-pool error names every excluded account, its exclusion
    /// cause, the reason class (auth = permanent vs exhaustion = TTL), the
    /// entry's own timestamp, the cooldown remaining, and the deciding binary.
    #[test]
    fn empty_pool_error_enumerates_per_token_exclusion_detail() {
        let tmp = make_pool(&["exh", "auth"]);
        let ts = (chrono::Utc::now() - chrono::Duration::seconds(3600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        fs::write(
            pool_dir(tmp.path()).join(".bad_tokens"),
            format!(
                "{ts} exh exhausted: hit your session limit\n\
                 {ts} auth 401 unauthorized\n"
            ),
        )
        .unwrap();

        let mut rng = Rng::seeded(1);
        let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
        let text = err.0;

        // Per-token lines, with class + permanence + timestamp + reason.
        assert!(text.contains("exh: bad-marked [exhaustion, TTL]"), "{text}");
        assert!(text.contains("auth: bad-marked [auth, permanent]"), "{text}");
        assert!(text.contains(&ts), "{text}");
        assert!(text.contains("exhausted: hit your session limit"), "{text}");
        assert!(text.contains("401 unauthorized"), "{text}");
        // Cooldown remaining for the TTL entry (~5h of the 6h default left),
        // and the operator action for the permanent one.
        assert!(text.contains("clears in 4h") || text.contains("clears in 5h"), "{text}");
        assert!(text.contains("needs `loom-daemon tokens unblock auth`"), "{text}");
        // Deciding binary + the cooldown knob.
        assert!(text.contains("deciding binary: loom-daemon "), "{text}");
        assert!(text.contains(crate::self_update::BUILT_COMMIT), "{text}");
        assert!(text.contains(EXHAUSTION_COOLDOWN_ENV), "{text}");
    }

    /// #4643: a token whose file is present but empty is reported as such,
    /// not lumped in with the bad-marked ones.
    #[test]
    #[serial]
    fn empty_pool_error_distinguishes_empty_token_files() {
        let tmp = make_pool(&["blank"]);
        fs::write(pool_dir(tmp.path()).join("blank.token"), "   \n").unwrap();
        std::env::set_var(super::super::paths::SHARED_TOKENS_DIR_ENV, "");
        let mut rng = Rng::seeded(1);
        let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
        std::env::remove_var(super::super::paths::SHARED_TOKENS_DIR_ENV);
        assert!(err.0.contains("blank: empty .token file"), "{}", err.0);
    }

    /// #4643: a malformed `.bad_tokens` timestamp shows up as fail-closed
    /// permanent in the detail rather than as a TTL entry with a bogus clock.
    #[test]
    fn empty_pool_error_shows_malformed_entry_as_permanent() {
        let tmp = make_pool(&["a"]);
        fs::write(pool_dir(tmp.path()).join(".bad_tokens"), "garbage a exhausted\n").unwrap();
        let mut rng = Rng::seeded(1);
        let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
        assert!(
            err.0
                .contains("a: bad-marked [malformed-timestamp, permanent (fail-closed)]"),
            "{}",
            err.0
        );
    }

    #[test]
    fn deciding_binary_identity_names_version_and_commit() {
        let id = deciding_binary_identity();
        assert!(id.starts_with("loom-daemon "), "{id}");
        assert!(id.contains(env!("CARGO_PKG_VERSION")), "{id}");
        assert!(id.contains(crate::self_update::BUILT_COMMIT), "{id}");
    }

    #[test]
    #[serial]
    fn spread_top_n_env_caps_ranked_window() {
        let tmp = make_pool(&["a", "b", "c"]);
        fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available\nb|available\nc|available\n")
            .unwrap();
        std::env::set_var("LOOM_TOKEN_SPREAD_TOP_N", "1");
        // Pre-seed rotation cursor so the outcome is deterministic.
        fs::write(pool_dir(tmp.path()).join(".rotation_cursor"), "0").unwrap();
        let mut rng = Rng::seeded(1);
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        std::env::remove_var("LOOM_TOKEN_SPREAD_TOP_N");
        // N=1 == greedy first-eligible: always "a".
        assert_eq!(sel.name, "a");
    }

    // ---- config_resolver migration (#4241) — tier precedence ---------

    fn write_legacy_config(root: &Path, contents: &str) {
        let dir = root.join(".loom");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.json"), contents).unwrap();
    }

    fn write_project_config(root: &Path, contents: &str) {
        let full = root.join(crate::config_resolver::PROJECT_CONFIG_REL);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, contents).unwrap();
    }

    #[test]
    #[serial(loom_config_env)]
    fn read_config_spread_top_n_legacy_tier_only() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_legacy_config(tmp.path(), r#"{"tokens": {"spreadTopN": 3}}"#);
        let n = read_config_spread_top_n(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(n, Some(3));
    }

    #[test]
    #[serial(loom_config_env)]
    fn read_config_spread_top_n_project_tier_only_is_honored() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_project_config(tmp.path(), r#"{"tokens": {"spreadTopN": 5}}"#);
        let n = read_config_spread_top_n(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(n, Some(5));
    }

    #[test]
    #[serial(loom_config_env)]
    fn read_config_spread_top_n_project_tier_overrides_legacy() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_legacy_config(tmp.path(), r#"{"tokens": {"spreadTopN": 2}}"#);
        write_project_config(tmp.path(), r#"{"tokens": {"spreadTopN": 7}}"#);
        let n = read_config_spread_top_n(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(n, Some(7));
    }

    #[test]
    #[serial(loom_config_env)]
    fn read_config_spread_top_n_missing_everywhere_is_none() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        let n = read_config_spread_top_n(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(n, None);
    }

    #[test]
    fn read_token_file_strips_whitespace() {
        let tmp = make_pool(&["a"]);
        fs::write(pool_dir(tmp.path()).join("a.token"), "  sk-ant\noat01\t-xyz  \n").unwrap();
        let key = read_token_file(&pool_dir(tmp.path()).join("a.token")).unwrap();
        assert_eq!(key, "sk-antoat01-xyz");
    }

    // ---- 5h load gate (issue #4195) ----------------------------------

    #[test]
    fn read_ranking_parses_optional_util_field() {
        let tmp = tempfile::tempdir().unwrap();
        let ranking = tmp.path().join(".ranking");
        // 3-field, legacy 2-field, and a malformed util (-> None, "unknown").
        fs::write(&ranking, "a|available|0.70\nb|available\nc|available|bad\n").unwrap();
        let rows = read_ranking(&ranking);
        assert_eq!(rows[0], ("a".to_string(), "available".to_string(), Some(0.70)));
        assert_eq!(rows[1], ("b".to_string(), "available".to_string(), None));
        assert_eq!(rows[2], ("c".to_string(), "available".to_string(), None));
    }

    // ---- limit-reset field (issue #4874) -----------------------------

    #[test]
    fn parse_ranking_line_reads_optional_limit_reset_field() {
        // A 4-field row surfaces the reset verbatim; the 2- and 3-field legacy
        // layouts still parse with `limit_reset = None` (backward compatible).
        let full = parse_ranking_line("a|exhausted|0.00|2026-08-02T03:00:00Z").unwrap();
        assert_eq!(full.name, "a");
        assert_eq!(full.status, "exhausted");
        assert_eq!(full.util_5h, Some(0.00));
        assert_eq!(full.limit_reset.as_deref(), Some("2026-08-02T03:00:00Z"));

        assert_eq!(parse_ranking_line("b|available|0.70").unwrap().limit_reset, None);
        assert_eq!(parse_ranking_line("c|available").unwrap().limit_reset, None);
    }

    #[test]
    fn parse_ranking_line_reset_without_util_does_not_fabricate_zero() {
        // `name|status||reset` — the reset-known/util-unknown layout. The empty
        // third field must parse back to `None`, never to `0.0`, or a fully
        // idle account would look like a measured-zero-load one.
        let row = parse_ranking_line("a|exhausted||2026-08-04T11:00:00Z").unwrap();
        assert_eq!(row.status, "exhausted");
        assert_eq!(row.util_5h, None);
        assert_eq!(row.limit_reset.as_deref(), Some("2026-08-04T11:00:00Z"));
    }

    #[test]
    fn parse_ranking_line_4th_field_does_not_swallow_the_util() {
        // Regression guard for the `splitn(3, ..)` hazard the curator flagged:
        // with a 3-way split the 4th segment is swallowed into the utilization
        // field, so `0.00|2026-...` fails to parse as a float and the
        // utilization silently becomes `None`.
        let row = parse_ranking_line("a|exhausted|0.42|2026-08-02T03:00:00Z").unwrap();
        assert_eq!(row.util_5h, Some(0.42), "the 4th field must not swallow the util");
    }

    #[test]
    fn parse_ranking_line_reset_is_comment_stripped_and_trimmed() {
        // `#` comments are stripped before splitting, and surrounding
        // whitespace is trimmed off the reset like every other field.
        let row = parse_ranking_line("a|exhausted|0.00| 2026-08-02T03:00:00Z  # probed").unwrap();
        assert_eq!(row.limit_reset.as_deref(), Some("2026-08-02T03:00:00Z"));
        // An empty 4th field is "unknown", not an empty string.
        assert_eq!(parse_ranking_line("a|exhausted|0.00|").unwrap().limit_reset, None);
    }

    #[test]
    fn read_ranking_ignores_the_reset_field_for_selection() {
        // Selection consumes the projected triple: a 4-field row must behave
        // exactly like the same row without a reset, so adding the field
        // cannot perturb which account gets picked.
        let tmp = tempfile::tempdir().unwrap();
        let ranking = tmp.path().join(".ranking");
        fs::write(&ranking, "a|available|0.70|2026-08-02T03:00:00Z\nb|available|0.70\n").unwrap();
        let rows = read_ranking(&ranking);
        assert_eq!(rows[0], ("a".to_string(), "available".to_string(), Some(0.70)));
        assert_eq!(rows[1], ("b".to_string(), "available".to_string(), Some(0.70)));
    }

    #[test]
    #[serial]
    fn resolve_load_gate_env_and_default() {
        std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
        assert_eq!(resolve_load_gate(), DEFAULT_5H_LOAD_GATE);
        std::env::set_var("LOOM_TOKEN_5H_LOAD_GATE", "0.5");
        assert_eq!(resolve_load_gate(), 0.5);
        // Unparseable -> default.
        std::env::set_var("LOOM_TOKEN_5H_LOAD_GATE", "garbage");
        assert_eq!(resolve_load_gate(), DEFAULT_5H_LOAD_GATE);
        std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
    }

    #[test]
    #[serial]
    fn load_gate_excludes_loaded_account_in_preferred_pass() {
        std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
        std::env::remove_var("LOOM_TOKEN_SPREAD_TOP_N");
        let tmp = make_pool(&["a", "b"]);
        // `a` healthy but 90% 5h-loaded (>= 0.70 default gate); `b` light.
        fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available|0.90\nb|available|0.10\n")
            .unwrap();
        let mut rng = Rng::seeded(1);
        for _ in 0..10 {
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            assert_eq!(sel.name, "b");
            assert_eq!(sel.mode, "ranked");
        }
    }

    #[test]
    #[serial]
    fn load_gate_readmits_in_fallback_when_all_loaded() {
        std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
        std::env::remove_var("LOOM_TOKEN_SPREAD_TOP_N");
        let tmp = make_pool(&["a", "b"]);
        // Both over the gate -> fallback pass drops the gate; pool never
        // hard-fails on load alone.
        fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available|0.95\nb|available|0.90\n")
            .unwrap();
        let mut rng = Rng::seeded(1);
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert!(sel.name == "a" || sel.name == "b");
        assert_eq!(sel.mode, "ranked");
    }

    #[test]
    #[serial]
    fn load_gate_unknown_util_is_never_gated() {
        std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
        std::env::remove_var("LOOM_TOKEN_SPREAD_TOP_N");
        let tmp = make_pool(&["a", "b", "c"]);
        // a: legacy 2-field (unknown); b: malformed util (unknown); c: loaded.
        fs::write(
            pool_dir(tmp.path()).join(".ranking"),
            "a|available\nb|available|not-a-number\nc|available|0.99\n",
        )
        .unwrap();
        let mut chosen = HashSet::new();
        for _ in 0..10 {
            let mut rng = Rng::seeded(1);
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            assert_eq!(sel.mode, "ranked");
            chosen.insert(sel.name);
        }
        // a and b (unknown) rotate; c (0.99 loaded) is excluded.
        assert!(chosen.contains("a") && chosen.contains("b"));
        assert!(!chosen.contains("c"));
    }

    #[test]
    #[serial]
    fn load_gate_env_override_lowers_threshold() {
        std::env::remove_var("LOOM_TOKEN_SPREAD_TOP_N");
        let tmp = make_pool(&["a", "b"]);
        fs::write(pool_dir(tmp.path()).join(".ranking"), "a|available|0.50\nb|available|0.10\n")
            .unwrap();
        std::env::set_var("LOOM_TOKEN_5H_LOAD_GATE", "0.40");
        let mut rng = Rng::seeded(1);
        for _ in 0..10 {
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            assert_eq!(sel.name, "b"); // `a` now over the lowered gate.
        }
        std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
    }

    #[test]
    #[serial]
    fn load_gate_backward_compatible_2field_ranking() {
        std::env::remove_var("LOOM_TOKEN_5H_LOAD_GATE");
        std::env::remove_var("LOOM_TOKEN_SPREAD_TOP_N");
        let tmp = make_pool(&["a", "b"]);
        // Pure legacy 2-field file still parses + selects unchanged.
        fs::write(pool_dir(tmp.path()).join(".ranking"), "a|exhausted\nb|available\n").unwrap();
        let mut rng = Rng::seeded(1);
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "b");
        assert_eq!(sel.mode, "ranked");
    }

    // ---- provider filtering + upstream_id (issue #5609, design D8/D9) ----

    fn write_index_json(pool: &Path, body: &str) {
        fs::write(pool.join("index.json"), body).unwrap();
    }

    /// A pool containing a non-claude manifest row is never selected from by
    /// the Claude selector (random tier) — the row is skipped regardless of
    /// its `.token` file being present on disk (defense-in-depth, D4/D8) —
    /// and the eligible Claude row's `upstream_id` is carried through.
    #[test]
    fn non_claude_manifest_row_is_never_selected_random_tier() {
        let tmp = make_pool(&["good", "sneaky"]);
        write_index_json(
            &pool_dir(tmp.path()),
            r#"{"version":3,"accounts":[
                {"name":"sneaky","provider":"codex","upstream_id":"monitor:99","email":"s@x.com","file":"sneaky.token","source":"monitor-db","materialized":true},
                {"name":"good","provider":"claude","upstream_id":"monitor:1","email":"g@x.com","file":"good.token","source":"monitor-db","materialized":true}
            ]}"#,
        );
        let mut rng = Rng::seeded(1);
        for _ in 0..10 {
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            assert_eq!(sel.name, "good");
            assert_eq!(sel.mode, "random");
            assert_eq!(sel.upstream_id.as_deref(), Some("monitor:1"));
        }
    }

    /// Same exclusion applies to the ranked tier — a `.ranking` row marking a
    /// non-claude account `available` must not be selected.
    #[test]
    fn non_claude_manifest_row_is_never_selected_ranked_tier() {
        let tmp = make_pool(&["good", "sneaky"]);
        write_index_json(
            &pool_dir(tmp.path()),
            r#"{"version":3,"accounts":[
                {"name":"sneaky","provider":"codex","upstream_id":"monitor:99","email":"s@x.com","file":"sneaky.token","source":"monitor-db","materialized":true},
                {"name":"good","provider":"claude","upstream_id":"monitor:1","email":"g@x.com","file":"good.token","source":"monitor-db","materialized":true}
            ]}"#,
        );
        fs::write(pool_dir(tmp.path()).join(".ranking"), "sneaky|available\ngood|available\n")
            .unwrap();
        let mut rng = Rng::seeded(1);
        for _ in 0..10 {
            let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
            assert_eq!(sel.name, "good");
            assert_eq!(sel.mode, "ranked");
        }
    }

    /// A pool whose only account is a non-claude manifest row fails closed
    /// (not silently, and never falls back to picking it): the empty-pool
    /// error names the provider mismatch.
    #[test]
    fn non_claude_only_pool_errors_with_provider_detail() {
        let tmp = make_pool(&["sneaky"]);
        write_index_json(
            &pool_dir(tmp.path()),
            r#"{"version":3,"accounts":[
                {"name":"sneaky","provider":"codex","upstream_id":"monitor:99","email":"s@x.com","file":"sneaky.token","source":"monitor-db","materialized":true}
            ]}"#,
        );
        let mut rng = Rng::seeded(1);
        let err = select_token(tmp.path(), Some(&mut rng)).unwrap_err();
        assert!(err.0.contains("sneaky: hard-excluded"), "{}", err.0);
        assert!(err.0.contains("codex"), "{}", err.0);
    }

    /// A pool with no `index.json` at all still selects normally — the
    /// fail-open path (D6/D8): absence of a manifest row is never treated as
    /// "not claude".
    #[test]
    fn no_manifest_file_is_fail_open() {
        let tmp = make_pool(&["only"]);
        assert!(!pool_dir(tmp.path()).join("index.json").is_file());
        let mut rng = Rng::seeded(1);
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "only");
        assert_eq!(sel.upstream_id, None);
    }

    /// A manifest row present for the selected name but with no
    /// `upstream_id` field (e.g. a hand-edited or older row) yields `None`,
    /// never a fabricated identity.
    #[test]
    fn manifest_row_without_upstream_id_yields_none() {
        let tmp = make_pool(&["only"]);
        write_index_json(
            &pool_dir(tmp.path()),
            r#"{"version":3,"accounts":[
                {"name":"only","provider":"claude","email":"o@x.com","file":"only.token","source":"env","materialized":true}
            ]}"#,
        );
        let mut rng = Rng::seeded(1);
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.upstream_id, None);
    }
}
