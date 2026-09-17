//! `start` / `stop`: the half of #8055 that writes to a workspace.
//!
//! Everything here is built around one rule: **`stop` must be able to undo
//! exactly what `start` did, and nothing else.** `.loom-local/local.json` is
//! the operator's own highest-precedence override tier
//! ([`LOCAL_CONFIG_REL`](crate::config_resolver::LOCAL_CONFIG_REL)); it may
//! already hold unrelated keys, and it may already hold a *different* value at
//! one of the two pointers the experiment writes. So `start` records, inside
//! the file, a [`Marker`] carrying the pointers it wrote, the value it wrote at
//! each, and the value that was there before (present or absent).
//!
//! The pointer edits go through [`crate::config_resolver::deep_merge`] — the
//! same merge the resolver itself performs — so an overlay that already carries
//! `{"autonomous": {"workFinder": …}}` keeps it.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{Plan, EXPERIMENT_POINTERS, MARKER_KEY, STATE_DIR_ENV};
use crate::config_resolver::{deep_merge, LOCAL_CONFIG_REL};

/// What was at a pointer before `start` wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prior {
    /// `false` means the pointer did not exist — `stop` removes it outright.
    pub present: bool,
    /// The previous value, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// The ownership marker `start` writes into each overlay under
/// [`MARKER_KEY`]. Its `id` is what makes a second experiment refuse to start
/// on a workspace this one already owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// The owning experiment's id.
    pub id: String,
    /// The arm (and therefore the model alias) this workspace was assigned.
    pub arm: String,
    /// When `start` wrote this overlay.
    pub started_at: String,
    /// The `--until` date, when one was given. Advisory: nothing expires an
    /// experiment automatically, `stop` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// The JSON pointers this experiment owns in this file.
    pub keys: Vec<String>,
    /// What was written at each pointer, so `stop` can detect drift.
    pub wrote: BTreeMap<String, Value>,
    /// What was there before, so `stop` can restore rather than just delete.
    pub prior: BTreeMap<String, Prior>,
}

/// The host-level record of a running experiment, at
/// `~/.loom/experiments/<id>.json`. It lives beside the workspace registry
/// rather than in any one repo: an experiment spans workspaces and must
/// survive any single repo being cleaned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentState {
    /// Same value as `plan.experiment_id`, hoisted so the file is greppable.
    pub experiment_id: String,
    /// The plan `start` was given, verbatim — the source of truth.
    pub plan: Plan,
    /// When `start` ran.
    pub started_at: String,
    /// `--until`, when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Set by `stop`; a state file with this set is a historical record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<String>,
    /// Repo-relative overlay path written in each workspace.
    pub overlay_rel: String,
    /// The pointers written in every workspace.
    pub keys: Vec<String>,
}

/// Host-level experiment-state directory: `$LOOM_EXPERIMENTS_DIR` when set,
/// else `~/.loom/experiments`.
pub fn state_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var(STATE_DIR_ENV) {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(".loom").join("experiments"))
}

/// The state file for one experiment id.
#[must_use]
pub fn state_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// A workspace's overlay file.
#[must_use]
pub fn overlay_path(root: &Path) -> PathBuf {
    root.join(LOCAL_CONFIG_REL)
}

// ---------------------------------------------------------------------------
// Pointer plumbing
// ---------------------------------------------------------------------------

fn segments(pointer: &str) -> Vec<&str> {
    pointer.trim_start_matches('/').split('/').collect()
}

/// Read a pointer out of a document, `None` when any segment is missing.
#[must_use]
pub fn get_pointer(doc: &Value, pointer: &str) -> Option<Value> {
    doc.pointer(pointer).cloned()
}

/// A single-pointer overlay document: `/a/b` + `v` ⇒ `{"a":{"b":v}}`. Merging
/// one of these through `deep_merge` is how a pointer is set without touching
/// its siblings.
#[must_use]
pub fn pointer_overlay(pointer: &str, value: Value) -> Value {
    let mut built = value;
    for seg in segments(pointer).into_iter().rev() {
        let mut map = Map::new();
        map.insert((*seg).to_string(), built);
        built = Value::Object(map);
    }
    built
}

