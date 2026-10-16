//! Weighted, per-stage, per-attempt runtime draw (#11726).
//!
//! `runtimes.rolePreference` is "first available wins". On 2026-10-12 that let
//! a Codex-first Judge/Champion/Curator run blocked — every tool call denied by
//! a stale hook — exit cleanly, and never reach the Claude entry below it, for
//! ~30 h. Operator direction (2026-10-13): replace the ordered list with a
//! **weighted random draw** per stage and per attempt, so an experimental
//! provider gets a controlled, measurable share of a stage, and a failed or
//! blocked attempt is followed by a draw from the *next* attempt's table.
//!
//! ```jsonc
//! "runtimes": {
//!   "roleDraw": {
//!     "judge": [
//!       { "attempt": 1, "draw": { "claude/opus": 80, "claude/fable": 10, "codex": 10 } },
//!       { "attempt": 2, "draw": { "claude/fable": 100 } }
//!     ],
//!     // the same thing keyed by attempt number (merges per attempt under a
//!     // generic deep merge, e.g. fleet-gitops host overrides):
//!     "curator": { "1": { "codex": 10, "claude": 90 }, "2": { "claude": 100 } }
//!   },
//!   "excludeProviders": ["codex"]
//! }
//! ```
//!
//! # Rules (operator rulings, 2026-10-13)
//!
//! - `P(candidate) = w / Σw` over the **available** candidates; an unavailable
//!   one (not admitted, no spawnable credential, a launch-failure hold, at the
//!   backstop ceiling) is excluded and the rest renormalised. Implemented as a
//!   seeded weighted permutation walked by the ordinary preference walk — see
//!   [`crate::weighted_draw`] for why that is the same distribution.
//! - Weight `0` is off. `excludeProviders` removes a runtime from every table
//!   on the host and renormalises.
//! - A launched attempt that fails or is blocked (any `HoldFeed::failure`)
//!   counts as a failed attempt: the next tick of that role in that workspace
//!   draws from the next attempt's table ([`attempts`]). A success resets to
//!   attempt 1. Attempt `N` uses the highest configured attempt `<= N`.
//! - No content validation: any mix is allowed. A table that is empty after
//!   exclusions is a non-blocking **warning** (logged, shown by `loom-daemon
//!   roles draw --explain`); the stage then has nothing to run on and its tick
//!   is a pre-spawn skip, exactly like a wholly unavailable preference list.
//! - Malformed entries are skipped with a warning, never fatal — an older or
//!   newer shape must not stop a daemon.
//! - When a role has **no** `roleDraw` table for the attempt, nothing here
//!   applies and `rolePreference` / `preference` / static resolution decide,
//!   byte-identically to before.
//!
//! # Candidate keys
//!
//! `<runtime>[:<profile>][/<model-or-profile>]`:
//!
//! - `claude`, `codex` — the bare runtime; the model resolves as it does
//!   today (`roleModels`, shipped default, CLI default). The same tap a
//!   `rolePreference` entry `"claude"` names.
//! - `opencode:zai-flash` — a runtime with a model profile, exactly the
//!   `{"runtime":"opencode","modelProfile":"zai-flash"}` preference entry
//!   (its `Display` rendering).
//! - `claude/opus`, `claude/fable` — the runtime plus a model pinned with
//!   `--model` on the role-tick launch. For a **native** runtime (OpenCode,
//!   whose model is chosen by its profile) the `/` segment names the model
//!   profile instead: `opencode/zai-flash` ≡ `opencode:zai-flash`.
//!
//! # Reproducibility
//!
//! One seed per draw: the first 8 bytes (big-endian) of
//! `sha256("loom-role-draw/v1:" + minute + ":" + host + ":" + workspace + ":" +
//! role + ":" + attempt)` — the workspace draw's `loom-draw/v1` scheme
//! (#11683) plus the workspace directory name, role and attempt. The
//! [`RoleDrawRecord`] carries the seed, its inputs, the table, every roll and
//! the pick, on the `# LOOM_RUNTIME_PREFERENCE` marker and the
//! `role_tick.outcome` record.

pub mod attempts;
pub mod explain;

