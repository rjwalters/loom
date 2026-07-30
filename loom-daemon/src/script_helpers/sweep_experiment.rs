//! Model-cost experiment instrumentation for `/loom:sweep` (issue #3725) — the
//! native port of `loom_tools.sweep_experiment` (#4275), behind
//! `sweep-experiment.sh`.
//!
//! The sweep skill is LLM-executed markdown, so the load-bearing arithmetic
//! (tri-state resolution, per-issue arm assignment, the durable JSONL append,
//! the harvest) lives here as a small CLI the skill shells out to — the LLM
//! never computes a modulo by hand.
//!
//! Surface (all subcommands print to stdout; warnings go to stderr):
//!
//! ```text
//! resolve-mode                        -> effective mode after the canary guardrail
//! assign-arm --issue N [--complexity] -> deterministic arm + forced model
//! banner --issue N [--complexity]     -> loud startup banner naming mode + arm
//! record ...                          -> append one JSONL outcome-chain record
//! harvest [--archive-dir DIR]         -> per-arm inequality inputs for #3718
//! ```
//!
//! # Arm A must resolve identically to `resolve-model.sh` (#4060 contract)
//!
//! [`ARM_MODEL`] stays **logical aliases** (Arm A = `opus`) so the arm identity
//! is stable for reporting. [`resolved_arm_model`] turns that alias into the
//! wire ID through [`super::model_tiers::resolve_model`] — the same single
//! implementation `resolve-model.sh` uses — so the experiment's Arm A and the
//! sweep's escalation ladder can never disagree about what `opus` means.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

pub const VALID_MODES: [&str; 3] = ["off", "observe", "experiment"];
const TRUTHY: [&str; 4] = ["1", "true", "yes", "on"];

/// Gitignored, repo-local sentinel that confirms a canary WITHOUT travelling in
/// a committed config (issue #3731). Its confirmation power comes precisely
/// from being uncommitted — a git-tracked copy is refused.
pub const CANARY_SENTINEL: &str = ".loom/CANARY";

/// Arm → forced Builder model. Arm A is opus-first, Arm B is sonnet-first.
pub const ARM_MODEL: [(&str, &str); 2] = [("A", "opus"), ("B", "sonnet")];

pub const DEFAULT_STATS_FILE: &str = ".loom/stats/sweep-model-stats.jsonl";

// --------------------------------------------------------------------------
// Tri-state mode resolution + canary guardrail
// --------------------------------------------------------------------------

/// Resolve the tri-state mode env-over-config, BEFORE the canary guardrail.
///
/// Returns `(mode, warnings)`. An unknown/malformed value → `off` + a warning.
/// Follows the *string-valued* guard precedence (`guards.rmScope` /
/// `guards.forceScope`), never the boolean pattern.
#[must_use]
pub fn resolve_raw_mode(env_mode: Option<&str>, config: &Value) -> (String, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();

    if let Some(raw) = env_mode.filter(|v| !v.trim().is_empty()) {
        let val = raw.trim().to_lowercase();
        if VALID_MODES.contains(&val.as_str()) {
            return (val, warnings);
        }
        warnings.push(format!(
            "LOOM_MODEL_EXPERIMENT={raw:?} is not one of {VALID_MODES:?}; treating as 'off'"
        ));
        return ("off".to_string(), warnings);
    }

    if let Some(v) = crate::config_resolver::get_path(config, "sweep.modelExperiment") {
        if let Some(s) = v.as_str() {
            let norm = s.trim().to_lowercase();
            if VALID_MODES.contains(&norm.as_str()) {
                return (norm, warnings);
            }
        }
        if !v.is_null() {
            warnings.push(format!(
                "sweep.modelExperiment={v} is not one of {VALID_MODES:?}; treating as 'off'"
            ));
        }
    }

    ("off".to_string(), warnings)
}

/// Best-effort: is `path` tracked by git in its repo? Any error → `false`.
fn sentinel_is_tracked(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let mut cmd = std::process::Command::new("git");
    cmd.args(["ls-files", "--error-unmatch", "--"]).arg(name);
    if let Some(d) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        cmd.current_dir(d);
    }
    cmd.output().is_ok_and(|o| o.status.success())
}

/// How the canary was confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanarySource {
    Env,
    Sentinel,
}

impl CanarySource {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Sentinel => "sentinel",
        }
    }
}

/// Evaluate canary confirmation from an UNCOMMITTED signal only (#3731).
///
/// Confirmation is accepted ONLY from a signal that cannot travel with a
/// copied, committed `.loom/config.json`:
///
/// * the `LOOM_MODEL_EXPERIMENT_CANARY` env var (truthy), or
/// * a **gitignored** sentinel file ([`CANARY_SENTINEL`]) present on disk.
///
/// A git-TRACKED sentinel is refused (with a warning) — a committed sentinel is
/// just the config-propagation vector by another name. The retired committed
/// `sweep.modelExperimentCanary` flag is inert.
///
/// `tracked_probe` is injectable so tests do not need a git repo.
pub fn evaluate_canary(
    env_canary: Option<&str>,
    sentinel_path: Option<&Path>,
    tracked_probe: &dyn Fn(&Path) -> bool,
) -> (bool, Option<CanarySource>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();

    if let Some(raw) = env_canary {
        let v = raw.trim().to_lowercase();
        if !v.is_empty() && TRUTHY.contains(&v.as_str()) {
            return (true, Some(CanarySource::Env), warnings);
        }
    }

    let path: PathBuf =
        sentinel_path.map_or_else(|| PathBuf::from(CANARY_SENTINEL), Path::to_path_buf);
    if path.exists() {
        if tracked_probe(&path) {
            warnings.push(format!(
                "canary sentinel {} is TRACKED by git — refusing it as a confirmation \
                 source. A committed sentinel defeats the uncommitted-signal guardrail \
                 (#3731); gitignore it and run `git rm --cached {}`.",
                path.display(),
                path.display()
            ));
        } else {
            return (true, Some(CanarySource::Sentinel), warnings);
        }
    }

    (false, None, warnings)
}

/// [`evaluate_canary`] with the real git probe.
pub fn evaluate_canary_default(
    env_canary: Option<&str>,
    sentinel_path: Option<&Path>,
) -> (bool, Option<CanarySource>, Vec<String>) {
    evaluate_canary(env_canary, sentinel_path, &sentinel_is_tracked)
}

