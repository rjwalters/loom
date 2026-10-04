//! `autonomous.eta` configuration. Precedence: env > config > default.
//!
//! | key | env | default |
//! |---|---|---|
//! | `enabled` | `LOOM_ETA_ENABLED` | `true` (operator decision on #9289) |
//! | `dryRun` | `LOOM_ETA_DRY_RUN` | `false`: compute and log, enqueue nothing |
//! | `refreshSecs` | `LOOM_ETA_REFRESH_SECS` | `300` |
//! | `historyScope` | `LOOM_ETA_HISTORY_SCOPE` | `augment` (#9343) |
//! | `fit.enabled` | `LOOM_ETA_FIT_ENABLED` | `true`: the daily refit (#10245) |
//! | `current.start` / `current.finish` / `current.land` | — | `start-v1` / `finish-v1` / `land-v1` |
//!
//! `autonomous.eta.fleetRefresh.*` (#10263) — the daemon task that backfills
//! and refreshes the fleet snapshots (`observability::eta_fleet_refresh`).
//! Resolved once at spawn, so a change needs a daemon restart.
//!
//! | key | env | default |
//! |---|---|---|
//! | `enabled` | `LOOM_ETA_FLEET_REFRESH_ENABLED` | `true` |
//! | `intervalSecs` | `LOOM_ETA_FLEET_REFRESH_INTERVAL_SECS` | `3600` (min `900`) |
//! | `maxCallsPerCycle` | `LOOM_ETA_FLEET_REFRESH_MAX_CALLS` | `300` |
//! | `backfillMaxCallsPerCycle` | `LOOM_ETA_FLEET_REFRESH_BACKFILL_MAX_CALLS` | `1500` |
//! | `reserveCalls` | `LOOM_ETA_FLEET_REFRESH_RESERVE` | `1500` |
//! | `backfillDays` | `LOOM_ETA_FLEET_REFRESH_BACKFILL_DAYS` | `21` (min `fit::WINDOW_DAYS + 1`) |

use super::Kind;
use std::path::Path;

/// Default refresh cadence for an unchanged estimate.
pub const DEFAULT_REFRESH_SECS: u64 = 300;

/// Shortest refresh accepted, so a misconfiguration cannot flood the queue.
pub const MIN_REFRESH_SECS: u64 = 60;

/// Which history an estimate reads (#9343).
///
/// The default is [`HistoryScopeMode::Augment`], a no-op until a fleet
/// snapshot exists. Since #10263 the daemon's own fleet refresh task builds
/// one by default (`autonomous.eta.fleetRefresh.enabled`), so on a host with
/// reader Apps live estimates switch to `scope = fleet` once its first
/// backfill publishes; [`HistoryScopeMode::Local`] is the escape hatch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HistoryScopeMode {
    /// This host's journals only. Pre-#9343 behaviour, kept as an escape
    /// hatch.
    Local,
    /// The cached fleet snapshot **plus** this host's journals, reported as
    /// `scope = fleet`. The daemon default: the forge cannot see in-sweep
    /// phases, so dropping the local journals would cost every `finish`
    /// estimate.
    #[default]
    Augment,
    /// The cached fleet snapshot **only**, so the estimate is a pure function
    /// of a named snapshot and two hosts holding the same snapshot id agree
    /// byte for byte. Falls back to local when no snapshot exists — a missing
    /// cache must not silently become "no history".
    Fleet,
}

impl HistoryScopeMode {
    /// Parse the config / env / CLI vocabulary. `None` for anything else, so a
    /// typo leaves the default in force rather than picking a scope at random.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "local" => Some(HistoryScopeMode::Local),
            "augment" => Some(HistoryScopeMode::Augment),
            "fleet" => Some(HistoryScopeMode::Fleet),
            _ => None,
        }
    }

    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HistoryScopeMode::Local => "local",
            HistoryScopeMode::Augment => "augment",
            HistoryScopeMode::Fleet => "fleet",
        }
    }
}

/// Default fleet refresh cadence (#10263).
pub const DEFAULT_FLEET_REFRESH_INTERVAL_SECS: u64 = 3600;

/// Shortest fleet refresh cadence accepted.
pub const MIN_FLEET_REFRESH_INTERVAL_SECS: u64 = 900;

/// Default per-cycle forge-call budget for refresh passes.
pub const DEFAULT_FLEET_REFRESH_MAX_CALLS: u64 = 300;

/// Default per-cycle forge-call budget for backfill passes.
pub const DEFAULT_FLEET_REFRESH_BACKFILL_MAX_CALLS: u64 = 1500;

/// Default reserve floor: stop a reader App's repos for the cycle once a
/// response reports fewer core calls than this remaining.
pub const DEFAULT_FLEET_REFRESH_RESERVE: u64 = 1500;

/// Default backfill depth, in days.
pub const DEFAULT_FLEET_REFRESH_BACKFILL_DAYS: i64 = 21;

/// Shallowest backfill accepted: one day more than the daily fit's window,
/// so a fresh host's first fit sees a whole window of history.
pub const MIN_FLEET_REFRESH_BACKFILL_DAYS: i64 = super::fit::WINDOW_DAYS + 1;

