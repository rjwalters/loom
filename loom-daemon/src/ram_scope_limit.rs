//! Where the per-agent-scope `MemoryMax` settings come from (#12246).
//!
//! `loom-daemon ram-scope-limit` (the `cli::ram_scope_limit` subcommand,
//! #11094) decides the `MemoryMax` that `spawn-claude.sh` puts on each agent's
//! systemd scope. Its three knobs used to be env-only, so a fleet could change
//! them only by editing each host's service unit. They are now layered config
//! keys like every other `autonomous.*` tuning knob, and this module resolves
//! them:
//!
//! | Env (wins when set and non-empty) | Config key | Default |
//! |---|---|---|
//! | [`MAX_ENV`] | [`MAX_KEY`]: `"off"`, `"auto"`, or a positive MiB value (a JSON number is accepted) | `auto` |
//! | [`PCT_ENV`] | [`PCT_KEY`]: multiple of the observed peak, `>= 100` | `200` |
//! | [`MIN_MB_ENV`] | [`MIN_MB_KEY`]: floor for a derived limit, MiB | `4096` |
//!
//! Precedence per key: env, then the config tiers highest first, then the
//! default. A value that does not parse at a level falls through to the next
//! level, so garbage anywhere behaves as if that level were unset.
//!
//! # Which tiers
//!
//! The subcommand runs per spawn with `--workspace` = the agent's repo, but the
//! fleet sync writes the host-local tier (`.loom-local/local.json`) into the
//! **daemon's own** workspace only (`fleet_store::render`). A repo's own tiers
//! alone would therefore never see a host override on a multi-repo host. The
//! daemon exports its workspace to every child as [`DAEMON_WORKSPACE_ENV`]
//! ([`export_daemon_workspace`]), and [`tier_paths`] slots that workspace's
//! host-local file in directly below the repo's own host-local file:
//!
//! 1. `<repo>/.loom-local/local.json` (a hand-made per-repo host override)
//! 2. `<daemon workspace>/.loom-local/local.json` (the fleet's host tier)
//! 3. `<repo>/.loom-local/fleet-repo.json`
//! 4. `<repo>/.loom-project/project.json`
//! 5. `<repo>/.loom/config.json`
//! 6. the machine tier ([`crate::config_resolver::private_defaults_path`])
//!
//! Without the env var (an older daemon, or a manual run) the list is the
//! repo's own tiers, exactly [`crate::config_resolver::tier_paths_by_precedence`].
//!
//! Nothing is cached: every call reads the files, so a change applies to the
//! next spawn without a daemon restart (classified `Live` in
//! `fleet_store::reload`).

use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config_resolver::{get_path, soft_read_json_object, LOCAL_CONFIG_REL};

/// Env override for [`MAX_KEY`].
pub const MAX_ENV: &str = "LOOM_SWEEP_MEMORY_MAX";
/// Env override for [`PCT_KEY`].
pub const PCT_ENV: &str = "LOOM_SWEEP_MEMORY_MAX_PCT";
/// Env override for [`MIN_MB_KEY`].
pub const MIN_MB_ENV: &str = "LOOM_SWEEP_MEMORY_MAX_MIN_MB";
/// `"off"`, `"auto"`, or a positive MiB value.
pub const MAX_KEY: &str = "autonomous.agentMemoryMax";
/// Multiple of the observed peak, in percent.
pub const PCT_KEY: &str = "autonomous.agentMemoryMaxPct";
/// Floor for a derived limit, in MiB.
pub const MIN_MB_KEY: &str = "autonomous.agentMemoryMaxMinMb";
/// The daemon's own workspace, exported to every child it spawns.
pub const DAEMON_WORKSPACE_ENV: &str = "LOOM_DAEMON_WORKSPACE";

/// Default multiple of the observed peak.
pub const DEFAULT_PCT: u64 = 200;
/// Default floor for a derived limit, MiB.
pub const DEFAULT_FLOOR_MB: u64 = 4096;

/// What [`MAX_KEY`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxMode {
    /// No limit.
    Off,
    /// Derive from the repo's observed peak (no history = no limit).
    Auto,
    /// An explicit limit in MiB; no history needed.
    Mib(u64),
}

/// Which level supplied a setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The named env var.
    Env(&'static str),
    /// The named key in this tier file.
    Config(&'static str, PathBuf),
    /// Nothing set (or nothing parsable): the built-in default.
    Default,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(name) => write!(f, "env {name}"),
            Self::Config(key, path) => write!(f, "config {key} in {}", path.display()),
            Self::Default => f.write_str("default"),
        }
    }
}

/// One resolved setting and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting<T> {
    pub value: T,
    pub source: Source,
}

/// The three resolved settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub max: Setting<MaxMode>,
    pub pct: Setting<u64>,
    pub floor_mb: Setting<u64>,
}

impl Default for Resolved {
    fn default() -> Self {
        Self {
            max: Setting {
                value: MaxMode::Auto,
                source: Source::Default,
            },
            pct: Setting {
                value: DEFAULT_PCT,
                source: Source::Default,
            },
            floor_mb: Setting {
                value: DEFAULT_FLOOR_MB,
                source: Source::Default,
            },
        }
    }
}