/// Resolve mode AND apply the canary guardrail.
///
/// `experiment` is behavior-changing (it forces Builder models and suppresses
/// the complexity bump), so it is honored only on a confirmed canary. On any
/// other target it is loudly downgraded to `observe` (safe anywhere) rather
/// than refused outright — the measurement still accrues, just without the
/// model-forcing behavior change.
pub fn resolve_effective_mode(
    env_mode: Option<&str>,
    env_canary: Option<&str>,
    config: &Value,
    sentinel_path: Option<&Path>,
    tracked_probe: &dyn Fn(&Path) -> bool,
) -> (String, Vec<String>) {
    let (mut mode, mut warnings) = resolve_raw_mode(env_mode, config);
    if mode == "experiment" {
        let (confirmed, _source, canary_warnings) =
            evaluate_canary(env_canary, sentinel_path, tracked_probe);
        warnings.extend(canary_warnings);
        if !confirmed {
            warnings.push(
                "experiment mode requested on a NON-CANARY target — downgrading to \
                 'observe' (no model forcing). Confirm a canary via an UNCOMMITTED \
                 signal: export LOOM_MODEL_EXPERIMENT_CANARY=1 or create the \
                 gitignored sentinel .loom/CANARY. (Committed \
                 sweep.modelExperimentCanary is no longer accepted — #3731.)"
                    .to_string(),
            );
            mode = "observe".to_string();
        }
    }
    (mode, warnings)
}

/// [`resolve_effective_mode`] with the real git tracked-sentinel probe — the
/// form the CLI uses.
pub fn resolve_effective_mode_default(
    env_mode: Option<&str>,
    env_canary: Option<&str>,
    config: &Value,
    sentinel_path: Option<&Path>,
) -> (String, Vec<String>) {
    resolve_effective_mode(env_mode, env_canary, config, sentinel_path, &sentinel_is_tracked)
}

// --------------------------------------------------------------------------
// Deterministic, resume-safe, stratified arm assignment
// --------------------------------------------------------------------------

/// Normalize the #3702 marker to `complex` | `routine` (default `routine`).
#[must_use]
pub fn normalize_complexity(complexity: Option<&str>) -> &'static str {
    if complexity
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("complex")
    {
        "complex"
    } else {
        "routine"
    }
}

/// Deterministically assign `A` or `B` for an issue.
///
/// A pure function of `issue_number` and the complexity stratum, so a resumed
/// sweep re-running the same issue lands on the same arm. The complexity bit
/// offsets the parity split so the `complex` and `routine` strata each get an
/// independent ~50/50 A/B balance rather than correlating.
#[must_use]
pub fn assign_arm(issue_number: i64, complexity: Option<&str>) -> &'static str {
    let bit = i64::from(normalize_complexity(complexity) == "complex");
    if (issue_number + bit).rem_euclid(2) == 0 {
        "A"
    } else {
        "B"
    }
}

#[must_use]
pub fn arm_model(arm: &str) -> String {
    let key = arm.trim().to_uppercase();
    ARM_MODEL
        .iter()
        .find(|(a, _)| *a == key)
        .map_or_else(String::new, |(_, m)| (*m).to_string())
}

/// Resolve an arm's logical model alias to the concrete model ID to dispatch.
///
/// Routes through [`super::model_tiers::resolve_model`] — the SAME resolver
/// `resolve-model.sh` uses — so Arm A reaches Opus 5 rather than the CLI's
/// stale gen-4 `opus` alias, and the two surfaces cannot diverge (#4060).
#[must_use]
pub fn resolved_arm_model(arm: &str, config: &Value) -> String {
    let alias = arm_model(arm);
    if alias.is_empty() {
        String::new()
    } else {
        super::model_tiers::resolve_model(&alias, config)
    }
}

/// Normalize a model alias or pinned ID to its family (mirrors
/// [`model_pricing`]). Matching is generation-agnostic — it keys off the family
/// stem so a future `claude-sonnet-6` classifies correctly with no code change
/// (#3981). Legacy `claude-3-5-sonnet` / `claude-3-opus` / `claude-3-haiku` IDs
/// predate the `-<generation>-` scheme and are matched explicitly.
#[must_use]
pub fn model_family(model: Option<&str>) -> Option<&'static str> {
    let m = model.unwrap_or("").to_lowercase();
    if m == "sonnet" || m.contains("claude-3-5-sonnet") || m.contains("claude-sonnet-") {
        return Some("sonnet");
    }
    if m == "opus" || m.contains("claude-3-opus") || m.contains("claude-opus-") {
        return Some("opus");
    }
    if m == "haiku" || m.contains("claude-3-haiku") || m.contains("claude-haiku-") {
        return Some("haiku");
    }
    if m == "fable" || m.contains("claude-fable-") {
        return Some("fable");
    }
    None
}

/// Map an observed Builder model to its inequality arm (opus→A, sonnet→B).
///
/// `observe` mode records the outcome chain WITHOUT forcing a model, so its
/// records carry `arm=null`. Without this inference every observe-mode record
/// would collapse into the single `"?"` bucket and a passive multi-day observe
/// sample could never populate the opus/sonnet split #3718 needs. `None` for
/// any model that is not one of the two arms, so such records stay in `"?"`.
#[must_use]
pub fn infer_arm_from_model(model: Option<&str>) -> Option<&'static str> {
    match model_family(model) {
        Some("opus") => Some("A"),
        Some("sonnet") => Some("B"),
        _ => None,
    }
}

// --------------------------------------------------------------------------
// Durable stats store (atomic O_APPEND, one JSONL line per phase invocation)
// --------------------------------------------------------------------------

#[must_use]
pub fn stats_path(stats_file: Option<&str>) -> PathBuf {
    PathBuf::from(stats_file.unwrap_or(DEFAULT_STATS_FILE))
}

/// Append one JSONL record atomically.
///
/// POSIX `O_APPEND` guarantees per-line atomicity for concurrent detached
/// writers, so no lock is needed for single-line writes. The stats file is
/// created `0600` and its parent directory `0700` (best-effort) — it can carry
/// issue context.
pub fn append_record(record: &Value, stats_file: Option<&str>) -> std::io::Result<()> {
    let path = stats_path(stats_file);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(parent);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let line = format!("{}\n", serde_json::to_string(record)?);

    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path)?;
    f.write_all(line.as_bytes())
}

/// The fields of one outcome-chain record. The HARD deterministic fields
/// (arm/model/attempt/verdict/cycle_count/complexity) are the load-bearing
/// evidence for #3718; `agent_id` is the join key into #3726's transcript index
/// for exact-cost harvest.
#[derive(Debug, Default, Clone)]
pub struct RecordFields<'a> {
    pub issue: i64,
    pub phase: &'a str,
    pub role: &'a str,
    pub model: Option<&'a str>,
    pub mode: &'a str,
    pub arm: Option<&'a str>,
    pub attempt: i64,
    pub complexity: Option<&'a str>,
    pub judge_verdict: Option<&'a str>,
    pub cycle_count: i64,
    pub pr: Option<i64>,
    pub effort: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub transcript: Option<&'a str>,
    pub in_tok: Option<i64>,
    pub out_tok: Option<i64>,
    pub token_fidelity: &'a str,
}