use super::resolve::{Resolution, Tap};
use crate::config_resolver::{
    get_path, private_defaults_path, soft_read_json_object, FLEET_REPO_CONFIG_REL,
    LEGACY_CONFIG_REL, LOCAL_CONFIG_REL, PROJECT_CONFIG_REL,
};
use crate::priority_pick::PickRng;
use crate::runtime_admission::canonical_role;
use crate::weighted_draw::{draw_order, DrawStep};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The seed-derivation scheme, and the hash input's prefix.
pub const SEED_SCHEME: &str = "loom-role-draw/v1";

/// One draw candidate, parsed from its key ([module docs](self)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The key as configured, trimmed.
    pub key: String,
    /// The runtime id (`claude`, `codex`, `opencode`, …) — the "provider"
    /// `excludeProviders` matches.
    pub runtime: String,
    /// The model profile the tap pins, when the key names one.
    pub model_profile: Option<String>,
    /// The model a role-tick launch pins with `--model`, when the key names one.
    pub model: Option<String>,
}

impl Candidate {
    /// Parse `<runtime>[:<profile>][/<model-or-profile>]`.
    ///
    /// # Errors
    /// A message naming the key when a segment is empty or the runtime id has
    /// a character outside `[A-Za-z0-9._-]`.
    pub fn parse(key: &str) -> Result<Self, String> {
        let key = key.trim();
        let (head, slash) = match key.split_once('/') {
            Some((head, slash)) => (head, Some(slash)),
            None => (key, None),
        };
        let (runtime, profile) = match head.split_once(':') {
            Some((runtime, profile)) => (runtime, Some(profile)),
            None => (head, None),
        };
        let valid = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        };
        if !valid(runtime) {
            return Err(format!("candidate {key:?}: runtime must match [A-Za-z0-9._-]+"));
        }
        if profile.is_some_and(str::is_empty) || slash.is_some_and(str::is_empty) {
            return Err(format!("candidate {key:?} has an empty segment"));
        }
        let (mut model_profile, mut model) = (profile.map(str::to_string), None);
        if let Some(slash) = slash {
            if model_profile.is_none() && crate::worker_spawn::is_native(runtime) {
                model_profile = Some(slash.to_string());
            } else {
                model = Some(slash.to_string());
            }
        }
        Ok(Self {
            key: key.to_string(),
            runtime: runtime.to_string(),
            model_profile,
            model,
        })
    }

    /// The preference-walk tap this candidate launches on.
    #[must_use]
    pub fn tap(&self) -> Tap {
        Tap {
            runtime: self.runtime.clone(),
            model_profile: self.model_profile.clone(),
        }
    }
}

/// Which config tier set a table entry. The machine-defaults tier is where
/// fleet-gitops renders `config.defaults` plus host-class and host overrides;
/// which of *those* set a value is fleet-gitops' own `--explain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layer {
    /// `~/.local/share/loom/config/defaults.json` (`LOOM_CONFIG_DEFAULTS_FILE`).
    MachineDefaults,
    /// The workspace's `.loom/config.json`.
    RepoConfig,
    /// The workspace's tracked `.loom-project/project.json`.
    Project,
    /// The per-repo fleet tier, `.loom-local/fleet-repo.json`, written by the
    /// fleet sync from the fleet config's `repos[].config` (#11738).
    FleetRepo,
    /// The workspace's ignored `.loom-local/local.json`.
    Local,
}

impl Layer {
    /// Stable wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MachineDefaults => "machine-defaults",
            Self::RepoConfig => "repo-config",
            Self::Project => "project",
            Self::FleetRepo => "fleet-repo",
            Self::Local => "local",
        }
    }
}

/// The config tiers, lowest precedence first — the merge order of
/// [`crate::config_resolver::resolve_effective_config`].
fn tiers(root: &Path) -> Vec<(Layer, PathBuf)> {
    let mut tiers: Vec<(Layer, PathBuf)> = private_defaults_path()
        .map(|path| (Layer::MachineDefaults, path))
        .into_iter()
        .collect();
    tiers.push((Layer::RepoConfig, root.join(LEGACY_CONFIG_REL)));
    tiers.push((Layer::Project, root.join(PROJECT_CONFIG_REL)));
    tiers.push((Layer::FleetRepo, root.join(FLEET_REPO_CONFIG_REL)));
    tiers.push((Layer::Local, root.join(LOCAL_CONFIG_REL)));
    tiers
}

