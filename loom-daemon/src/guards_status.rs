//! `loom-daemon guards status` — show what the `PreToolUse` guard layer is
//! *actually* configured to do, and warn when that is not what the operator
//! probably thinks (Issue #10434).
//!
//! The guard hook runs as a separate process and reads **Claude Code's**
//! environment (the `env` block of a `settings.json`), never the environment of
//! the command it is judging. An inline `LOOM_GUARD_CLOUD=0 aws ...` prefix and a
//! `LOOM_GUARD_*` export in a shell rc therefore do nothing, and nothing used to
//! say so. This module is pure (all I/O inputs are injected through [`Inputs`])
//! so the fixtures in the tests cover precedence, wiring, log summaries and every
//! warning.
//!
//! Resolution order mirrors the hook: env (Claude Code settings `env`) beats
//! `.loom-project/project.json` / `.loom/config.json` `guards.*`, which beats the
//! default. Keep [`CATEGORIES`] in sync with `toggle_hint_for_tag()` in
//! `defaults/hooks/guard-destructive-generic.sh`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde::Serialize;
use serde_json::Value;

/// Default number of asks in the window before the "set the toggle" warning.
pub const DEFAULT_ASK_THRESHOLD: usize = 3;
/// Window for the log-based warnings.
pub const WINDOW_DAYS: i64 = 7;
/// Share of in-window decisions that are `*-unresolved-var` denies above which
/// the scratch-dir warning fires...
pub const UNRESOLVED_VAR_SHARE: f64 = 0.25;
/// ...provided at least this many decisions were logged in the window.
pub const UNRESOLVED_VAR_MIN_SAMPLE: usize = 10;

/// How a category's value is shaped.
#[derive(Clone, Copy)]
enum Kind {
    Bool {
        default: bool,
    },
    Enum {
        default: &'static str,
        allowed: &'static [&'static str],
    },
}

struct Category {
    /// `guards.<key>` in config.
    key: &'static str,
    env: &'static str,
    kind: Kind,
    /// Decision-log rule-tag prefixes that belong to this category.
    tags: &'static [&'static str],
}

const CATEGORIES: &[Category] = &[
    Category {
        key: "sqlDdl",
        env: "LOOM_GUARD_SQL",
        kind: Kind::Bool { default: true },
        tags: &["sql-ddl", "sql-delete-no-where"],
    },
    Category {
        key: "cloudCli",
        env: "LOOM_GUARD_CLOUD",
        kind: Kind::Bool { default: true },
        tags: &["cloud-cli:"],
    },
    Category {
        key: "cargoCleanScope",
        env: "LOOM_GUARD_CARGO_CLEAN",
        kind: Kind::Bool { default: true },
        tags: &["cargo-clean-scope"],
    },
    Category {
        key: "reversibleGh",
        env: "LOOM_GUARD_REVERSIBLE_GH",
        kind: Kind::Bool { default: false },
        tags: &["reversible-gh:"],
    },
    Category {
        key: "decisionLog",
        env: "LOOM_GUARD_DECISION_LOG",
        kind: Kind::Bool { default: false },
        tags: &[],
    },
    Category {
        key: "readOnlyFastPath",
        env: "LOOM_GUARD_READONLY_FASTPATH",
        kind: Kind::Bool { default: true },
        tags: &[],
    },
    Category {
        key: "worktreeIsolation",
        env: "LOOM_GUARD_WORKTREE_ISOLATION",
        kind: Kind::Bool { default: true },
        tags: &["worktree-write-confinement"],
    },
    Category {
        key: "stashScope",
        env: "LOOM_GUARD_STASH_SCOPE",
        kind: Kind::Bool { default: true },
        tags: &["stash-scope:"],
    },
    Category {
        key: "backgroundSubagents",
        env: "LOOM_GUARD_BACKGROUND_SUBAGENTS",
        kind: Kind::Bool { default: true },
        tags: &[],
    },
    Category {
        key: "installedFileWrites",
        env: "LOOM_GUARD_INSTALLED_FILE_WRITES",
        kind: Kind::Bool { default: true },
        tags: &[],
    },
    Category {
        key: "rmScope",
        env: "LOOM_RM_SCOPE",
        kind: Kind::Enum {
            default: "repo",
            allowed: &["repo", "off", "permissive"],
        },
        tags: &["rm-scope-"],
    },
    Category {
        key: "forceScope",
        env: "LOOM_FORCE_SCOPE",
        kind: Kind::Enum {
            default: "all",
            allowed: &["all", "protected", "off"],
        },
        tags: &["force-op:"],
    },
];