fn parse_max_str(raw: &str) -> Option<MaxMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "0" | "off" | "false" | "no" => Some(MaxMode::Off),
        "auto" => Some(MaxMode::Auto),
        other => other
            .parse::<u64>()
            .ok()
            .filter(|&v| v > 0)
            .map(MaxMode::Mib),
    }
}

fn parse_max(v: &Value) -> Option<MaxMode> {
    match v {
        Value::String(s) => parse_max_str(s),
        Value::Number(n) => n.as_u64().map(|v| {
            if v == 0 {
                MaxMode::Off
            } else {
                MaxMode::Mib(v)
            }
        }),
        Value::Bool(false) => Some(MaxMode::Off),
        _ => None,
    }
}

fn parse_u64(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

fn parse_pct(v: &Value) -> Option<u64> {
    parse_u64(v).filter(|&p| p >= 100)
}

/// The tier files to search, highest precedence first (see the module doc).
#[must_use]
pub fn tier_paths(workspace: &Path, daemon_workspace: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = crate::config_resolver::tier_paths_by_precedence(workspace);
    if let Some(daemon) = daemon_workspace.filter(|d| !d.as_os_str().is_empty()) {
        let host = daemon.join(LOCAL_CONFIG_REL);
        if !paths.contains(&host) {
            paths.insert(1, host);
        }
    }
    paths
}

fn pick<T>(
    env: &dyn Fn(&str) -> Option<String>,
    tiers: &[(PathBuf, Value)],
    env_name: &'static str,
    key: &'static str,
    parse: fn(&Value) -> Option<T>,
    default: T,
) -> Setting<T> {
    if let Some(v) = env(env_name)
        .filter(|v| !v.trim().is_empty())
        .and_then(|raw| parse(&Value::String(raw)))
    {
        return Setting {
            value: v,
            source: Source::Env(env_name),
        };
    }
    for (path, tree) in tiers {
        if let Some(v) = get_path(tree, key).and_then(parse) {
            return Setting {
                value: v,
                source: Source::Config(key, path.clone()),
            };
        }
    }
    Setting {
        value: default,
        source: Source::Default,
    }
}

/// Resolve the three settings for an agent in `workspace`. `env` is the
/// environment lookup (injected for tests); the daemon workspace comes from
/// [`DAEMON_WORKSPACE_ENV`] through it.
#[must_use]
pub fn resolve(workspace: &Path, env: &dyn Fn(&str) -> Option<String>) -> Resolved {
    let daemon = env(DAEMON_WORKSPACE_ENV).map(PathBuf::from);
    let tiers: Vec<(PathBuf, Value)> = tier_paths(workspace, daemon.as_deref())
        .into_iter()
        .map(|p| {
            let tree = soft_read_json_object(&p);
            (p, tree)
        })
        .collect();
    Resolved {
        max: pick(env, &tiers, MAX_ENV, MAX_KEY, parse_max, MaxMode::Auto),
        pct: pick(env, &tiers, PCT_ENV, PCT_KEY, parse_pct, DEFAULT_PCT),
        floor_mb: pick(env, &tiers, MIN_MB_ENV, MIN_MB_KEY, parse_u64, DEFAULT_FLOOR_MB),
    }
}

/// [`resolve`] against the process environment.
#[must_use]
pub fn resolve_process(workspace: &Path) -> Resolved {
    resolve(workspace, &|k| std::env::var(k).ok())
}

/// Export [`DAEMON_WORKSPACE_ENV`] for every child of the daemon: the
/// daemon's `LOOM_WORKSPACE`, else its cwd. Spawn paths overwrite
/// `LOOM_WORKSPACE` with the target repo, so this is how a per-spawn helper
/// finds the host-local tier the fleet sync writes. An already-exported value
/// wins. Call once, early in daemon startup.
pub fn export_daemon_workspace() {
    if std::env::var_os(DAEMON_WORKSPACE_ENV).is_some_and(|v| !v.is_empty()) {
        return;
    }
    let root = std::env::var_os("LOOM_WORKSPACE")
        .filter(|w| !w.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    if let Some(root) = root {
        std::env::set_var(DAEMON_WORKSPACE_ENV, root);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    /// A repo and a separate daemon workspace. The machine tier is absent
    /// under `cfg(test)` unless a test names one; `serial` keeps the tests
    /// that do away from these.
    struct Fixture {
        _tmp: tempfile::TempDir,
        repo: PathBuf,
        daemon: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let daemon = tmp.path().join("daemon");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&daemon).unwrap();
        Fixture {
            _tmp: tmp,
            repo,
            daemon,
        }
    }

    impl Fixture {
        fn env<'a>(&'a self, extra: &[(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> {
            let mut pairs = vec![(DAEMON_WORKSPACE_ENV, self.daemon.to_str().unwrap())];
            pairs.extend_from_slice(extra);
            env_of(&pairs)
        }
    }

    #[test]
    #[serial_test::serial]
    fn nothing_set_is_the_unchanged_default() {
        let f = fixture();
        assert_eq!(resolve(&f.repo, &f.env(&[])), Resolved::default());
    }

    #[test]
    #[serial_test::serial]
    fn config_off_disables() {
        let f = fixture();
        let cfg = f.repo.join(".loom/config.json");
        write(&cfg, r#"{"autonomous": {"agentMemoryMax": "off"}}"#);
        let r = resolve(&f.repo, &f.env(&[]));
        assert_eq!(r.max.value, MaxMode::Off);
        assert_eq!(r.max.source, Source::Config(MAX_KEY, cfg));
    }

    #[test]
    #[serial_test::serial]
    fn env_beats_config() {
        let f = fixture();
        write(
            &f.daemon.join(LOCAL_CONFIG_REL),
            r#"{"autonomous": {"agentMemoryMax": "off", "agentMemoryMaxPct": 300}}"#,
        );
        let r = resolve(&f.repo, &f.env(&[(MAX_ENV, "8000"), (PCT_ENV, "150")]));
        assert_eq!(
            r.max,
            Setting {
                value: MaxMode::Mib(8000),
                source: Source::Env(MAX_ENV)
            }
        );
        assert_eq!(
            r.pct,
            Setting {
                value: 150,
                source: Source::Env(PCT_ENV)
            }
        );
        // An empty env var is unset, not a value.
        let r = resolve(&f.repo, &f.env(&[(MAX_ENV, "")]));
        assert_eq!(r.max.value, MaxMode::Off);
    }

    #[test]
    #[serial_test::serial]
    fn host_tier_beats_workspace_tier() {
        let f = fixture();
        write(&f.repo.join(".loom/config.json"), r#"{"autonomous": {"agentMemoryMax": 6000}}"#);
        let host = f.daemon.join(LOCAL_CONFIG_REL);
        write(
            &host,
            r#"{"autonomous": {"agentMemoryMax": "off", "agentMemoryMaxMinMb": "8192"}}"#,
        );
        let r = resolve(&f.repo, &f.env(&[]));
        assert_eq!(
            r.max,
            Setting {
                value: MaxMode::Off,
                source: Source::Config(MAX_KEY, host.clone())
            }
        );
        assert_eq!(
            r.floor_mb,
            Setting {
                value: 8192,
                source: Source::Config(MIN_MB_KEY, host)
            }
        );
        // Without the daemon workspace only the repo's own tiers apply.
        let r = resolve(&f.repo, &env_of(&[]));
        assert_eq!(r.max.value, MaxMode::Mib(6000));
        // The repo's own host-local file is more specific still.
        write(&f.repo.join(LOCAL_CONFIG_REL), r#"{"autonomous": {"agentMemoryMax": "auto"}}"#);
        assert_eq!(resolve(&f.repo, &f.env(&[])).max.value, MaxMode::Auto);
    }

    #[test]
    #[serial_test::serial]
    fn explicit_mib_from_config() {
        let f = fixture();
        write(
            &f.daemon.join(LOCAL_CONFIG_REL),
            r#"{"autonomous": {"agentMemoryMax": "12288"}}"#,
        );
        assert_eq!(resolve(&f.repo, &f.env(&[])).max.value, MaxMode::Mib(12288));
        write(&f.daemon.join(LOCAL_CONFIG_REL), r#"{"autonomous": {"agentMemoryMax": 12288}}"#);
        assert_eq!(resolve(&f.repo, &f.env(&[])).max.value, MaxMode::Mib(12288));
    }

    #[test]
    #[serial_test::serial]
    fn garbage_falls_back_to_the_default() {
        let f = fixture();
        write(
            &f.daemon.join(LOCAL_CONFIG_REL),
            r#"{"autonomous": {"agentMemoryMax": "lots", "agentMemoryMaxPct": 50,
                "agentMemoryMaxMinMb": [1]}}"#,
        );
        let env = f.env(&[(MAX_ENV, "junk"), (PCT_ENV, "x"), (MIN_MB_ENV, "-1")]);
        assert_eq!(resolve(&f.repo, &env), Resolved::default());
        // Garbage at a higher tier falls through to a valid lower one.
        write(
            &f.repo.join(".loom/config.json"),
            r#"{"autonomous": {"agentMemoryMaxPct": 250}}"#,
        );
        assert_eq!(resolve(&f.repo, &env).pct.value, 250);
    }

    #[test]
    fn max_values_parse() {
        for v in ["0", "off", "OFF", "false", "no", " No "] {
            assert_eq!(parse_max_str(v), Some(MaxMode::Off), "{v}");
        }
        assert_eq!(parse_max_str("Auto"), Some(MaxMode::Auto));
        assert_eq!(parse_max_str("4096"), Some(MaxMode::Mib(4096)));
        assert_eq!(parse_max(&serde_json::json!(0)), Some(MaxMode::Off));
        assert_eq!(parse_max(&serde_json::json!(false)), Some(MaxMode::Off));
        assert_eq!(parse_max(&serde_json::json!(true)), None);
        assert_eq!(parse_max(&serde_json::json!(-5)), None);
    }
}