/// `autonomous.eta.fleetRefresh` (#10263): the daemon task that keeps the
/// fleet snapshots fresh ahead of the daily fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetRefreshConfig {
    /// Run the task at all (also requires `autonomous.eta.enabled`).
    pub enabled: bool,
    /// Seconds between cycles.
    pub interval_secs: u64,
    /// Forge calls per cycle for refresh passes, host-wide.
    pub max_calls_per_cycle: u64,
    /// Forge calls per cycle for backfill passes, host-wide.
    pub backfill_max_calls_per_cycle: u64,
    /// The reserve floor, in remaining core calls.
    pub reserve_calls: u64,
    /// How far back a backfill reaches, in days.
    pub backfill_days: i64,
    /// The SigNoz in-sweep half (#9758), refreshed on the same cadence.
    pub signoz: FleetSignozConfig,
}

impl Default for FleetRefreshConfig {
    fn default() -> Self {
        FleetRefreshConfig {
            enabled: true,
            interval_secs: DEFAULT_FLEET_REFRESH_INTERVAL_SECS,
            max_calls_per_cycle: DEFAULT_FLEET_REFRESH_MAX_CALLS,
            backfill_max_calls_per_cycle: DEFAULT_FLEET_REFRESH_BACKFILL_MAX_CALLS,
            reserve_calls: DEFAULT_FLEET_REFRESH_RESERVE,
            backfill_days: DEFAULT_FLEET_REFRESH_BACKFILL_DAYS,
            signoz: FleetSignozConfig::default(),
        }
    }
}

/// Default rows per SigNoz page.
pub const DEFAULT_FLEET_SIGNOZ_PAGE_SIZE: u32 = 500;

/// Default page ceiling per repo per cycle. A fetch that reaches it is not
/// published: a snapshot is all of the window or nothing.
pub const DEFAULT_FLEET_SIGNOZ_MAX_PAGES: u32 = 200;

/// `autonomous.eta.fleetRefresh.signoz` (#9758): where the refresh task reads
/// the fleet's `sweep.outcome` records back from.
///
/// **Off by default.** It needs an endpoint and, usually, a credential that
/// only an operator can provision; with it off nothing is read and no SigNoz
/// snapshot is written, so every estimate is exactly what it was before.
/// `credentialFile` is a **path** to an owner-only file outside every
/// repository (`credential-storage.md`), never the secret itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetSignozConfig {
    /// Refresh SigNoz snapshots at all.
    pub enabled: bool,
    /// The telemetry store's ClickHouse HTTP endpoint (`http(s)://host:port`).
    pub endpoint: Option<String>,
    /// The ClickHouse user, when the endpoint requires one.
    pub user: Option<String>,
    /// Owner-only file holding that user's password.
    pub credential_file: Option<std::path::PathBuf>,
    /// Rows per page.
    pub page_size: u32,
    /// Pages per repo per cycle.
    pub max_pages: u32,
}

impl Default for FleetSignozConfig {
    fn default() -> Self {
        FleetSignozConfig {
            enabled: false,
            endpoint: None,
            user: None,
            credential_file: None,
            page_size: DEFAULT_FLEET_SIGNOZ_PAGE_SIZE,
            max_pages: DEFAULT_FLEET_SIGNOZ_MAX_PAGES,
        }
    }
}

/// Resolved ETA settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EtaConfig {
    /// Run the tracker at all.
    pub enabled: bool,
    /// Compute and log every would-be record, enqueue none.
    pub dry_run: bool,
    /// Refresh an unchanged estimate after this many seconds.
    pub refresh_secs: u64,
    /// Which history an estimate reads (#9343).
    pub history_scope: HistoryScopeMode,
    /// Configured `current` heuristic for `start`.
    pub current_start: Option<String>,
    /// Configured `current` heuristic for `finish`.
    pub current_finish: Option<String>,
    /// Configured `current` heuristic for `land`.
    pub current_land: Option<String>,
    /// Run the daily refit (#10245); only when [`Self::enabled`] too.
    pub fit_enabled: bool,
    /// The fleet snapshot refresh task (#10263).
    pub fleet_refresh: FleetRefreshConfig,
}