/// Remove a pointer, then prune every container the removal emptied.
///
/// Pruning matters: leaving `{"autonomous":{"roleRunner":{}}}` behind would
/// mean a `stop`ped workspace never reaches the "the file became `{}`, delete
/// it" case, and the operator is left with a residue file that says nothing.
pub fn remove_pointer(doc: &mut Value, pointer: &str) {
    let segs = segments(pointer);
    remove_segments(doc, &segs);
}

fn remove_segments(doc: &mut Value, segs: &[&str]) {
    let Some(map) = doc.as_object_mut() else {
        return;
    };
    let Some((head, rest)) = segs.split_first() else {
        return;
    };
    if rest.is_empty() {
        map.shift_remove(*head);
        return;
    }
    if let Some(child) = map.get_mut(*head) {
        remove_segments(child, rest);
        if child.as_object().is_some_and(Map::is_empty) {
            map.shift_remove(*head);
        }
    }
}

// ---------------------------------------------------------------------------
// The overlay edit (pure)
// ---------------------------------------------------------------------------

/// Compute the post-`start` overlay document for one workspace, plus the
/// marker that records how to undo it. Pure — no filesystem access.
///
/// Errors when `doc` is not a JSON object: a `.loom-local/local.json` holding
/// an array or a scalar contributes nothing to the resolver today, and merging
/// into it would destroy whatever the operator meant by it.
pub fn plan_overlay_edit(
    doc: &Value,
    id: &str,
    arm: &str,
    started_at: &str,
    until: Option<&str>,
) -> Result<(Value, Marker)> {
    if !doc.is_object() {
        bail!("overlay is not a JSON object");
    }
    let mut prior = BTreeMap::new();
    let mut wrote = BTreeMap::new();
    let mut next = doc.clone();
    for pointer in EXPERIMENT_POINTERS {
        let before = get_pointer(doc, pointer);
        prior.insert(
            pointer.to_string(),
            Prior {
                present: before.is_some(),
                value: before,
            },
        );
        let value = Value::String(arm.to_string());
        wrote.insert(pointer.to_string(), value.clone());
        next = deep_merge(&next, &pointer_overlay(pointer, value));
    }
    let marker = Marker {
        id: id.to_string(),
        arm: arm.to_string(),
        started_at: started_at.to_string(),
        until: until.map(str::to_string),
        keys: EXPERIMENT_POINTERS
            .iter()
            .map(|p| (*p).to_string())
            .collect(),
        wrote,
        prior,
    };
    if let Some(map) = next.as_object_mut() {
        map.insert(MARKER_KEY.to_string(), serde_json::to_value(&marker)?);
    }
    Ok((next, marker))
}

/// The id of the experiment that owns this overlay, if any.
#[must_use]
pub fn marker_id(doc: &Value) -> Option<String> {
    doc.get(MARKER_KEY)?.get("id")?.as_str().map(str::to_string)
}

/// Parse the marker out of an overlay document.
#[must_use]
pub fn read_marker(doc: &Value) -> Option<Marker> {
    serde_json::from_value(doc.get(MARKER_KEY)?.clone()).ok()
}

/// What [`revert_overlay_edit`] decided about one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revert {
    /// No marker at all — `start` never wrote here, or `stop` already ran.
    NotOurs,
    /// A marker, but a different experiment's. Left strictly alone.
    OtherExperiment(String),
    /// One or more pointers no longer hold what `start` wrote. Left alone
    /// unless `force`.
    Drifted(Vec<String>),
    /// The document with this experiment's keys reversed. `None` means the
    /// document became `{}` and the file should be deleted.
    Reverted(Option<Value>),
}

/// Reverse exactly the pointers `start` wrote in this document.
///
/// A pointer that had a prior value is **restored** to it, not deleted — the
/// round-trip property the acceptance criteria ask for ("the file returns to
/// its pre-`start` bytes") is only true if a pre-existing pin comes back.
#[must_use]
pub fn revert_overlay_edit(doc: &Value, id: &str, force: bool) -> Revert {
    let Some(marker) = read_marker(doc) else {
        return Revert::NotOurs;
    };
    if marker.id != id {
        return Revert::OtherExperiment(marker.id);
    }
    let drifted: Vec<String> = marker
        .keys
        .iter()
        .filter(|p| {
            let expected = marker.wrote.get(*p);
            get_pointer(doc, p).as_ref() != expected
        })
        .cloned()
        .collect();
    if !drifted.is_empty() && !force {
        return Revert::Drifted(drifted);
    }
    let mut next = doc.clone();
    for pointer in &marker.keys {
        match marker.prior.get(pointer) {
            Some(Prior {
                present: true,
                value: Some(previous),
            }) => {
                next = deep_merge(&next, &pointer_overlay(pointer, previous.clone()));
            }
            _ => remove_pointer(&mut next, pointer),
        }
    }
    if let Some(map) = next.as_object_mut() {
        map.shift_remove(MARKER_KEY);
    }
    if next.as_object().is_some_and(Map::is_empty) {
        Revert::Reverted(None)
    } else {
        Revert::Reverted(Some(next))
    }
}