/// The category (config key) a decision-log rule tag belongs to, if toggleable.
#[must_use]
pub fn category_for_tag(tag: &str) -> Option<&'static str> {
    CATEGORIES
        .iter()
        .find(|c| c.tags.iter().any(|p| tag.starts_with(p)))
        .map(|c| c.key)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Env,
    Config,
    Default,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Env => "env (Claude Code settings.json env)",
            Source::Config => ".loom/config.json",
            Source::Default => "default",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CategoryStatus {
    pub key: String,
    pub env_var: String,
    pub value: String,
    pub source: Source,
    /// Which settings file supplied the env value, when `source == Env`.
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Wiring {
    pub path: String,
    pub exists: bool,
    pub wired: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RuleCounts {
    pub ask: usize,
    pub deny: usize,
    pub ask_7d: usize,
    pub deny_7d: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LogSummary {
    pub path: String,
    pub exists: bool,
    pub total: usize,
    pub malformed: usize,
    pub by_rule: BTreeMap<String, RuleCounts>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Warning {
    /// Stable all-caps headline; tests and scripts match on it.
    pub headline: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub categories: Vec<CategoryStatus>,
    pub repo_wiring: Wiring,
    pub repo_local_wiring: Wiring,
    pub global_wiring: Wiring,
    pub log: LogSummary,
    pub warnings: Vec<Warning>,
}

/// Everything the report depends on, injected so tests control it.
pub struct Inputs {
    pub repo_root: PathBuf,
    /// `$HOME`; `None` skips global settings and shell rc checks.
    pub home: Option<PathBuf>,
    pub now: DateTime<Utc>,
    pub ask_threshold: usize,
    /// Overrides the decision log path (`LOOM_GUARD_DECISION_LOG_FILE`).
    pub log_path: Option<PathBuf>,
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// `env` block of a settings file as name -> string value.
fn settings_env(path: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(Value::Object(m)) = read_json(path).as_ref().and_then(|v| v.get("env")) {
        for (k, v) in m {
            match v {
                Value::String(s) => out.insert(k.clone(), s.clone()),
                Value::Bool(b) => out.insert(k.clone(), b.to_string()),
                Value::Number(n) => out.insert(k.clone(), n.to_string()),
                _ => None,
            };
        }
    }
    out
}

fn wiring(path: &Path) -> Wiring {
    let json = read_json(path);
    let wired = json
        .as_ref()
        .and_then(|v| v.get("hooks"))
        .is_some_and(|h| h.to_string().contains("guard-destructive"));
    Wiring {
        path: path.display().to_string(),
        exists: path.exists(),
        wired,
    }
}

/// Normalise a raw env/config value for a category; `None` = unrecognised
/// (the hook ignores it and falls through to the next tier).
fn normalise(kind: Kind, raw: &str) -> Option<String> {
    let r = raw.trim().to_ascii_lowercase();
    match kind {
        Kind::Bool { .. } => match r.as_str() {
            "0" | "false" | "no" => Some("false".into()),
            "1" | "true" | "yes" => Some("true".into()),
            _ => None,
        },
        Kind::Enum { allowed, .. } => {
            let r = if matches!(r.as_str(), "0" | "no") {
                "off".to_string()
            } else {
                r
            };
            allowed.contains(&r.as_str()).then_some(r)
        }
    }
}

fn config_raw(repo_root: &Path, key: &str) -> Option<String> {
    for rel in [".loom-project/project.json", ".loom/config.json"] {
        let found =
            read_json(&repo_root.join(rel)).and_then(|v| v.get("guards")?.get(key).cloned());
        match found {
            Some(Value::Bool(b)) => return Some(b.to_string()),
            Some(Value::String(s)) => return Some(s),
            _ => {}
        }
    }
    None
}

fn default_of(kind: Kind) -> String {
    match kind {
        Kind::Bool { default } => default.to_string(),
        Kind::Enum { default, .. } => default.to_string(),
    }
}

fn resolve_categories(
    repo_root: &Path,
    env_layers: &[(String, BTreeMap<String, String>)],
) -> Vec<CategoryStatus> {
    CATEGORIES
        .iter()
        .map(|c| {
            let from_env = env_layers.iter().find_map(|(origin, m)| {
                let v = normalise(c.kind, m.get(c.env)?)?;
                Some((v, origin.clone()))
            });
            if let Some((value, origin)) = from_env {
                return CategoryStatus {
                    key: c.key.into(),
                    env_var: c.env.into(),
                    value,
                    source: Source::Env,
                    origin: Some(origin),
                };
            }
            if let Some(value) = config_raw(repo_root, c.key).and_then(|r| normalise(c.kind, &r)) {
                return CategoryStatus {
                    key: c.key.into(),
                    env_var: c.env.into(),
                    value,
                    source: Source::Config,
                    origin: None,
                };
            }
            CategoryStatus {
                key: c.key.into(),
                env_var: c.env.into(),
                value: default_of(c.kind),
                source: Source::Default,
                origin: None,
            }
        })
        .collect()
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ")
        .ok()
        .map(|n| n.and_utc())
        .or_else(|| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|d| d.with_timezone(&Utc))
        })
}

/// Summarise JSONL decision-log text. Malformed lines are counted, never fatal.
#[must_use]
pub fn summarise_log(
    text: &str,
    now: DateTime<Utc>,
) -> (BTreeMap<String, RuleCounts>, usize, usize) {
    let cutoff = now - Duration::days(WINDOW_DAYS);
    let mut by_rule: BTreeMap<String, RuleCounts> = BTreeMap::new();
    let (mut total, mut malformed) = (0, 0);
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let rec = serde_json::from_str::<Value>(line).ok();
        let fields = rec.as_ref().and_then(|v| {
            Some((
                v.get("decision")?.as_str()?.to_string(),
                v.get("pattern")?.as_str()?.to_string(),
                v.get("ts").and_then(Value::as_str).and_then(parse_ts),
            ))
        });
        let Some((decision, pattern, ts)) = fields else {
            malformed += 1;
            continue;
        };
        if decision != "ask" && decision != "deny" {
            malformed += 1;
            continue;
        }
        total += 1;
        let e = by_rule.entry(pattern).or_default();
        let recent = ts.is_some_and(|t| t >= cutoff);
        if decision == "ask" {
            e.ask += 1;
            e.ask_7d += usize::from(recent);
        } else {
            e.deny += 1;
            e.deny_7d += usize::from(recent);
        }
    }
    (by_rule, total, malformed)
}

/// `LOOM_GUARD_*` / `LOOM_RM_SCOPE` / `LOOM_FORCE_SCOPE` assignments in a shell rc.
fn rc_guard_vars(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim_start();
        if l.starts_with('#') {
            continue;
        }
        let l = l.strip_prefix("export ").unwrap_or(l).trim_start();
        if let Some((name, _)) = l.split_once('=') {
            let n = name.trim();
            let ours = CATEGORIES.iter().any(|c| c.env == n) || n == "LOOM_GUARD_SCRATCH_ROOT";
            if ours && !out.iter().any(|o| o == n) {
                out.push(n.to_string());
            }
        }
    }
    out
}

/// Build the full report.
#[must_use]
pub fn collect(inputs: &Inputs) -> Report {
    let repo = &inputs.repo_root;
    let repo_settings = repo.join(".claude/settings.json");
    let repo_local = repo.join(".claude/settings.local.json");
    let global_settings = inputs
        .home
        .as_ref()
        .map(|h| h.join(".claude/settings.json"));

    // Highest precedence first, as Claude Code merges them.
    let mut layers = vec![
        ("repo .claude/settings.local.json".to_string(), settings_env(&repo_local)),
        ("repo .claude/settings.json".to_string(), settings_env(&repo_settings)),
    ];
    if let Some(g) = &global_settings {
        layers.push(("~/.claude/settings.json".to_string(), settings_env(g)));
    }

    let categories = resolve_categories(repo, &layers);
    let repo_wiring = wiring(&repo_settings);
    let repo_local_wiring = wiring(&repo_local);
    let global_wiring = global_settings.as_deref().map_or(
        Wiring {
            path: "(no HOME)".into(),
            exists: false,
            wired: false,
        },
        wiring,
    );

    let log_path = inputs
        .log_path
        .clone()
        .unwrap_or_else(|| repo.join(".loom/logs/guard-decisions.log"));
    let text = std::fs::read_to_string(&log_path).ok();
    let (by_rule, total, malformed) = text
        .as_deref()
        .map_or_else(Default::default, |t| summarise_log(t, inputs.now));
    let log = LogSummary {
        path: log_path.display().to_string(),
        exists: text.is_some(),
        total,
        malformed,
        by_rule,
    };

    let mut warnings = Vec::new();

    // (a) rc-only env.
    if let Some(home) = &inputs.home {
        for rc in [".bashrc", ".zshrc", ".profile"] {
            let Ok(t) = std::fs::read_to_string(home.join(rc)) else {
                continue;
            };
            for var in rc_guard_vars(&t) {
                if layers.iter().any(|(_, m)| m.contains_key(&var)) {
                    continue;
                }
                warnings.push(Warning {
                    headline: "SHELL-RC-ONLY-ENV".into(),
                    detail: format!(
                        "{var} is set in ~/{rc} but not in the Claude Code settings \"env\" block. The guard hook reads Claude Code's environment, so this has no effect. Set it under \"env\" in ~/.claude/settings.json (or .claude/settings.json), or use the .loom/config.json guards key."
                    ),
                });
            }
        }
    }

    // (b) repeated asks on a category that is still at its default.
    let mut asks_by_cat: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (tag, c) in &log.by_rule {
        if let Some(cat) = category_for_tag(tag) {
            *asks_by_cat.entry(cat).or_default() += c.ask_7d;
        }
    }
    for (cat, n) in asks_by_cat {
        let st = categories.iter().find(|c| c.key == cat);
        if n >= inputs.ask_threshold && st.is_some_and(|s| s.source == Source::Default) {
            let env = st.map_or("", |s| s.env_var.as_str());
            warnings.push(Warning {
                headline: "REPEATED-ASKS-UNCONFIGURED".into(),
                detail: format!(
                    "{n} asks for guards.{cat} in the last {WINDOW_DAYS} days and the toggle is unset. The decision log records no approval outcome, so ask count is used as the proxy for \"all approved\". If you approve these every time, set guards.{cat} in .loom/config.json or {env} in Claude Code settings \"env\" (an inline env prefix on the command does not work)."
                ),
            });
        }
    }

    // (c) double wiring.
    let repo_wired = repo_wiring.wired || repo_local_wiring.wired;
    if repo_wired && global_wiring.wired {
        warnings.push(Warning {
            headline: "DOUBLE-WIRED-HOOK".into(),
            detail: "guard-destructive is wired in both ~/.claude/settings.json and the repo's .claude settings, so every Bash call is evaluated twice (and each ask may prompt twice). Keep one.".into(),
        });
    }

    // (d) unresolved-var deny share.
    let (mut recent_total, mut recent_unres) = (0usize, 0usize);
    for (tag, c) in &log.by_rule {
        recent_total += c.ask_7d + c.deny_7d;
        if tag.ends_with("-unresolved-var") {
            recent_unres += c.deny_7d;
        }
    }
    if recent_total >= UNRESOLVED_VAR_MIN_SAMPLE
        && (recent_unres as f64) / (recent_total as f64) >= UNRESOLVED_VAR_SHARE
    {
        warnings.push(Warning {
            headline: "HIGH-UNRESOLVED-VAR-DENY-RATE".into(),
            detail: format!(
                "{recent_unres} of {recent_total} guard decisions in the last {WINDOW_DAYS} days were *-unresolved-var denies (rm/redirect to a $VAR the guard cannot resolve). Write to a literal scratch dir (LOOM_GUARD_SCRATCH_ROOT / guards.scratchRoot, see defaults/docs/guard-hooks.md) instead of a shell variable path."
            ),
        });
    }

    Report {
        categories,
        repo_wiring,
        repo_local_wiring,
        global_wiring,
        log,
        warnings,
    }
}

/// Human-readable rendering.
#[must_use]
pub fn render(r: &Report) -> String {
    let mut s = String::from("Guard categories (env beats config beats default):\n");
    for c in &r.categories {
        let origin = c
            .origin
            .as_deref()
            .map(|o| format!(" [{o}]"))
            .unwrap_or_default();
        let _ = writeln!(
            s,
            "  guards.{:<20} {:<10} source: {}{}  ({})",
            c.key,
            c.value,
            c.source.label(),
            origin,
            c.env_var
        );
    }
    s.push_str("\nHook wiring (guard-destructive on PreToolUse):\n");
    for w in [&r.repo_wiring, &r.repo_local_wiring, &r.global_wiring] {
        let state = if w.wired {
            "wired"
        } else if w.exists {
            "not wired"
        } else {
            "no file"
        };
        let _ = writeln!(s, "  {:<9} {}", state, w.path);
    }
    let _ = writeln!(s, "\nDecision log: {}", r.log.path);
    if !r.log.exists {
        s.push_str(
            "  (no log; enable guards.decisionLog / LOOM_GUARD_DECISION_LOG to record decisions)\n",
        );
    } else {
        let _ = writeln!(
            s,
            "  {} decisions, {} malformed line(s) skipped",
            r.log.total, r.log.malformed
        );
        let _ = writeln!(
            s,
            "  {:<44} {:>5} {:>5} {:>7} {:>7}",
            "rule", "ask", "deny", "ask-7d", "deny-7d"
        );
        for (rule, c) in &r.log.by_rule {
            let _ = writeln!(
                s,
                "  {:<44} {:>5} {:>5} {:>7} {:>7}",
                rule, c.ask, c.deny, c.ask_7d, c.deny_7d
            );
        }
    }
    s.push('\n');
    if r.warnings.is_empty() {
        s.push_str("No misconfiguration warnings.\n");
    }
    for w in &r.warnings {
        let _ = writeln!(s, "WARNING {}: {}", w.headline, w.detail);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn now() -> DateTime<Utc> {
        parse_ts("2026-10-05T12:00:00Z").unwrap()
    }

    fn put(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn inputs(repo: &Path, home: &Path) -> Inputs {
        Inputs {
            repo_root: repo.into(),
            home: Some(home.into()),
            now: now(),
            ask_threshold: DEFAULT_ASK_THRESHOLD,
            log_path: None,
        }
    }

    fn ask_line(tag: &str, ts: &str) -> String {
        format!("{{\"ts\":\"{ts}\",\"decision\":\"ask\",\"pattern\":\"{tag}\",\"tier\":\"ask\",\"command\":\"x\"}}\n")
    }

    fn deny_line(tag: &str, ts: &str) -> String {
        format!("{{\"ts\":\"{ts}\",\"decision\":\"deny\",\"pattern\":\"{tag}\",\"tier\":\"catastrophic\",\"command\":\"x\"}}\n")
    }

    fn find<'a>(r: &'a Report, key: &str) -> &'a CategoryStatus {
        r.categories.iter().find(|c| c.key == key).unwrap()
    }

    #[test]
    fn precedence_env_beats_config_beats_default() {
        let (repo, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        put(
            repo.path(),
            ".loom/config.json",
            r#"{"guards":{"cloudCli":false,"sqlDdl":false,"stashScope":false}}"#,
        );
        put(
            home.path(),
            ".claude/settings.json",
            r#"{"env":{"LOOM_GUARD_CLOUD":"1","LOOM_RM_SCOPE":"off"}}"#,
        );
        put(
            repo.path(),
            ".claude/settings.local.json",
            r#"{"env":{"LOOM_GUARD_CLOUD":"0"}}"#,
        );
        let r = collect(&inputs(repo.path(), home.path()));
        let cloud = find(&r, "cloudCli");
        assert_eq!((cloud.value.as_str(), cloud.source), ("false", Source::Env));
        assert!(cloud
            .origin
            .as_deref()
            .unwrap()
            .contains("settings.local.json"));
        let sql = find(&r, "sqlDdl");
        assert_eq!((sql.value.as_str(), sql.source), ("false", Source::Config));
        let rm = find(&r, "rmScope");
        assert_eq!((rm.value.as_str(), rm.source), ("off", Source::Env));
        let wt = find(&r, "worktreeIsolation");
        assert_eq!((wt.value.as_str(), wt.source), ("true", Source::Default));
    }

    #[test]
    fn invalid_env_value_falls_through() {
        let (repo, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        put(repo.path(), ".loom/config.json", r#"{"guards":{"cloudCli":false}}"#);
        put(repo.path(), ".claude/settings.json", r#"{"env":{"LOOM_GUARD_CLOUD":"maybe"}}"#);
        let r = collect(&inputs(repo.path(), home.path()));
        assert_eq!(find(&r, "cloudCli").source, Source::Config);
    }

    #[test]
    fn wiring_detection_and_double_wiring_warning() {
        let (repo, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let wired = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":".loom/hooks/guard-destructive.sh"}]}]}}"#;
        put(repo.path(), ".claude/settings.json", wired);
        let r = collect(&inputs(repo.path(), home.path()));
        assert!(r.repo_wiring.wired && !r.global_wiring.wired);
        assert!(r.warnings.iter().all(|w| w.headline != "DOUBLE-WIRED-HOOK"));
        put(home.path(), ".claude/settings.json", wired);
        let r = collect(&inputs(repo.path(), home.path()));
        assert!(r.global_wiring.wired);
        assert!(r.warnings.iter().any(|w| w.headline == "DOUBLE-WIRED-HOOK"));
    }

    #[test]
    fn log_summary_counts_by_rule_and_skips_malformed() {
        let mut log = String::new();
        log += &ask_line("cloud-cli:aws", "2026-10-04T00:00:00Z");
        log += &ask_line("cloud-cli:aws", "2026-08-01T00:00:00Z");
        log += &deny_line("rm-scope-unresolved-var", "2026-10-04T00:00:00Z");
        log += "not json\n{\"decision\":\"ask\"}\n";
        let (by, total, bad) = summarise_log(&log, now());
        assert_eq!((total, bad), (3, 2));
        let c = &by["cloud-cli:aws"];
        assert_eq!((c.ask, c.ask_7d, c.deny), (2, 1, 0));
        assert_eq!(by["rm-scope-unresolved-var"].deny_7d, 1);
    }

    #[test]
    fn repeated_asks_warning_names_the_config_key() {
        let (repo, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let log: String = (0..3)
            .map(|_| ask_line("cloud-cli:aws", "2026-10-04T00:00:00Z"))
            .collect();
        put(repo.path(), ".loom/logs/guard-decisions.log", &log);
        let r = collect(&inputs(repo.path(), home.path()));
        let w = r
            .warnings
            .iter()
            .find(|w| w.headline == "REPEATED-ASKS-UNCONFIGURED")
            .expect("warning");
        assert!(w.detail.contains("guards.cloudCli"));
        assert!(render(&r).contains("guards.cloudCli"));

        // Below threshold, or once configured: silent.
        let fewer: String = (0..2)
            .map(|_| ask_line("cloud-cli:aws", "2026-10-04T00:00:00Z"))
            .collect();
        put(repo.path(), ".loom/logs/guard-decisions.log", &fewer);
        assert!(collect(&inputs(repo.path(), home.path()))
            .warnings
            .is_empty());
        put(repo.path(), ".loom/logs/guard-decisions.log", &log);
        put(repo.path(), ".loom/config.json", r#"{"guards":{"cloudCli":false}}"#);
        assert!(collect(&inputs(repo.path(), home.path()))
            .warnings
            .is_empty());
    }

    #[test]
    fn shell_rc_only_env_warns_unless_in_settings_env() {
        let (repo, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        put(
            home.path(),
            ".bashrc",
            "# LOOM_GUARD_SQL=0\nexport LOOM_GUARD_CLOUD=0\nexport PATH=/x\n",
        );
        let r = collect(&inputs(repo.path(), home.path()));
        let ws: Vec<_> = r
            .warnings
            .iter()
            .filter(|w| w.headline == "SHELL-RC-ONLY-ENV")
            .collect();
        assert_eq!(ws.len(), 1);
        assert!(ws[0].detail.contains("LOOM_GUARD_CLOUD"));
        put(home.path(), ".claude/settings.json", r#"{"env":{"LOOM_GUARD_CLOUD":"0"}}"#);
        assert!(collect(&inputs(repo.path(), home.path()))
            .warnings
            .is_empty());
    }

    #[test]
    fn high_unresolved_var_deny_rate_warns() {
        let (repo, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut log = String::new();
        for _ in 0..8 {
            log += &deny_line("rm-scope-unresolved-var", "2026-10-04T00:00:00Z");
        }
        for _ in 0..4 {
            log += &deny_line("rm-protected-path", "2026-10-04T00:00:00Z");
        }
        put(repo.path(), ".loom/logs/guard-decisions.log", &log);
        let r = collect(&inputs(repo.path(), home.path()));
        let w = r
            .warnings
            .iter()
            .find(|w| w.headline == "HIGH-UNRESOLVED-VAR-DENY-RATE")
            .expect("warning");
        assert!(w.detail.contains("LOOM_GUARD_SCRATCH_ROOT"));

        // Too small a sample: silent.
        put(
            repo.path(),
            ".loom/logs/guard-decisions.log",
            &deny_line("rm-scope-unresolved-var", "2026-10-04T00:00:00Z"),
        );
        assert!(collect(&inputs(repo.path(), home.path()))
            .warnings
            .is_empty());
    }

    #[test]
    fn tag_mapping_matches_hook_categories() {
        assert_eq!(category_for_tag("cloud-cli:aws"), Some("cloudCli"));
        assert_eq!(category_for_tag("force-op:detached"), Some("forceScope"));
        assert_eq!(category_for_tag("stash-scope:main-checkout"), Some("stashScope"));
        assert_eq!(category_for_tag("rm-protected-path"), None);
        assert_eq!(category_for_tag("catastrophic:x"), None);
    }
}