/// One configured attempt table, as one tier set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptTable {
    /// The attempt number it applies from (`>= 1`).
    pub attempt: u32,
    /// The tier that set it.
    pub layer: Layer,
    /// `(candidate key, weight)` in declaration order.
    pub entries: Vec<(String, u64)>,
}

/// Every role's per-attempt tables plus the host's provider exclusions, merged
/// across the config tiers: a higher tier replaces a role's table **per
/// attempt**, so a host-local override of `judge` attempt 1 keeps the fleet's
/// attempt 2 and every other role untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrawConfig {
    /// Canonical role → attempt → table.
    pub roles: BTreeMap<String, BTreeMap<u32, AttemptTable>>,
    /// `runtimes.excludeProviders` from the highest tier that sets it.
    pub exclude: Vec<String>,
    /// The tier that set [`Self::exclude`].
    pub exclude_layer: Option<Layer>,
    /// Non-fatal problems (malformed entries skipped, unknown role names).
    pub warnings: Vec<String>,
}

impl DrawConfig {
    /// Read and merge every tier for `root`. Never fails: an unreadable tier
    /// contributes nothing, a malformed entry is a warning.
    #[must_use]
    pub fn load(root: &Path) -> Self {
        let layers: Vec<(Layer, Value)> = tiers(root)
            .into_iter()
            .map(|(layer, path)| (layer, soft_read_json_object(&path)))
            .collect();
        Self::from_layers(&layers)
    }

    /// [`Self::load`] over already-read tiers, lowest precedence first.
    #[must_use]
    pub fn from_layers(layers: &[(Layer, Value)]) -> Self {
        let mut out = Self::default();
        for (layer, config) in layers {
            out.merge_tier(*layer, config);
        }
        out
    }

    fn merge_tier(&mut self, layer: Layer, config: &Value) {
        match get_path(config, "runtimes.excludeProviders") {
            None => {}
            Some(Value::Null) => {
                self.exclude.clear();
                self.exclude_layer = None;
            }
            Some(Value::Array(items)) => {
                self.exclude = items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                self.exclude_layer = Some(layer);
                if self.exclude.len() != items.len() {
                    self.warn(layer, "runtimes.excludeProviders: non-string entries ignored");
                }
            }
            Some(_) => self.warn(layer, "runtimes.excludeProviders must be an array; ignored"),
        }
        let roles = match get_path(config, "runtimes.roleDraw") {
            None => return,
            Some(Value::Null) => {
                self.roles.clear();
                return;
            }
            Some(Value::Object(roles)) => roles,
            Some(_) => {
                self.warn(layer, "runtimes.roleDraw must be an object of roles; ignored");
                return;
            }
        };
        for (name, value) in roles {
            if name.starts_with('_') {
                continue; // `_note` keys, the fleet config's comment convention
            }
            let Some(role) = canonical_role(name) else {
                self.warn(layer, &format!("runtimes.roleDraw.{name}: unknown role; ignored"));
                continue;
            };
            if value.is_null() {
                self.roles.remove(role);
                continue;
            }
            for table in self.parse_role(layer, name, value) {
                self.roles
                    .entry(role.to_string())
                    .or_default()
                    .insert(table.attempt, table);
            }
        }
    }

    fn parse_role(&mut self, layer: Layer, name: &str, value: &Value) -> Vec<AttemptTable> {
        let path = format!("runtimes.roleDraw.{name}");
        let raw: Vec<(Option<u32>, &Value)> = match value {
            Value::Array(items) => items
                .iter()
                .map(|item| {
                    let attempt = item
                        .get("attempt")
                        .and_then(Value::as_u64)
                        .and_then(|a| u32::try_from(a).ok());
                    (attempt, item.get("draw").unwrap_or(&Value::Null))
                })
                .collect(),
            Value::Object(map) => map
                .iter()
                .filter(|(k, _)| !k.starts_with('_'))
                .map(|(k, v)| (k.trim().parse::<u32>().ok(), v))
                .collect(),
            _ => {
                self.warn(layer, &format!("{path} must be a list or an object; ignored"));
                return Vec::new();
            }
        };
        let mut tables = Vec::new();
        for (attempt, draw) in raw {
            let Some(attempt) = attempt.filter(|a| *a >= 1) else {
                self.warn(layer, &format!("{path}: an entry has no attempt >= 1; ignored"));
                continue;
            };
            let Some(map) = draw.as_object() else {
                self.warn(layer, &format!("{path} attempt {attempt}: draw must be an object"));
                continue;
            };
            let mut entries = Vec::new();
            for (key, weight) in map {
                if key.starts_with('_') {
                    continue;
                }
                match weight.as_u64() {
                    Some(w) => entries.push((key.trim().to_string(), w)),
                    None => self.warn(
                        layer,
                        &format!(
                            "{path} attempt {attempt}: weight of {key:?} must be a \
                             non-negative integer; ignored"
                        ),
                    ),
                }
            }
            tables.push(AttemptTable {
                attempt,
                layer,
                entries,
            });
        }
        tables
    }