// ---------------------------------------------------------------------------
// start
// ---------------------------------------------------------------------------

/// Pool-hold probe signature. Production passes [`live_pool_held`]; tests pass
/// a stub, because the real probe reads this host's token pool and a test that
/// depended on it would pass or fail with the operator's account state.
pub type PoolProbe = fn(&Path, DateTime<Utc>) -> bool;

/// The production probe: `work_finder::pool_preflight::observe_root`, the same
/// live pre-flight read the work finder uses to decide whether a root may be
/// dispatched to at all. Reused rather than re-derived so this refusal can
/// never disagree with the dispatcher's own view of the pool.
#[must_use]
pub fn live_pool_held(root: &Path, now: DateTime<Utc>) -> bool {
    crate::work_finder::pool_preflight::observe_root(root, now)
}

/// Inputs to [`start`].
pub struct StartOptions<'a> {
    /// The plan to start. Source of truth for which workspace gets which arm.
    pub plan: &'a Plan,
    /// Advisory end date, recorded in the state file and every marker.
    pub until: Option<&'a str>,
    /// Start even though a workspace's token pool is currently held.
    pub allow_exhausted_pool: bool,
    /// Where the host-level state file goes.
    pub state_dir: &'a Path,
    /// Injected clock.
    pub now: DateTime<Utc>,
    /// Injected pool probe — see [`PoolProbe`].
    pub pool_held: PoolProbe,
}

/// What [`start`] did.
#[derive(Debug)]
pub struct StartReport {
    /// `(workspace root, arm)` for every overlay written.
    pub started: Vec<(PathBuf, String)>,
    /// Non-fatal findings the operator should see: CLI-default sentinel pins,
    /// per-role pins that outrank the overlay, un-ignored overlay paths.
    pub warnings: Vec<String>,
    /// The host-level state file.
    pub state_file: PathBuf,
}