impl Default for EtaConfig {
    fn default() -> Self {
        EtaConfig {
            enabled: true,
            dry_run: false,
            refresh_secs: DEFAULT_REFRESH_SECS,
            history_scope: HistoryScopeMode::default(),
            current_start: None,
            current_finish: None,
            current_land: None,
            fit_enabled: true,
            fleet_refresh: FleetRefreshConfig::default(),
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
    if let Some(v) = get("historyScope")
        .and_then(serde_json::Value::as_str)
        .and_then(HistoryScopeMode::parse)
    {
        resolved.history_scope = v;
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
    if let Some(v) = get("fit")
        .and_then(|f| f.get("enabled"))
        .and_then(serde_json::Value::as_bool)
    {
        resolved.fit_enabled = v;
    }

    if let Some(v) = env("LOOM_ETA_ENABLED").as_deref().and_then(parse_bool) {
        resolved.enabled = v;
    }
    if let Some(v) = env("LOOM_ETA_DRY_RUN").as_deref().and_then(parse_bool) {
        resolved.dry_run = v;
    }
    if let Some(v) = env("LOOM_ETA_REFRESH_SECS").and_then(|s| s.trim().parse::<u64>().ok()) {
        resolved.refresh_secs = v;
    }
    if let Some(v) = env("LOOM_ETA_HISTORY_SCOPE")
        .as_deref()
        .and_then(HistoryScopeMode::parse)
    {
        resolved.history_scope = v;
    }
    if let Some(v) = env("LOOM_ETA_FIT_ENABLED").as_deref().and_then(parse_bool) {
        resolved.fit_enabled = v;
    }
    resolved.refresh_secs = resolved.refresh_secs.max(MIN_REFRESH_SECS);
    resolved.fleet_refresh = resolve_fleet_refresh(block.and_then(|b| b.get("fleetRefresh")), &env);
    resolved
}

/// `autonomous.eta.fleetRefresh`, env > config > default, then clamped.
fn resolve_fleet_refresh(
    block: Option<&serde_json::Value>,
    env: &impl Fn(&str) -> Option<String>,
) -> FleetRefreshConfig {
    let get = |key: &str| block.and_then(|b| b.get(key));
    let env_u64 = |key: &str| env(key).and_then(|s| s.trim().parse::<u64>().ok());
    let mut c = FleetRefreshConfig::default();
    if let Some(v) = get("enabled").and_then(serde_json::Value::as_bool) {
        c.enabled = v;
    }
    if let Some(v) = env("LOOM_ETA_FLEET_REFRESH_ENABLED")
        .as_deref()
        .and_then(parse_bool)
    {
        c.enabled = v;
    }
    let u64_key = |slot: &mut u64, key: &str, var: &str| {
        if let Some(v) = get(key).and_then(serde_json::Value::as_u64) {
            *slot = v;
        }
        if let Some(v) = env_u64(var) {
            *slot = v;
        }
    };
    u64_key(&mut c.interval_secs, "intervalSecs", "LOOM_ETA_FLEET_REFRESH_INTERVAL_SECS");
    u64_key(
        &mut c.max_calls_per_cycle,
        "maxCallsPerCycle",
        "LOOM_ETA_FLEET_REFRESH_MAX_CALLS",
    );
    u64_key(
        &mut c.backfill_max_calls_per_cycle,
        "backfillMaxCallsPerCycle",
        "LOOM_ETA_FLEET_REFRESH_BACKFILL_MAX_CALLS",
    );
    u64_key(&mut c.reserve_calls, "reserveCalls", "LOOM_ETA_FLEET_REFRESH_RESERVE");
    if let Some(v) = get("backfillDays").and_then(serde_json::Value::as_i64) {
        c.backfill_days = v;
    }
    if let Some(v) = env("LOOM_ETA_FLEET_REFRESH_BACKFILL_DAYS").and_then(|s| s.trim().parse().ok())
    {
        c.backfill_days = v;
    }
    c.interval_secs = c.interval_secs.max(MIN_FLEET_REFRESH_INTERVAL_SECS);
    c.backfill_days = c.backfill_days.max(MIN_FLEET_REFRESH_BACKFILL_DAYS);
    c.signoz = resolve_fleet_signoz(get("signoz"), env);
    c
}

/// `autonomous.eta.fleetRefresh.signoz`, env > config > default (#9758).
fn resolve_fleet_signoz(
    block: Option<&serde_json::Value>,
    env: &impl Fn(&str) -> Option<String>,
) -> FleetSignozConfig {
    let get = |key: &str| block.and_then(|b| b.get(key));
    let text = |key: &str, var: &str| {
        env(var)
            .or_else(|| {
                get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let count = |key: &str, var: &str, default: u32| {
        env(var)
            .and_then(|s| s.trim().parse::<u32>().ok())
            .or_else(|| {
                get(key)
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
            })
            .filter(|v| *v > 0)
            .unwrap_or(default)
    };
    let mut c = FleetSignozConfig::default();
    if let Some(v) = get("enabled").and_then(serde_json::Value::as_bool) {
        c.enabled = v;
    }
    if let Some(v) = env("LOOM_ETA_FLEET_SIGNOZ_ENABLED")
        .as_deref()
        .and_then(parse_bool)
    {
        c.enabled = v;
    }
    c.endpoint = text("endpoint", "LOOM_ETA_FLEET_SIGNOZ_ENDPOINT");
    c.user = text("user", "LOOM_ETA_FLEET_SIGNOZ_USER");
    c.credential_file =
        text("credentialFile", "LOOM_ETA_FLEET_SIGNOZ_CREDENTIAL_FILE").map(Into::into);
    c.page_size =
        count("pageSize", "LOOM_ETA_FLEET_SIGNOZ_PAGE_SIZE", DEFAULT_FLEET_SIGNOZ_PAGE_SIZE);
    c.max_pages =
        count("maxPages", "LOOM_ETA_FLEET_SIGNOZ_MAX_PAGES", DEFAULT_FLEET_SIGNOZ_MAX_PAGES);
    c
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