/// Assemble one outcome-chain record. Key order matches the Python original —
/// the JSONL store is read back by `harvest` and eyeballed in logs.
#[must_use]
pub fn build_record(f: &RecordFields<'_>, ts: &str) -> Value {
    let opt_str = |v: Option<&str>| v.map_or(Value::Null, |s| json!(s));
    let opt_i64 = |v: Option<i64>| v.map_or(Value::Null, |n| json!(n));

    let mut m = Map::new();
    m.insert("ts".into(), json!(ts));
    m.insert("issue".into(), json!(f.issue));
    m.insert("pr".into(), opt_i64(f.pr));
    m.insert("mode".into(), json!(f.mode));
    m.insert("phase".into(), json!(f.phase));
    m.insert("role".into(), json!(f.role));
    m.insert("model".into(), opt_str(f.model));
    m.insert("effort".into(), opt_str(f.effort));
    m.insert(
        "arm".into(),
        f.arm
            .filter(|a| !a.is_empty())
            .map_or(Value::Null, |a| json!(a.to_uppercase())),
    );
    m.insert("attempt".into(), json!(f.attempt));
    m.insert("complexity".into(), json!(normalize_complexity(f.complexity)));
    m.insert("judge_verdict".into(), opt_str(f.judge_verdict));
    m.insert("cycle_count".into(), json!(f.cycle_count));
    m.insert("agent_id".into(), opt_str(f.agent_id));
    m.insert("transcript".into(), opt_str(f.transcript));
    m.insert("in_tok".into(), opt_i64(f.in_tok));
    m.insert("out_tok".into(), opt_i64(f.out_tok));
    m.insert("token_fidelity".into(), json!(f.token_fidelity));
    Value::Object(m)
}

// --------------------------------------------------------------------------
// Cache-aware pricing
// --------------------------------------------------------------------------

/// `(input, output, cache_read, cache_write)` US$ per 1k tokens.
///
/// Delegates to [`crate::activity::resource_usage::ModelPricing`] so this port
/// does not recreate the Python/Rust mirror pair it was meant to collapse:
/// there is now exactly ONE pricing table in the tree. `claude-fable-*` has no
/// published per-token rate (it is an escalation-ladder rung above Opus,
/// #3702), so it is conservatively priced at the Opus rate rather than falling
/// through to the cheaper Sonnet default and under-reporting cost.
#[must_use]
pub fn model_pricing(model: Option<&str>) -> (f64, f64, f64, f64) {
    // The Python original also matched the bare aliases `opus`/`fable`/`haiku`
    // (its input can be an unresolved arm alias). Normalize through the family
    // classifier first so the shared Rust table — which keys off pinned-ID
    // stems — sees a value it recognizes.
    let normalized = match model_family(model) {
        Some("sonnet") => "claude-sonnet-",
        Some("opus" | "fable") => "claude-opus-",
        Some("haiku") => "claude-haiku-",
        _ => "",
    };
    let p = crate::activity::resource_usage::ModelPricing::for_model(normalized);
    (
        p.input_cost_per_1k,
        p.output_cost_per_1k,
        p.cache_read_cost_per_1k,
        p.cache_write_cost_per_1k,
    )
}

#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn calc_cost(
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    model: Option<&str>,
) -> f64 {
    let (inp, out, cr, cw) = model_pricing(model);
    input_tokens as f64 / 1000.0 * inp
        + output_tokens as f64 / 1000.0 * out
        + cache_read_tokens as f64 / 1000.0 * cr
        + cache_write_tokens as f64 / 1000.0 * cw
}

/// Totals from summing every `usage` block in a subagent transcript.
#[derive(Debug, Default, Clone)]
pub struct TranscriptUsage {
    pub model: Option<String>,
    pub usage_blocks: usize,
    pub cost_usd: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
}

/// Sum every `usage` block in a subagent transcript.
///
/// Best-effort: unreadable lines are skipped, a missing file yields zeros.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn sum_transcript_usage(path: &Path) -> TranscriptUsage {
    let mut out = TranscriptUsage::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for raw in text.lines() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let container = obj.get("message").filter(|m| m.is_object()).unwrap_or(&obj);
        if !container.is_object() {
            continue;
        }
        if out.model.is_none() {
            if let Some(mv) = container
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                out.model = Some(mv.to_string());
            }
        }
        let Some(usage) = container.get("usage").and_then(Value::as_object) else {
            continue;
        };
        out.usage_blocks += 1;
        let get = |k: &str| usage.get(k).and_then(Value::as_f64).unwrap_or(0.0) as i64;
        out.input_tokens += get("input_tokens");
        out.output_tokens += get("output_tokens");
        out.cache_read_input_tokens += get("cache_read_input_tokens");
        out.cache_creation_input_tokens += get("cache_creation_input_tokens");
    }
    out.cost_usd = calc_cost(
        out.input_tokens,
        out.output_tokens,
        out.cache_read_input_tokens,
        out.cache_creation_input_tokens,
        out.model.as_deref(),
    );
    out
}

// --------------------------------------------------------------------------
// Transcript index join (consumes #3726's loom.transcript-index/v1)
// --------------------------------------------------------------------------

/// Map `agent-<id>` → absolute transcript path from #3726 archive indexes.
///
/// Walks every `index.json` (schema `loom.transcript-index/v1`) under
/// `archive_dir`. The index lives at `<...>/<uuid>/index.json` and each
/// transcript rel-path is relative to `<...>/<uuid>/<session_uuid>/`.
#[must_use]
pub fn build_transcript_map(archive_dir: Option<&Path>) -> BTreeMap<String, PathBuf> {
    let mut mapping = BTreeMap::new();
    let Some(root) = archive_dir else {
        return mapping;
    };
    if !root.is_dir() {
        return mapping;
    }
    for index_path in walk_index_files(root) {
        let Some(data) = super::read_json_file(&index_path) else {
            continue;
        };
        if data.get("schema").and_then(Value::as_str) != Some("loom.transcript-index/v1") {
            continue;
        }
        let Some(sess_dir) = index_path.parent() else {
            continue;
        };
        let uuid = data
            .get("session_uuid")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(agents) = data.get("agents").and_then(Value::as_array) else {
            continue;
        };
        for agent in agents {
            let (Some(agent_id), Some(tr)) = (
                agent.get("agent_id").and_then(Value::as_str),
                agent.get("transcript").and_then(Value::as_str),
            ) else {
                continue;
            };
            if agent_id.is_empty() {
                continue;
            }
            let abs = if uuid.is_empty() {
                sess_dir.join(tr)
            } else {
                sess_dir.join(uuid).join(tr)
            };
            mapping.insert(agent_id.to_string(), abs);
        }
    }
    mapping
}

/// Recursive `rglob("index.json")` equivalent. Depth-bounded so a pathological
/// archive tree cannot turn the harvest into a whole-filesystem walk.
fn walk_index_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 12 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push((path, depth + 1)),
                Ok(_) if path.file_name().is_some_and(|n| n == "index.json") => found.push(path),
                _ => {}
            }
        }
    }
    found.sort();
    found
}