/// Write the experiment's overlays.
///
/// Refuses — **before writing anything** — when any workspace is missing, has
/// an unparsable overlay, already carries a *different* experiment's marker, or
/// sits on a held token pool (an experiment that starts during pool exhaustion
/// measures exhaustion). The validation pass is separate from the write pass
/// precisely so a refusal leaves no half-started fleet behind.
pub fn start(opts: StartOptions<'_>) -> Result<StartReport> {
    let started_at = opts.now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut refusals: Vec<String> = Vec::new();
    let mut held: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut pending: Vec<(PathBuf, PathBuf, Value, String)> = Vec::new();

    for w in &opts.plan.workspaces {
        let root = PathBuf::from(&w.path);
        if !root.is_dir() {
            refusals.push(format!("{}: workspace directory does not exist", w.path));
            continue;
        }
        let overlay = overlay_path(&root);
        let doc = match read_overlay(&overlay) {
            Ok(v) => v,
            Err(e) => {
                refusals.push(format!("{}: {e}", overlay.display()));
                continue;
            }
        };
        match marker_id(&doc) {
            Some(existing) if existing != opts.plan.experiment_id => {
                refusals.push(format!(
                    "{}: already carries experiment {existing} — stop it first",
                    overlay.display()
                ));
                continue;
            }
            _ => {}
        }
        if (opts.pool_held)(&root, opts.now) {
            held.push(w.path.clone());
        }
        warnings.extend(effectiveness_warnings(&root, &w.arm));
        if let Some(note) = gitignore_warning(&root) {
            warnings.push(note);
        }
        let (next, _marker) =
            plan_overlay_edit(&doc, &opts.plan.experiment_id, &w.arm, &started_at, opts.until)
                .with_context(|| format!("preparing {}", overlay.display()))?;
        pending.push((root, overlay, next, w.arm.clone()));
    }

    if !refusals.is_empty() {
        bail!(
            "refusing to start {}: nothing was written.\n  {}",
            opts.plan.experiment_id,
            refusals.join("\n  ")
        );
    }
    if !held.is_empty() && !opts.allow_exhausted_pool {
        bail!(
            "refusing to start {}: the token pool is currently held for {} workspace(s); an \
             experiment that starts during exhaustion measures exhaustion. Nothing was written. \
             Wait for the hold to clear, or pass --allow-exhausted-pool to record the override.\n  \
             {}",
            opts.plan.experiment_id,
            held.len(),
            held.join("\n  ")
        );
    }
    if !held.is_empty() {
        warnings.push(format!(
            "--allow-exhausted-pool: started with {} workspace(s) on a held token pool",
            held.len()
        ));
    }

    // State first, overlays second. A crash between the two leaves overlays
    // that `stop --id` can still find and reverse; the reverse order would
    // leave overlays no command knows about.
    let state = ExperimentState {
        experiment_id: opts.plan.experiment_id.clone(),
        plan: opts.plan.clone(),
        started_at: started_at.clone(),
        until: opts.until.map(str::to_string),
        stopped_at: None,
        overlay_rel: LOCAL_CONFIG_REL.to_string(),
        keys: EXPERIMENT_POINTERS
            .iter()
            .map(|p| (*p).to_string())
            .collect(),
    };
    let state_file = state_path(opts.state_dir, &opts.plan.experiment_id);
    super::super::write_json_file(&state_file, &serde_json::to_value(&state)?)
        .with_context(|| format!("writing {}", state_file.display()))?;

    let mut started = Vec::new();
    for (root, overlay, next, arm) in pending {
        super::super::write_json_file(&overlay, &next)
            .with_context(|| format!("writing {}", overlay.display()))?;
        started.push((root, arm));
    }

    Ok(StartReport {
        started,
        warnings,
        state_file,
    })
}