    fn warn(&mut self, layer: Layer, message: &str) {
        self.warnings
            .push(format!("{message} ({})", layer.as_str()));
    }

    /// The table attempt `attempt` of `role` draws from: the highest
    /// configured attempt `<= attempt`. `None` ⇒ no draw for this tick.
    #[must_use]
    pub fn table_for(&self, role: &str, attempt: u32) -> Option<&AttemptTable> {
        self.roles
            .get(role)?
            .range(..=attempt.max(1))
            .next_back()
            .map(|(_, table)| table)
    }

    /// The table attempt `attempt` of `role` draws from, after exclusions —
    /// falling through (#11782). The table [`Self::table_for`] names can be
    /// empty on this host (every candidate excluded or weight 0), and a skip
    /// before spawn never advances the attempt counter, so the stage would
    /// stay on that table forever. "The draw never waits on a missing
    /// provider": when it is empty, the next configured attempt's table is
    /// tried, and so on; the first non-empty one wins. When every remaining
    /// table is empty too, the originally named (empty) table is returned so
    /// the skip still carries its record and the EMPTY diagnostic.
    #[must_use]
    pub fn effective_for(&self, role: &str, attempt: u32) -> Option<EffectiveTable> {
        let first = self.table_for(role, attempt)?;
        let named = self.effective(first);
        if !named.active.is_empty() {
            return Some(named);
        }
        let later = self.roles.get(role).into_iter().flat_map(|tables| {
            tables
                .range(first.attempt.saturating_add(1)..)
                .map(|(_, table)| table)
        });
        for table in later {
            let effective = self.effective(table);
            if !effective.active.is_empty() {
                return Some(effective);
            }
        }
        Some(named)
    }

    /// Whether `runtime` is excluded on this host.
    #[must_use]
    pub fn excludes(&self, candidate: &Candidate) -> bool {
        self.exclude
            .iter()
            .any(|p| p == &candidate.runtime || p == &candidate.key)
    }

    /// Apply exclusions and weight-0 to `table`.
    #[must_use]
    pub fn effective(&self, table: &AttemptTable) -> EffectiveTable {
        let mut out = EffectiveTable {
            attempt: table.attempt,
            layer: table.layer,
            active: Vec::new(),
            off: Vec::new(),
        };
        for (key, weight) in &table.entries {
            let why = match Candidate::parse(key) {
                Err(error) => Some(format!("invalid: {error}")),
                Ok(candidate) if self.excludes(&candidate) => Some(format!(
                    "excluded by runtimes.excludeProviders ({})",
                    self.exclude_layer.map_or("unknown", Layer::as_str)
                )),
                Ok(_) if *weight == 0 => Some("weight 0 (off)".to_string()),
                Ok(candidate) => {
                    out.active.push((candidate, *weight));
                    None
                }
            };
            if let Some(why) = why {
                out.off.push((key.clone(), why));
            }
        }
        out
    }
}

/// An attempt table after exclusions: the candidates that may be drawn and
/// the ones that may not, with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveTable {
    pub attempt: u32,
    pub layer: Layer,
    /// Positive-weight, non-excluded candidates in declaration order.
    pub active: Vec<(Candidate, u64)>,
    /// `(key, reason)` for every entry that cannot be drawn.
    pub off: Vec<(String, String)>,
}

impl EffectiveTable {
    /// `Σ w` over [`Self::active`], saturating at `u64::MAX` (#11782).
    #[must_use]
    pub fn total(&self) -> u64 {
        self.active
            .iter()
            .fold(0u64, |acc, (_, w)| acc.saturating_add(*w))
    }
}