// --------------------------------------------------------------------------
// Harvest / aggregation
// --------------------------------------------------------------------------

#[must_use]
pub fn read_records(stats_file: Option<&str>) -> Vec<Value> {
    let path = stats_path(stats_file);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(Value::is_object)
        .collect()
}

const PASS_VERDICTS: [&str; 6] = ["pass", "approve", "approved", "lgtm", "merged", "merge"];

fn is_pass(verdict: Option<&str>) -> bool {
    PASS_VERDICTS.contains(&verdict.unwrap_or("").trim().to_lowercase().as_str())
}

/// Resolve one record's cost + the actual token-fidelity source used.
///
/// Preference order: exact transcript usage (`transcript`) > the record's own
/// best-effort sweep-aggregate tokens (`sweep-aggregate-log`) > nothing
/// (`none`).
#[allow(clippy::cast_possible_truncation)]
fn record_cost(record: &Value, transcript_map: &BTreeMap<String, PathBuf>) -> (f64, &'static str) {
    if let Some(agent_id) = record.get("agent_id").and_then(Value::as_str) {
        if let Some(path) = transcript_map.get(agent_id) {
            let summary = sum_transcript_usage(path);
            if summary.usage_blocks > 0 {
                return (summary.cost_usd, "transcript");
            }
        }
    }
    let in_tok = record.get("in_tok").and_then(Value::as_f64);
    let out_tok = record.get("out_tok").and_then(Value::as_f64);
    if in_tok.is_some() || out_tok.is_some() {
        let cost = calc_cost(
            in_tok.unwrap_or(0.0) as i64,
            out_tok.unwrap_or(0.0) as i64,
            0,
            0,
            record.get("model").and_then(Value::as_str),
        );
        return (cost, "sweep-aggregate-log");
    }
    (0.0, "none")
}

#[derive(Default)]
struct IssueState {
    builder_model: Option<String>,
    first_judge_pass: Option<bool>,
    doctor_cycles: i64,
    merged: bool,
}

#[derive(Default)]
struct ArmRollup {
    n_issues: i64,
    n_judged: i64,
    n_first_pass: i64,
    doctor_cycles_total: i64,
    n_merged: i64,
}

fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

/// Aggregate the stats store into the per-arm inputs #3718 needs.
///
/// Per arm: first-attempt Judge-pass rate, mean Doctor cycles, exact total cost
/// (via transcript join where available), merge-rate quality floor, and the
/// derived per-issue mean cost that feeds the sonnet-first vs opus-first
/// inequality.
///
/// Arm attribution per issue: the explicit `experiment`-mode arm (A/B) wins
/// when present; otherwise the arm is inferred from the issue's observed
/// Builder model (opus→A, sonnet→B). Issues whose Builder model is neither
/// (or unknown) stay under `"?"`.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn harvest(stats_file: Option<&str>, archive_dir: Option<&Path>) -> Value {
    let records = read_records(stats_file);
    let transcript_map = build_transcript_map(archive_dir);

    // Pass 1 — resolve each issue's effective arm.
    let mut issue_explicit_arm: BTreeMap<i64, String> = BTreeMap::new();
    let mut issue_builder_model: BTreeMap<i64, Option<String>> = BTreeMap::new();
    for rec in &records {
        let Some(issue) = rec.get("issue").and_then(Value::as_i64) else {
            continue;
        };
        if let Some(arm) = rec
            .get("arm")
            .and_then(Value::as_str)
            .filter(|a| !a.is_empty())
        {
            issue_explicit_arm
                .entry(issue)
                .or_insert_with(|| arm.to_uppercase());
        }
        let role = rec
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        if role == "builder" {
            issue_builder_model
                .entry(issue)
                .or_insert_with(|| rec.get("model").and_then(Value::as_str).map(str::to_string));
        }
    }
    let issue_arm = |issue: i64| -> String {
        if let Some(a) = issue_explicit_arm.get(&issue) {
            return a.clone();
        }
        issue_builder_model
            .get(&issue)
            .and_then(|m| infer_arm_from_model(m.as_deref()))
            .unwrap_or("?")
            .to_string()
    };

    let mut issues: BTreeMap<(String, i64), IssueState> = BTreeMap::new();
    let mut arm_cost: BTreeMap<String, f64> = BTreeMap::new();
    let mut fidelity_counts: BTreeMap<&'static str, i64> =
        [("transcript", 0), ("sweep-aggregate-log", 0), ("none", 0)]
            .into_iter()
            .collect();

    for rec in &records {
        let issue = rec.get("issue").and_then(Value::as_i64);
        let arm = match issue {
            Some(i) => issue_arm(i),
            None => rec
                .get("arm")
                .and_then(Value::as_str)
                .filter(|a| !a.is_empty())
                .unwrap_or("?")
                .to_string(),
        };
        let phase = rec
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let role = rec
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();

        let (cost, fidelity) = record_cost(rec, &transcript_map);
        *arm_cost.entry(arm.clone()).or_insert(0.0) += cost;
        *fidelity_counts.entry(fidelity).or_insert(0) += 1;

        let Some(issue) = issue else { continue };
        let state = issues.entry((arm, issue)).or_default();
        if role == "builder" && state.builder_model.is_none() {
            state.builder_model = rec.get("model").and_then(Value::as_str).map(str::to_string);
        }
        if phase == "judge" || role == "judge" {
            let attempt = rec.get("attempt").and_then(Value::as_i64).unwrap_or(1);
            if attempt == 1 && state.first_judge_pass.is_none() {
                state.first_judge_pass =
                    Some(is_pass(rec.get("judge_verdict").and_then(Value::as_str)));
            }
        }
        if phase == "doctor" || role == "doctor" {
            state.doctor_cycles += 1;
        }
        if phase == "merge" {
            state.merged = true;
        }
    }

    // Roll issues up per arm.
    let mut arms: BTreeMap<String, ArmRollup> = BTreeMap::new();
    for ((arm, _issue), state) in &issues {
        let a = arms.entry(arm.clone()).or_default();
        a.n_issues += 1;
        if let Some(passed) = state.first_judge_pass {
            a.n_judged += 1;
            if passed {
                a.n_first_pass += 1;
            }
        }
        a.doctor_cycles_total += state.doctor_cycles;
        if state.merged {
            a.n_merged += 1;
        }
    }

    let report_arms: Vec<Value> = arms
        .iter()
        .map(|(arm, a)| {
            let n = a.n_issues;
            let judged = a.n_judged;
            let total_cost = arm_cost.get(arm).copied().unwrap_or(0.0);
            let model = ARM_MODEL
                .iter()
                .find(|(k, _)| k == arm)
                .map_or(Value::Null, |(_, m)| json!(m));
            let mut out = Map::new();
            out.insert("arm".into(), json!(arm));
            out.insert("model".into(), model);
            out.insert("n_issues".into(), json!(n));
            out.insert(
                "first_attempt_pass_rate".into(),
                if judged > 0 {
                    json!(a.n_first_pass as f64 / judged as f64)
                } else {
                    Value::Null
                },
            );
            out.insert(
                "mean_doctor_cycles".into(),
                if n > 0 {
                    json!(a.doctor_cycles_total as f64 / n as f64)
                } else {
                    Value::Null
                },
            );
            out.insert(
                "merge_rate".into(),
                if n > 0 {
                    json!(a.n_merged as f64 / n as f64)
                } else {
                    Value::Null
                },
            );
            out.insert("total_cost_usd".into(), json!(round6(total_cost)));
            out.insert(
                "mean_cost_per_issue_usd".into(),
                if n > 0 {
                    json!(round6(total_cost / n as f64))
                } else {
                    Value::Null
                },
            );
            Value::Object(out)
        })
        .collect();

    let mut fidelity = Map::new();
    for (k, v) in &fidelity_counts {
        fidelity.insert((*k).to_string(), json!(v));
    }

    let mut report = Map::new();
    report.insert("n_records".into(), json!(records.len()));
    report.insert("token_fidelity_counts".into(), Value::Object(fidelity));
    report.insert("arms".into(), json!(report_arms));
    Value::Object(report)
}