/// Read an overlay, treating "absent" as `{}` and any other problem as an
/// error. This deliberately does **not** share `config_resolver`'s soft-fail
/// read: the resolver may ignore a malformed tier, but overwriting a file we
/// could not parse would destroy whatever the operator put there.
fn read_overlay(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Value::Object(Map::new())),
        Ok(text) => {
            let parsed: Value = serde_json::from_str(&text)
                .with_context(|| "overlay is not valid JSON".to_string())?;
            if !parsed.is_object() {
                bail!("overlay top level is not a JSON object");
            }
            Ok(parsed)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Warn where the arm will not actually take effect for role-runner ticks.
///
/// Two shapes, both from `role_runner::model_resolution`:
///
/// * a per-role pin (`autonomous.roleRunner.roleModels.<role>`) occupies the
///   same precedence tier as the global `roleRunner.model` this experiment
///   writes and **wins** for that role, so that role stays off the arm;
/// * the `is_cli_default_model_sentinel` short-circuit
///   (`model_resolution.rs:128-138`) returns before any alias resolution, so a
///   role pinned to the CLI-default sentinel is not on any arm at all.
///
/// A warning, never a refusal: a partially-covered workspace is still a valid
/// experiment subject as long as the operator knows.
#[must_use]
pub fn effectiveness_warnings(root: &Path, arm: &str) -> Vec<String> {
    use crate::role_runner::is_cli_default_model_sentinel;

    let config = crate::config_resolver::resolve_effective_config(root);
    let mut out = Vec::new();
    let role_runner = config.pointer("/autonomous/roleRunner");
    if let Some(models) = role_runner
        .and_then(|rr| rr.get("roleModels"))
        .and_then(Value::as_object)
    {
        for (role, value) in models {
            let Some(pin) = value.as_str() else { continue };
            if pin.trim().is_empty() {
                continue;
            }
            if is_cli_default_model_sentinel(pin) {
                out.push(format!(
                    "{}: role `{role}` is pinned to the CLI-default sentinel \
                     (autonomous.roleRunner.roleModels.{role}={pin}) — its ticks are on no arm, \
                     the {arm} assignment will not reach them",
                    root.display()
                ));
            } else {
                out.push(format!(
                    "{}: role `{role}` has its own pin \
                     (autonomous.roleRunner.roleModels.{role}={pin}), which outranks the \
                     experiment overlay — its ticks stay off the {arm} arm",
                    root.display()
                ));
            }
        }
    }
    if let Some(global) = role_runner
        .and_then(|rr| rr.get("model"))
        .and_then(Value::as_str)
    {
        if is_cli_default_model_sentinel(global) {
            out.push(format!(
                "{}: autonomous.roleRunner.model was the CLI-default sentinel ({global}); the \
                 experiment overlay replaces it for the duration and `stop` restores it",
                root.display()
            ));
        }
    }
    out
}

/// Warn when the overlay path is not gitignored in this repo — writing an arm
/// into a *tracked* file would put a host-local experiment into a commit.
/// Issue #8075 adds `.loom-local/` to the shipped `.gitignore`; until it has
/// landed everywhere, this says so per workspace.
#[must_use]
pub fn gitignore_warning(root: &Path) -> Option<String> {
    let out = super::super::run_git(root, &["check-ignore", "-q", LOCAL_CONFIG_REL]);
    match &out {
        crate::cmd_out::CmdOutcome::Ran(o) if o.status.success() => None,
        // Exit 1 is "not ignored" — the case worth a warning. Anything else
        // (not a git repo, git unavailable) is unknown, and an unknown is not
        // evidence of a problem.
        crate::cmd_out::CmdOutcome::Ran(o) if o.status.code() == Some(1) => Some(format!(
            "{}: {LOCAL_CONFIG_REL} is not gitignored here — the experiment overlay would show up \
             in `git status` (see #8075)",
            root.display()
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

/// What [`stop`] did.
#[derive(Debug, Default)]
pub struct StopReport {
    /// Overlays whose experiment keys were reversed in place.
    pub reverted: Vec<PathBuf>,
    /// Overlays deleted because reversing left them empty.
    pub deleted: Vec<PathBuf>,
    /// Workspaces left alone, with the reason.
    pub skipped: Vec<String>,
    /// Workspaces whose written values had been edited by hand.
    pub drifted: Vec<String>,
}

/// Reverse an experiment: restore every workspace's overlay to what it was
/// before `start`, and stamp the state file as stopped.
///
/// A drifted overlay — one whose written value is no longer what `start` wrote
/// — is reported and left in place unless `force`. Silently overwriting it
/// would discard a deliberate operator edit made during the experiment.
pub fn stop(
    state_dir: &Path,
    id: &str,
    force: bool,
    now: DateTime<Utc>,
) -> Result<(StopReport, ExperimentState)> {
    let path = state_path(state_dir, id);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("no experiment state at {}", path.display()))?;
    let mut state: ExperimentState =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    let mut report = StopReport::default();
    for w in &state.plan.workspaces {
        let overlay = overlay_path(Path::new(&w.path));
        let doc = match read_overlay(&overlay) {
            Ok(v) => v,
            Err(e) => {
                report.skipped.push(format!("{}: {e}", overlay.display()));
                continue;
            }
        };
        match revert_overlay_edit(&doc, id, force) {
            Revert::NotOurs => report
                .skipped
                .push(format!("{}: no {MARKER_KEY} marker (already stopped?)", overlay.display())),
            Revert::OtherExperiment(other) => report
                .skipped
                .push(format!("{}: owned by experiment {other}, left alone", overlay.display())),
            Revert::Drifted(keys) => report.drifted.push(format!(
                "{}: hand-edited at {} — left in place (use --force to remove anyway)",
                overlay.display(),
                keys.join(", ")
            )),
            Revert::Reverted(None) => {
                std::fs::remove_file(&overlay)
                    .with_context(|| format!("removing {}", overlay.display()))?;
                // Leave no `.loom-local/` shell behind either when `start`
                // created it. `remove_dir` refuses a non-empty directory, so a
                // workspace that keeps other host-local state there is safe.
                if let Some(parent) = overlay.parent() {
                    let _ = std::fs::remove_dir(parent);
                }
                report.deleted.push(overlay);
            }
            Revert::Reverted(Some(next)) => {
                super::super::write_json_file(&overlay, &next)
                    .with_context(|| format!("writing {}", overlay.display()))?;
                report.reverted.push(overlay);
            }
        }
    }

    if report.drifted.is_empty() {
        state.stopped_at = Some(now.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        super::super::write_json_file(&path, &serde_json::to_value(&state)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok((report, state))
}