/// One `(candidate, weight)` row of a recorded table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrawWeight {
    pub candidate: String,
    pub weight: u64,
}

/// What a draw's seed was derived from: enough to recompute it with [`seed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedInputs {
    pub scheme: String,
    /// `floor(unix_utc_secs / 60)`.
    pub minute: u64,
    pub host: String,
    /// The workspace directory's name.
    pub workspace: String,
}

/// The decision record of one role draw — the replayable "seed, table, Σw,
/// roll, pick" the operator asked for, carried on the preference marker and
/// the `role_tick.outcome` record (`role_draw`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleDrawRecord {
    /// Canonical role.
    pub role: String,
    /// This tick's attempt number (1 + consecutive failed attempts).
    pub attempt: u32,
    /// The configured attempt table that applied (highest `<= attempt`).
    pub table_attempt: u32,
    /// The config tier that set that table.
    pub layer: Layer,
    /// The drawable candidates and their weights, declaration order.
    pub table: Vec<DrawWeight>,
    /// Entries that could not be drawn, `key: reason`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub off: Vec<String>,
    /// The seed, 16 lowercase hex digits (a JSON number would lose bits).
    pub seed: String,
    /// What the seed was derived from; absent when a test pinned it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_inputs: Option<SeedInputs>,
    /// The rolls consumed: one per candidate drawn, up to and including the
    /// pick. More than one means earlier picks were unavailable.
    pub steps: Vec<DrawStep<String>>,
    /// Earlier picks the walk passed over, `key: reason`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passed_over: Vec<String>,
    /// The candidate key that launched; `None` when nothing could.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick: Option<String>,
    /// The model the pick pins (the `/<model>` segment), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl RoleDrawRecord {
    /// The winning step (the last one consumed), when there was a pick.
    #[must_use]
    pub fn winning_step(&self) -> Option<&DrawStep<String>> {
        self.pick.as_ref().and(self.steps.last())
    }

    /// The marker fragment: `draw=attempt:2,table:a2@repo-config,seed:…,
    /// sum:100,roll:37,pick:claude/opus`.
    #[must_use]
    pub fn marker_fragment(&self) -> String {
        let step = self.winning_step();
        format!(
            "draw=attempt:{},table:a{}@{},seed:{},sum:{},roll:{},pick:{}",
            self.attempt,
            self.table_attempt,
            self.layer.as_str(),
            self.seed,
            step.map_or_else(|| "none".to_string(), |s| s.total.to_string()),
            step.map_or_else(|| "none".to_string(), |s| s.roll.to_string()),
            self.pick.as_deref().unwrap_or("none"),
        )
    }

    /// Extra lines for a fail-closed diagnostic.
    #[must_use]
    pub fn diagnostic_suffix(&self) -> String {
        let mut out = format!(
            "\n  role draw: attempt {} uses table a{} set by {}",
            self.attempt,
            self.table_attempt,
            self.layer.as_str()
        );
        if self.table.is_empty() {
            out.push_str(" — EMPTY after exclusions/weight 0: nothing can run this stage here");
        }
        for off in &self.off {
            out.push_str(&format!("\n  {off}"));
        }
        out
    }
}

thread_local! {
    static SEED_OVERRIDE: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Run `f` with every role draw on this thread seeded by `seed` (tests).
pub fn with_seed<R>(seed: u64, f: impl FnOnce() -> R) -> R {
    let previous = SEED_OVERRIDE.with(|s| s.replace(Some(seed)));
    let out = f();
    SEED_OVERRIDE.with(|s| s.set(previous));
    out
}

/// The role-draw seed ([module docs](self)).
#[must_use]
pub fn seed(minute: u64, host: &str, workspace: &str, role: &str, attempt: u32) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(
        format!("{SEED_SCHEME}:{minute}:{host}:{workspace}:{role}:{attempt}").as_bytes(),
    );
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head)
}

/// A drawn order, ready for the preference walk, and the record to finish
/// once the walk has decided.
#[derive(Debug, Clone)]
pub struct DrawPlan {
    /// The candidates in drawn order (step `i` of [`RoleDrawRecord::steps`]).
    pub order: Vec<Candidate>,
    steps: Vec<DrawStep<String>>,
    record: RoleDrawRecord,
}

