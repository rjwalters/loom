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
//! skipped. A *stale* `.ranking` (present but older than the freshness
//! window) declines tier-1 but still contributes an advisory exclusion set
//! to tiers 2/3 (issue #3894) — with a fail-safe retry ignoring the
//! exclusions if they would empty the pool.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::bad_tokens::{
    blocking_entry, exhaustion_cooldown_secs, is_bad, EXHAUSTION_COOLDOWN_ENV,
};
use super::paths::{resolve_tokens_dir, shared_tokens_dir};
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

/// Statuses hard-excluded from tier-1 in *every* pass, even the
/// empty-pool fallback pass.
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
/// "bad-marked", "empty", and "unreadable", all of which are re-derivable
/// per token without disturbing the selection hot path.
fn describe_exclusion(workspace: &Path, token_file: &Path) -> String {
    let name = stem(token_file);
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
) -> Vec<SelectedToken> {
    let mut out = Vec::new();
    for (name, status, util) in read_ranking(ranking_file) {
        if is_hard_excluded_status(&status) {
            continue;
        }
        if healthy_only && !is_healthy_status(&status) {
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
        out.push(SelectedToken {
            name,
            file: token_file,
            key,
            mode: "ranked",
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
) -> Option<SelectedToken> {
    let age = file_age_seconds(ranking_file)?;
    if age >= RANKING_FRESH_SECONDS {
        return None;
    }

    let cap = resolve_spread_top_n(workspace);
    let load_gate = resolve_load_gate();

    let mut eligible =
        collect_ranked_candidates(tokens_dir, ranking_file, workspace, cap, true, load_gate);
    if eligible.is_empty() {
        eligible =
            collect_ranked_candidates(tokens_dir, ranking_file, workspace, cap, false, load_gate);
    }
    if eligible.is_empty() {
        return None;
    }
    let index = next_rotation_index(tokens_dir, eligible.len(), rng);
    Some(eligible.swap_remove(index))
}

/// Advisory exclusion set sourced from a *stale* `.ranking` (issue #3894).
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

    if let Some(selected) = try_ranking(&tokens_dir, &ranking_file, workspace, rng) {
        return Ok(selected);
    }

    let exclude = stale_ranking_exclusions(&ranking_file);

    if let Some(selected) = try_allowlist(&tokens_dir, &allowlist_file, workspace, rng, &exclude) {
        return Ok(selected);
    }
    if let Some(selected) = try_random(&tokens_dir, workspace, rng, &exclude) {
        return Ok(selected);
    }

    // Fail-safe: the advisory exclusions emptied the pool. Retry ignoring
    // them so a live pool never hard-fails on stale advice.
    if !exclude.is_empty() {
        let empty: HashSet<String> = HashSet::new();
        if let Some(selected) = try_allowlist(&tokens_dir, &allowlist_file, workspace, rng, &empty)
        {
            return Ok(selected);
        }
        if let Some(selected) = try_random(&tokens_dir, workspace, rng, &empty) {
            return Ok(selected);
        }
    }

    // Per-token exclusion detail (#4643): say WHY each account was excluded and
    // WHICH binary decided, so a recurrence is diagnosable from the sweep log
    // alone instead of by reading this source file.
    let detail: String = all_tokens
        .iter()
        .map(|f| format!("\n  - {}", describe_exclusion(workspace, f)))
        .collect();
    Err(EmptyTokenPoolError(format!(
        "All {} tokens in {} are marked bad or empty.{detail}\n  \
         deciding binary: {}\n  \
         exhaustion cooldown: {}s (override {EXHAUSTION_COOLDOWN_ENV}); auth entries never \
         expire — clear them with `loom-daemon tokens unblock <name>` \
         (add --all-reasons to drop non-auth entries too).\n  \
         Inspect .bad_tokens or run `loom-daemon tokens bootstrap --force`.",
        all_tokens.len(),
        tokens_dir.display(),
        deciding_binary_identity(),
        exhaustion_cooldown_secs(),
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
        // Both are bad-status in ranking (though not in .bad_tokens), so
        // tier-1 yields nothing; falls through to tier-3 (random) which does
        // not consult ranking status at all.
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert!(sel.name == "a" || sel.name == "b");
        assert_eq!(sel.mode, "random");
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
        fs::write(&ranking, "a|exhausted\n").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(700);
        let f = fs::File::open(&ranking).unwrap();
        f.set_modified(old).unwrap();

        let mut rng = Rng::seeded(1);
        // Excluding "a" would empty the pool -> fail-safe retry ignoring
        // exclusions must still return "a".
        let sel = select_token(tmp.path(), Some(&mut rng)).unwrap();
        assert_eq!(sel.name, "a");
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
}
