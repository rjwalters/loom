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

use super::bad_tokens::is_bad;
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

/// Yield `(name, status, util_5h)` triples from the ranking file. Format:
/// `name|status|5h_util` per line (issue #4195); the third field is optional,
/// so a legacy `name|status` line yields `util_5h = None` (backward
/// compatible). Malformed/empty lines are skipped; `status` defaults to `""`.
fn read_ranking(ranking_file: &Path) -> Vec<(String, String, Option<f64>)> {
    let Ok(text) = std::fs::read_to_string(ranking_file) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for raw in text.lines() {
        let stripped = strip_comment(raw);
        if stripped.is_empty() {
            continue;
        }
        let mut parts = stripped.splitn(3, '|');
        let name = parts.next().unwrap_or("").trim().to_string();
        let status = parts.next().unwrap_or("").trim().to_string();
        let util = parts.next().and_then(parse_util);
        if !name.is_empty() {
            out.push((name, status, util));
        }
    }
    out
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
            "Token directory does not exist: {}{}. Run `loom-tokens bootstrap` to populate it \
             (or `loom-tokens bootstrap --shared` for the machine-level pool).",
            tokens_dir.display(),
            shared_pool_hint()
        )));
    }

    let all_tokens = list_token_files(&tokens_dir);
    if all_tokens.is_empty() {
        return Err(EmptyTokenPoolError(format!(
            "No .token files in {}{}. Run `loom-tokens bootstrap` \
             (or `loom-tokens bootstrap --shared` for the machine-level pool).",
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

    Err(EmptyTokenPoolError(format!(
        "All {} tokens in {} are marked bad or empty. Inspect .bad_tokens or run \
         `loom-tokens bootstrap --force`.",
        all_tokens.len(),
        tokens_dir.display()
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