impl DrawPlan {
    /// The taps to walk, in drawn order.
    #[must_use]
    pub fn taps(&self) -> Vec<Tap> {
        self.order.iter().map(Candidate::tap).collect()
    }

    /// Fill the record from the walk's outcome: the steps up to the chosen
    /// tier, the passed-over picks and the pick itself.
    #[must_use]
    pub fn finish<T>(self, resolution: &Resolution<T>) -> RoleDrawRecord {
        let mut record = self.record;
        let consumed = resolution
            .chosen
            .as_ref()
            .map_or(self.steps.len(), |chosen| chosen.tier + 1);
        record.steps = self.steps.into_iter().take(consumed).collect();
        record.passed_over = resolution
            .skipped
            .iter()
            .filter_map(|s| {
                self.order
                    .get(s.tier)
                    .map(|c| format!("{}: {}", c.key, s.reason))
            })
            .collect();
        if let Some(chosen) = &resolution.chosen {
            if let Some(candidate) = self.order.get(chosen.tier) {
                record.pick = Some(candidate.key.clone());
                record.model.clone_from(&candidate.model);
            }
        }
        record
    }
}

/// Draw `table` for `role` at `attempt` with `seed`. Pure.
#[must_use]
pub fn plan(
    role: &str,
    attempt: u32,
    table: &EffectiveTable,
    seed: u64,
    seed_inputs: Option<SeedInputs>,
) -> DrawPlan {
    let keyed: Vec<(usize, u64)> = table
        .active
        .iter()
        .enumerate()
        .map(|(i, (_, w))| (i, *w))
        .collect();
    let drawn = draw_order(&keyed, &mut PickRng::from_seed(seed));
    let order: Vec<Candidate> = drawn
        .iter()
        .map(|s| table.active[s.picked].0.clone())
        .collect();
    let steps = drawn
        .iter()
        .map(|s| DrawStep {
            total: s.total,
            roll: s.roll,
            picked: table.active[s.picked].0.key.clone(),
        })
        .collect();
    DrawPlan {
        order,
        steps,
        record: RoleDrawRecord {
            role: role.to_string(),
            attempt,
            table_attempt: table.attempt,
            layer: table.layer,
            table: table
                .active
                .iter()
                .map(|(c, w)| DrawWeight {
                    candidate: c.key.clone(),
                    weight: *w,
                })
                .collect(),
            off: table
                .off
                .iter()
                .map(|(k, why)| format!("{k}: {why}"))
                .collect(),
            seed: format!("{seed:016x}"),
            seed_inputs,
            steps: Vec::new(),
            passed_over: Vec::new(),
            pick: None,
            model: None,
        },
    }
}

/// The draw for one resolution of `role` in `root`, or `None` when no
/// `roleDraw` table applies (the caller then keeps today's path).
///
/// `role_tick` selects the attempt: a role tick uses its tracked attempt
/// ([`attempts::current`]); every other caller (sweep dispatch, phase runs,
/// probes) draws attempt 1.
#[must_use]
pub fn prepare(root: &Path, role: &str, now: u64, role_tick: bool) -> Option<DrawPlan> {
    let config = DrawConfig::load(root);
    for warning in &config.warnings {
        log::debug!("role_draw: {warning} (#11726)");
    }
    let attempt = if role_tick {
        attempts::current(root, role)
    } else {
        1
    };
    let table = config.effective_for(role, attempt)?;
    if table.active.is_empty() {
        log::warn!(
            "role_draw: {role} attempt {attempt} table a{} ({}) is EMPTY after exclusions — \
             nothing can run this stage on this host; the tick is skipped. Not an error: \
             check `loom-daemon roles draw --explain` (#11726)",
            table.attempt,
            table.layer.as_str()
        );
    }
    let workspace = root
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let (seed_value, inputs) = match SEED_OVERRIDE.with(Cell::get) {
        Some(pinned) => (pinned, None),
        None => {
            let minute = now / 60;
            let host = crate::sweep_registry::host_identity();
            (
                seed(minute, &host, &workspace, role, attempt),
                Some(SeedInputs {
                    scheme: SEED_SCHEME.to_string(),
                    minute,
                    host,
                    workspace,
                }),
            )
        }
    };
    Some(plan(role, attempt, &table, seed_value, inputs))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
