//! `autonomous.eta` configuration. Precedence: env > config > default.
//!
//! | key | env | default |
//! |---|---|---|
//! | `enabled` | `LOOM_ETA_ENABLED` | `true` (operator decision on #9289) |
//! | `dryRun` | `LOOM_ETA_DRY_RUN` | `false`: compute and log, enqueue nothing |
//! | `refreshSecs` | `LOOM_ETA_REFRESH_SECS` | `300` |
//! | `current.start` / `current.finish` / `current.land` | — | `start-v1` / `finish-v1` / `land-v1` |

use super::Kind;
use std::path::Path;

/// Default refresh cadence for an unchanged estimate.
pub const DEFAULT_REFRESH_SECS: u64 = 300;

/// Shortest refresh accepted, so a misconfiguration cannot flood the queue.
pub const MIN_REFRESH_SECS: u64 = 60;

/// Resolved ETA settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EtaConfig {
    /// Run the tracker at all.
    pub enabled: bool,
    /// Compute and log every would-be record, enqueue none.
    pub dry_run: bool,
    /// Refresh an unchanged estimate after this many seconds.
    pub refresh_secs: u64,
    /// Configured `current` heuristic for `start`.
    pub current_start: Option<String>,
    /// Configured `current` heuristic for `finish`.
    pub current_finish: Option<String>,
    /// Configured `current` heuristic for `land`.
    pub current_land: Option<String>,
}

impl Default for EtaConfig {
    fn default() -> Self {
        EtaConfig {
            enabled: true,
            dry_run: false,
            refresh_secs: DEFAULT_REFRESH_SECS,
            current_start: None,
            current_finish: None,
            current_land: None,
        }
    }
}

impl EtaConfig {
    /// The configured current heuristic id for `kind`, if any.
    #[must_use]
    pub fn current(&self, kind: Kind) -> Option<&str> {
        match kind {
            Kind::Start => self.current_start.as_deref(),
            Kind::Finish => self.current_finish.as_deref(),
            Kind::Land => self.current_land.as_deref(),
        }
    }
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Resolve from an effective config value and an env lookup (pure).
#[must_use]
pub fn resolve(config: &serde_json::Value, env: impl Fn(&str) -> Option<String>) -> EtaConfig {
    let block = crate::config_resolver::get_path(config, "autonomous.eta");
    let get = |key: &str| block.and_then(|b| b.get(key));
    let mut resolved = EtaConfig::default();
    if let Some(v) = get("enabled").and_then(serde_json::Value::as_bool) {
        resolved.enabled = v;
    }
    if let Some(v) = get("dryRun").and_then(serde_json::Value::as_bool) {
        resolved.dry_run = v;
    }
    if let Some(v) = get("refreshSecs").and_then(serde_json::Value::as_u64) {
        resolved.refresh_secs = v;
    }
    let current = get("current");
    let id = |kind: &str| {
        current
            .and_then(|c| c.get(kind))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    resolved.current_start = id("start");
    resolved.current_finish = id("finish");
    resolved.current_land = id("land");

    if let Some(v) = env("LOOM_ETA_ENABLED").as_deref().and_then(parse_bool) {
        resolved.enabled = v;
    }
    if let Some(v) = env("LOOM_ETA_DRY_RUN").as_deref().and_then(parse_bool) {
        resolved.dry_run = v;
    }
    if let Some(v) = env("LOOM_ETA_REFRESH_SECS").and_then(|s| s.trim().parse::<u64>().ok()) {
        resolved.refresh_secs = v;
    }
    resolved.refresh_secs = resolved.refresh_secs.max(MIN_REFRESH_SECS);
    resolved
}

/// Read the configuration for `workspace_root` from its effective config and
/// the process environment.
#[must_use]
pub fn read(workspace_root: &Path) -> EtaConfig {
    let effective = crate::config_resolver::resolve_effective_config(workspace_root);
    resolve(&effective, |key| std::env::var(key).ok())
}

/// The config file a promotion writes to: the **host-local**, gitignored tier
/// ([`crate::config_resolver::LOCAL_CONFIG_REL`]).
///
/// Highest precedence, so the flip takes effect immediately; untracked, so a
/// daemon can never dirty a worktree by deciding one. The evidence behind a
/// promotion is this host's own history and live pairs (#9343), so host-local
/// is also the honest scope — rolling it fleet-wide is an operator copying the
/// key into the committed config, not one host deciding for all of them.
#[must_use]
pub fn promotion_config_path(workspace_root: &Path) -> std::path::PathBuf {
    workspace_root.join(crate::config_resolver::LOCAL_CONFIG_REL)
}

/// Set `autonomous.eta.current.<kind>` to `heuristic` in the JSON config at
/// `path`, creating the file and its parents when absent.
///
/// A read-modify-write over the **whole** document: every other key is
/// preserved byte-for-value, only the one leaf changes. A file that exists but
/// does not parse as a JSON object is an error rather than something to
/// overwrite — clobbering an operator's config to land a promotion would be a
/// far worse outcome than not promoting.
///
/// Written through a temp file and renamed, so a crash mid-write cannot leave
/// a half-written config behind.
///
/// # Errors
///
/// `path` exists and is not a readable JSON object, its parent could not be
/// created, or the write/rename failed.
pub fn promote(path: &Path, kind: Kind, heuristic: &str) -> std::io::Result<()> {
    let mut root: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => serde_json::json!({}),
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: not valid JSON, refusing to overwrite it: {e}", path.display()),
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e),
    };
    if !root.is_object() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: not a JSON object, refusing to overwrite it", path.display()),
        ));
    }
    let mut cursor = &mut root;
    for segment in ["autonomous", "eta", "current"] {
        let object = cursor.as_object_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: autonomous.eta.current is not an object", path.display()),
            )
        })?;
        cursor = object
            .entry(segment)
            .or_insert_with(|| serde_json::json!({}));
    }
    cursor
        .as_object_mut()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: autonomous.eta.current is not an object", path.display()),
            )
        })?
        .insert(kind.as_str().to_string(), serde_json::Value::String(heuristic.to_string()));

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&root).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{text}\n"))?;
    std::fs::rename(&tmp, path)
}