fn fmt_pct(v: Option<f64>) -> String {
    v.map_or_else(|| "-".to_string(), |x| format!("{:.0}%", x * 100.0))
}

/// Render a JSON number the way Python's `str()` does for the harvest summary
/// lines — `None` for null, `1.0` for an integral float.
fn fmt_py_number(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Number(n) => n.as_f64().map_or_else(
            || n.to_string(),
            |f| {
                if f.fract().abs() < f64::EPSILON {
                    format!("{f:.1}")
                } else {
                    format!("{f}")
                }
            },
        ),
        other => other.to_string(),
    }
}

#[must_use]
pub fn format_harvest_text(report: &Value) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("Sweep model-cost experiment — per-arm harvest (#3725 → #3718)".to_string());
    lines.push(format!(
        "  records: {}",
        report.get("n_records").and_then(Value::as_i64).unwrap_or(0)
    ));
    let fc = |k: &str| {
        report
            .get("token_fidelity_counts")
            .and_then(|f| f.get(k))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    lines.push(format!(
        "  token fidelity: transcript={} aggregate={} none={}",
        fc("transcript"),
        fc("sweep-aggregate-log"),
        fc("none")
    ));
    lines.push(String::new());
    let header = format!(
        "  {:<4} {:<8} {:>6} {:>9} {:>7} {:>6} {:>10} {:>10}",
        "arm", "model", "issues", "1st-pass", "cycles", "merge", "cost$", "$/issue"
    );
    let dashes = "-".repeat(header.chars().count().saturating_sub(2));
    lines.push(header);
    lines.push(format!("  {dashes}"));

    let empty: Vec<Value> = Vec::new();
    let arms = report
        .get("arms")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for a in arms {
        let fp = fmt_pct(a.get("first_attempt_pass_rate").and_then(Value::as_f64));
        let mc = a
            .get("mean_doctor_cycles")
            .and_then(Value::as_f64)
            .map_or_else(|| "-".to_string(), |x| format!("{x:.2}"));
        let mr = fmt_pct(a.get("merge_rate").and_then(Value::as_f64));
        let cpi = a
            .get("mean_cost_per_issue_usd")
            .and_then(Value::as_f64)
            .map_or_else(|| "-".to_string(), |x| format!("{x:.4}"));
        lines.push(format!(
            "  {:<4} {:<8} {:>6} {:>9} {:>7} {:>6} {:>10.4} {:>10}",
            a.get("arm").and_then(Value::as_str).unwrap_or("?"),
            a.get("model").and_then(Value::as_str).unwrap_or("-"),
            a.get("n_issues").and_then(Value::as_i64).unwrap_or(0),
            fp,
            mc,
            mr,
            a.get("total_cost_usd")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            cpi
        ));
    }

    // Inequality inputs the retune (#3718) consumes.
    let find_arm = |name: &str| {
        arms.iter()
            .find(|a| a.get("arm").and_then(Value::as_str) == Some(name))
    };
    if let (Some(a), Some(b)) = (find_arm("A"), find_arm("B")) {
        lines.push(String::new());
        lines.push("  Inequality inputs for #3718 (cost + merge-rate floor):".to_string());
        lines.push(format!(
            "    opus-first  (A): mean ${} / issue, merge-rate {}",
            fmt_py_number(&a["mean_cost_per_issue_usd"]),
            fmt_py_number(&a["merge_rate"])
        ));
        lines.push(format!(
            "    sonnet-first(B): mean ${} / issue, merge-rate {}",
            fmt_py_number(&b["mean_cost_per_issue_usd"]),
            fmt_py_number(&b["merge_rate"])
        ));
    }
    lines.join("\n")
}

#[must_use]
pub fn format_banner(
    mode: &str,
    issue: i64,
    arm: Option<&str>,
    model: Option<&str>,
    canary_source: Option<&str>,
) -> String {
    let bar = "=".repeat(72);
    let mut lines = vec![bar.clone()];
    match mode {
        "experiment" => {
            lines.push(format!("  LOOM MODEL EXPERIMENT — mode=EXPERIMENT  issue #{issue}"));
            lines.push(format!(
                "  ARM {}  ->  Builder model forced to '{}'",
                arm.unwrap_or("None"),
                model.unwrap_or("None")
            ));
            lines.push("  (tier-2.5 complexity bump SUPPRESSED for the forced arm)".to_string());
            let src = match canary_source {
                Some("env") => "env var LOOM_MODEL_EXPERIMENT_CANARY".to_string(),
                Some("sentinel") => format!("gitignored sentinel {CANARY_SENTINEL}"),
                _ => "unknown source".to_string(),
            };
            lines.push(format!("  CANARY confirmed via {src}."));
            lines.push("  CANARY-ONLY. Stats -> .loom/stats/sweep-model-stats.jsonl".to_string());
        }
        "observe" => {
            lines.push(format!("  LOOM MODEL EXPERIMENT — mode=OBSERVE  issue #{issue}"));
            lines.push("  Passive measurement only — no model forcing, no arm.".to_string());
            if canary_source == Some("unconfirmed") {
                lines.push(
                    "  (experiment requested but canary UNCONFIRMED — no uncommitted \
                     signal; downgraded.)"
                        .to_string(),
                );
            }
            lines.push("  Stats -> .loom/stats/sweep-model-stats.jsonl".to_string());
        }
        _ => {
            lines.push(format!("  LOOM MODEL EXPERIMENT — mode=OFF  issue #{issue}"));
            lines.push("  Instrumentation disabled — zero behavior change.".to_string());
        }
    }
    lines.push(bar);
    lines.join("\n")
}

/// `--archive-dir` / `LOOM_TRANSCRIPT_ARCHIVE` normalization: the sentinel
/// values `""`/`off`/`0`/`no`/`disabled` mean "no archive", not a directory.
#[must_use]
pub fn normalize_archive_dir(raw: Option<&str>) -> Option<PathBuf> {
    let raw = raw?;
    let lowered = raw.trim().to_lowercase();
    if lowered.is_empty() || matches!(lowered.as_str(), "off" | "0" | "no" | "disabled") {
        return None;
    }
    Some(PathBuf::from(raw))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn never_tracked(_: &Path) -> bool {
        false
    }
    fn always_tracked(_: &Path) -> bool {
        true
    }

    // ===== tri-state mode resolution =====

    #[test]
    fn mode_defaults_to_off() {
        let (mode, warnings) = resolve_raw_mode(None, &json!({}));
        assert_eq!(mode, "off");
        assert!(warnings.is_empty());
    }

    #[test]
    fn env_beats_config() {
        let cfg = json!({"sweep": {"modelExperiment": "experiment"}});
        assert_eq!(resolve_raw_mode(Some("observe"), &cfg).0, "observe");
        assert_eq!(resolve_raw_mode(Some("  OBSERVE "), &cfg).0, "observe");
        // A blank env value is unset, so config still applies.
        assert_eq!(resolve_raw_mode(Some("  "), &cfg).0, "experiment");
    }

    #[test]
    fn malformed_env_or_config_is_off_with_a_warning() {
        let (mode, warnings) = resolve_raw_mode(Some("bogus"), &json!({}));
        assert_eq!(mode, "off");
        assert_eq!(warnings.len(), 1);

        let (mode, warnings) =
            resolve_raw_mode(None, &json!({"sweep": {"modelExperiment": "bogus"}}));
        assert_eq!(mode, "off");
        assert_eq!(warnings.len(), 1);

        let (mode, warnings) = resolve_raw_mode(None, &json!({"sweep": {"modelExperiment": 7}}));
        assert_eq!(mode, "off");
        assert_eq!(warnings.len(), 1);
    }

    // ===== canary guardrail (#3731) =====

    #[test]
    fn experiment_downgrades_to_observe_without_a_canary() {
        let (mode, warnings) =
            resolve_effective_mode(Some("experiment"), None, &json!({}), None, &never_tracked);
        assert_eq!(mode, "observe");
        assert!(warnings.iter().any(|w| w.contains("NON-CANARY")));
    }

    #[test]
    fn env_canary_confirms_experiment() {
        for truthy in TRUTHY {
            let (mode, _) = resolve_effective_mode(
                Some("experiment"),
                Some(truthy),
                &json!({}),
                None,
                &never_tracked,
            );
            assert_eq!(mode, "experiment", "canary value {truthy} should confirm");
        }
        let (mode, _) = resolve_effective_mode(
            Some("experiment"),
            Some("nope"),
            &json!({}),
            None,
            &never_tracked,
        );
        assert_eq!(mode, "observe");
    }

    #[test]
    fn untracked_sentinel_confirms_but_tracked_one_is_refused() {
        let dir = tempdir().unwrap();
        let sentinel = dir.path().join("CANARY");
        std::fs::write(&sentinel, "").unwrap();

        let (ok, source, warnings) = evaluate_canary(None, Some(&sentinel), &never_tracked);
        assert!(ok);
        assert_eq!(source, Some(CanarySource::Sentinel));
        assert_eq!(source.unwrap().label(), "sentinel");
        assert!(warnings.is_empty());

        let (ok, source, warnings) = evaluate_canary(None, Some(&sentinel), &always_tracked);
        assert!(!ok);
        assert!(source.is_none());
        assert!(warnings.iter().any(|w| w.contains("TRACKED by git")));
    }

    /// A committed `sweep.modelExperimentCanary` must be inert (#3731).
    #[test]
    fn committed_config_canary_is_inert() {
        let cfg =
            json!({"sweep": {"modelExperiment": "experiment", "modelExperimentCanary": true}});
        let (mode, _) = resolve_effective_mode(None, None, &cfg, None, &never_tracked);
        assert_eq!(mode, "observe");
    }

    // ===== arm assignment =====

    #[test]
    fn arm_assignment_is_deterministic_and_stratified() {
        assert_eq!(assign_arm(100, Some("routine")), "A");
        assert_eq!(assign_arm(100, Some("routine")), "A");
        assert_eq!(assign_arm(100, Some("complex")), "B");
        assert_eq!(assign_arm(101, Some("routine")), "B");
        assert_eq!(assign_arm(101, Some("complex")), "A");
        // An absent/unknown marker is `routine`.
        assert_eq!(assign_arm(100, None), "A");
        assert_eq!(assign_arm(100, Some("mystery")), "A");
    }

    #[test]
    fn arm_model_and_resolution() {
        assert_eq!(arm_model("A"), "opus");
        assert_eq!(arm_model(" b "), "sonnet");
        assert_eq!(arm_model("Z"), "");
        assert_eq!(resolved_arm_model("A", &json!({})), "claude-opus-5");
        assert_eq!(resolved_arm_model("B", &json!({})), "sonnet");
        assert_eq!(resolved_arm_model("Z", &json!({})), "");
    }

    /// #4060: Arm A must resolve to exactly what `resolve-model.sh opus`
    /// prints. Both go through the one shared resolver.
    #[test]
    fn arm_a_matches_resolve_model_output() {
        for cfg in [
            json!({}),
            json!({"sweep": {"modelAliases": {"opus": "claude-opus-9"}}}),
            json!({"sweep": {"modelAliases": {"opus": "opus"}}}),
        ] {
            assert_eq!(
                resolved_arm_model("A", &cfg),
                super::super::model_tiers::resolve_model("opus", &cfg)
            );
            assert_eq!(
                resolved_arm_model("B", &cfg),
                super::super::model_tiers::resolve_model("sonnet", &cfg)
            );
        }
    }

    #[test]
    fn model_family_is_generation_agnostic() {
        assert_eq!(model_family(Some("claude-sonnet-9")), Some("sonnet"));
        assert_eq!(model_family(Some("claude-3-5-sonnet")), Some("sonnet"));
        assert_eq!(model_family(Some("opus")), Some("opus"));
        assert_eq!(model_family(Some("claude-fable-5")), Some("fable"));
        assert_eq!(model_family(Some("gpt-5")), None);
        assert_eq!(model_family(None), None);
    }

    #[test]
    fn arm_inference_only_covers_the_two_arms() {
        assert_eq!(infer_arm_from_model(Some("claude-opus-5")), Some("A"));
        assert_eq!(infer_arm_from_model(Some("sonnet")), Some("B"));
        assert_eq!(infer_arm_from_model(Some("claude-haiku-5")), None);
        assert_eq!(infer_arm_from_model(Some("claude-fable-5")), None);
        assert_eq!(infer_arm_from_model(None), None);
    }

    // ===== pricing =====

    /// The pricing table is now shared with `resource_usage.rs` — one
    /// implementation, so the old "keep both in sync" mirror pair is gone.
    #[test]
    fn pricing_matches_the_shared_daemon_table() {
        assert_eq!(model_pricing(Some("sonnet")), (0.003, 0.015, 0.0003, 0.00375));
        assert_eq!(model_pricing(Some("claude-opus-5")), (0.015, 0.075, 0.0015, 0.01875));
        assert_eq!(model_pricing(Some("fable")), (0.015, 0.075, 0.0015, 0.01875));
        assert_eq!(model_pricing(Some("claude-fable-5")), (0.015, 0.075, 0.0015, 0.01875));
        assert_eq!(model_pricing(Some("haiku")), (0.00025, 0.00125, 0.00003, 0.0003));
        // Unknown → Sonnet default (matches the Rust table's fallback).
        assert_eq!(model_pricing(Some("mystery")), (0.003, 0.015, 0.0003, 0.00375));
        assert_eq!(model_pricing(None), (0.003, 0.015, 0.0003, 0.00375));
    }

    #[test]
    fn cost_arithmetic_is_cache_aware() {
        // The exact figure `test-sweep-experiment.sh` asserts.
        let cost = calc_cost(10, 5, 2000, 1000, Some("claude-opus-4-8"));
        assert!((cost - 0.022_275).abs() < 1e-12, "got {cost}");
    }

    // ===== records =====

    #[test]
    fn record_shape_matches_the_python_contract() {
        let rec = build_record(
            &RecordFields {
                issue: 100,
                phase: "builder",
                role: "builder",
                model: Some("opus"),
                mode: "experiment",
                arm: Some("a"),
                attempt: 1,
                token_fidelity: "none",
                ..RecordFields::default()
            },
            "2026-01-01T00:00:00Z",
        );
        assert_eq!(rec["ts"], json!("2026-01-01T00:00:00Z"));
        assert_eq!(rec["arm"], json!("A"), "arm is upper-cased");
        assert_eq!(rec["complexity"], json!("routine"), "absent marker defaults");
        assert_eq!(rec["pr"], Value::Null);
        assert_eq!(rec["in_tok"], Value::Null);
        assert_eq!(rec["token_fidelity"], json!("none"));
        let keys: Vec<&String> = rec.as_object().unwrap().keys().collect();
        assert_eq!(keys[0], "ts");
        assert_eq!(keys[1], "issue");
    }

    #[test]
    fn append_creates_a_private_jsonl_store() {
        let dir = tempdir().unwrap();
        let stats = dir.path().join("nested").join("stats.jsonl");
        let stats_s = stats.to_string_lossy().to_string();
        for i in 0..3 {
            let rec = build_record(
                &RecordFields {
                    issue: 100 + i,
                    phase: "builder",
                    role: "builder",
                    mode: "observe",
                    attempt: 1,
                    token_fidelity: "none",
                    ..RecordFields::default()
                },
                "2026-01-01T00:00:00Z",
            );
            append_record(&rec, Some(&stats_s)).unwrap();
        }
        let text = std::fs::read_to_string(&stats).unwrap();
        assert_eq!(text.lines().count(), 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&stats).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "stats file must be 0600");
        }
    }

    // ===== transcript join + harvest =====

    fn write_fixture(root: &Path) -> (String, PathBuf) {
        let stats = root.join("stats.jsonl");
        let sess = root.join("archive/myrepo/2026-07-22/UUID1");
        std::fs::create_dir_all(sess.join("UUID1/subagents")).unwrap();
        std::fs::write(
            sess.join("UUID1/subagents/agent-bld1.jsonl"),
            r#"{"message":{"model":"claude-opus-4-8","usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":1000,"cache_read_input_tokens":2000}}}"#,
        )
        .unwrap();
        std::fs::write(
            sess.join("index.json"),
            r#"{"schema":"loom.transcript-index/v1","session_uuid":"UUID1","repo":"myrepo","agents":[{"agent_id":"agent-bld1","role":"loom-builder","transcript":"subagents/agent-bld1.jsonl"}]}"#,
        )
        .unwrap();
        (stats.to_string_lossy().to_string(), root.join("archive"))
    }

    fn append_arm_a_chain(stats: &str) {
        for fields in [
            RecordFields {
                issue: 100,
                phase: "builder",
                role: "builder",
                model: Some("opus"),
                mode: "experiment",
                arm: Some("A"),
                attempt: 1,
                complexity: Some("routine"),
                agent_id: Some("agent-bld1"),
                token_fidelity: "none",
                ..RecordFields::default()
            },
            RecordFields {
                issue: 100,
                phase: "judge",
                role: "judge",
                mode: "experiment",
                arm: Some("A"),
                attempt: 1,
                judge_verdict: Some("pass"),
                token_fidelity: "none",
                ..RecordFields::default()
            },
            RecordFields {
                issue: 100,
                phase: "merge",
                role: "merge",
                mode: "experiment",
                arm: Some("A"),
                attempt: 1,
                token_fidelity: "none",
                ..RecordFields::default()
            },
        ] {
            append_record(&build_record(&fields, "2026-01-01T00:00:00Z"), Some(stats)).unwrap();
        }
    }

    #[test]
    fn harvest_joins_transcripts_for_exact_cost() {
        let dir = tempdir().unwrap();
        let (stats, archive) = write_fixture(dir.path());
        append_arm_a_chain(&stats);

        let report = harvest(Some(&stats), Some(&archive));
        assert_eq!(report["n_records"], json!(3));
        assert_eq!(report["token_fidelity_counts"]["transcript"], json!(1));
        let arm_a = &report["arms"][0];
        assert_eq!(arm_a["arm"], json!("A"));
        assert_eq!(arm_a["model"], json!("opus"));
        assert_eq!(arm_a["n_issues"], json!(1));
        assert_eq!(arm_a["first_attempt_pass_rate"], json!(1.0));
        assert_eq!(arm_a["merge_rate"], json!(1.0));
        assert_eq!(arm_a["mean_doctor_cycles"], json!(0.0));
        assert_eq!(arm_a["total_cost_usd"], json!(0.022_275));
        assert_eq!(arm_a["mean_cost_per_issue_usd"], json!(0.022_275));
    }

    /// The shell harness asserts the literal substrings `"transcript": 1` and
    /// `"first_attempt_pass_rate": 1.0` in the pretty-printed JSON.
    #[test]
    fn harvest_json_renders_the_shapes_the_shell_test_asserts() {
        let dir = tempdir().unwrap();
        let (stats, archive) = write_fixture(dir.path());
        append_arm_a_chain(&stats);
        let text = serde_json::to_string_pretty(&harvest(Some(&stats), Some(&archive))).unwrap();
        assert!(text.contains("\"transcript\": 1"), "{text}");
        assert!(text.contains("\"first_attempt_pass_rate\": 1.0"), "{text}");
        assert!(text.contains("\"merge_rate\": 1.0"), "{text}");
        assert!(text.contains("0.022275"), "{text}");
    }

    #[test]
    fn harvest_on_a_missing_store_is_empty_not_a_crash() {
        let report = harvest(Some("/nonexistent/loom-stats.jsonl"), None);
        assert_eq!(report["n_records"], json!(0));
        assert_eq!(report["arms"], json!([]));
        let text = format_harvest_text(&report);
        assert!(text.contains("records: 0"));
    }

    #[test]
    fn observe_mode_records_are_arm_inferred_from_the_builder_model() {
        let dir = tempdir().unwrap();
        let stats = dir.path().join("stats.jsonl").to_string_lossy().to_string();
        append_record(
            &build_record(
                &RecordFields {
                    issue: 7,
                    phase: "builder",
                    role: "builder",
                    model: Some("claude-sonnet-4-6"),
                    mode: "observe",
                    attempt: 1,
                    token_fidelity: "none",
                    ..RecordFields::default()
                },
                "2026-01-01T00:00:00Z",
            ),
            Some(&stats),
        )
        .unwrap();
        let report = harvest(Some(&stats), None);
        assert_eq!(report["arms"][0]["arm"], json!("B"));
        assert_eq!(report["arms"][0]["model"], json!("sonnet"));
    }

    #[test]
    fn unattributable_records_land_in_the_question_bucket() {
        let dir = tempdir().unwrap();
        let stats = dir.path().join("stats.jsonl").to_string_lossy().to_string();
        append_record(
            &build_record(
                &RecordFields {
                    issue: 9,
                    phase: "builder",
                    role: "builder",
                    model: Some("claude-haiku-5"),
                    mode: "observe",
                    attempt: 1,
                    token_fidelity: "none",
                    ..RecordFields::default()
                },
                "2026-01-01T00:00:00Z",
            ),
            Some(&stats),
        )
        .unwrap();
        let report = harvest(Some(&stats), None);
        assert_eq!(report["arms"][0]["arm"], json!("?"));
        assert_eq!(report["arms"][0]["model"], Value::Null);
    }

    #[test]
    fn aggregate_log_tokens_are_the_fallback_fidelity() {
        let dir = tempdir().unwrap();
        let stats = dir.path().join("stats.jsonl").to_string_lossy().to_string();
        append_record(
            &build_record(
                &RecordFields {
                    issue: 5,
                    phase: "builder",
                    role: "builder",
                    model: Some("sonnet"),
                    mode: "observe",
                    attempt: 1,
                    in_tok: Some(1000),
                    out_tok: Some(1000),
                    token_fidelity: "sweep-aggregate-log",
                    ..RecordFields::default()
                },
                "2026-01-01T00:00:00Z",
            ),
            Some(&stats),
        )
        .unwrap();
        let report = harvest(Some(&stats), None);
        assert_eq!(report["token_fidelity_counts"]["sweep-aggregate-log"], json!(1));
        assert_eq!(report["arms"][0]["total_cost_usd"], json!(0.018));
    }

    #[test]
    fn doctor_cycles_are_counted_per_issue() {
        let dir = tempdir().unwrap();
        let stats = dir.path().join("stats.jsonl").to_string_lossy().to_string();
        for phase in ["builder", "doctor", "doctor"] {
            append_record(
                &build_record(
                    &RecordFields {
                        issue: 11,
                        phase,
                        role: phase,
                        model: Some("opus"),
                        mode: "observe",
                        attempt: 1,
                        token_fidelity: "none",
                        ..RecordFields::default()
                    },
                    "2026-01-01T00:00:00Z",
                ),
                Some(&stats),
            )
            .unwrap();
        }
        let report = harvest(Some(&stats), None);
        assert_eq!(report["arms"][0]["mean_doctor_cycles"], json!(2.0));
        assert_eq!(report["arms"][0]["merge_rate"], json!(0.0));
        assert_eq!(report["arms"][0]["first_attempt_pass_rate"], Value::Null);
    }

    #[test]
    fn transcript_map_ignores_foreign_index_schemas() {
        let dir = tempdir().unwrap();
        let sess = dir.path().join("a/b");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(
            sess.join("index.json"),
            r#"{"schema":"something/else","agents":[{"agent_id":"x","transcript":"t.jsonl"}]}"#,
        )
        .unwrap();
        assert!(build_transcript_map(Some(dir.path())).is_empty());
        assert!(build_transcript_map(None).is_empty());
        assert!(build_transcript_map(Some(Path::new("/nonexistent"))).is_empty());
    }

    #[test]
    fn sum_transcript_usage_skips_bad_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(
            &path,
            "not json\n\n{\"usage\":{\"input_tokens\":1000}}\n{\"message\":{\"model\":\"sonnet\",\"usage\":{\"output_tokens\":1000}}}\n",
        )
        .unwrap();
        let got = sum_transcript_usage(&path);
        assert_eq!(got.usage_blocks, 2);
        assert_eq!(got.input_tokens, 1000);
        assert_eq!(got.output_tokens, 1000);

        let missing = sum_transcript_usage(&dir.path().join("nope.jsonl"));
        assert_eq!(missing.usage_blocks, 0);
        assert_eq!(missing.cost_usd, 0.0);
    }

    // ===== banner + misc =====

    #[test]
    fn banner_names_mode_arm_and_suppression() {
        let out = format_banner("experiment", 100, Some("B"), Some("sonnet"), Some("env"));
        assert!(out.contains("mode=EXPERIMENT"));
        assert!(out.contains("ARM B"));
        assert!(out.contains("SUPPRESSED"));
        assert!(out.contains("env var LOOM_MODEL_EXPERIMENT_CANARY"));

        let out = format_banner("observe", 100, None, None, Some("unconfirmed"));
        assert!(out.contains("mode=OBSERVE"));
        assert!(out.contains("canary UNCONFIRMED"));

        let out = format_banner("off", 100, None, None, None);
        assert!(out.contains("mode=OFF"));
        assert!(out.contains("zero behavior change"));
    }

    #[test]
    fn archive_dir_disable_sentinels() {
        assert!(normalize_archive_dir(None).is_none());
        for s in ["", "  ", "off", "0", "NO", "disabled"] {
            assert!(normalize_archive_dir(Some(s)).is_none(), "{s} should disable");
        }
        assert_eq!(
            normalize_archive_dir(Some("/tmp/archive")),
            Some(PathBuf::from("/tmp/archive"))
        );
    }
}
