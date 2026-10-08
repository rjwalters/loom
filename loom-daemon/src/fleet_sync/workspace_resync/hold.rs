//! What a workspace pass tells the dispatch hold (#10719): one
//! [`Observation`] per registered workspace, covering both copies of the
//! installed files that dispatch executes.
//!
//! * **The default branch** is what the pass already classified. Its W state
//!   is the verdict: W4 holds as `daemon-too-old`, W3 as
//!   `install-incompatible`. W3 already means "too old AND the payload
//!   differs" (an empty diff is W0), so the hold keys on the diff. A
//!   repo-ahead workspace is clear unless what the gate refused is an
//!   interrupted newer resync, which holds and asks for no roll.
//! * **The checkout** is `<root>/.loom/install-metadata.json` in the working
//!   tree, gated exactly like the default branch's. The payload is diffed
//!   against the working tree only when the stamp is too old, and the answer
//!   is remembered per (HEAD, stamp).
//!
//! A copy that could not be read this pass is [`Verdict::Unknown`], which
//! keeps whatever the last pass found.
//!
//! The versions a hold names come from the gate's own refusal and the
//! recorded `requires_daemon`, never from the report's `reason`: a failed pass
//! replaces that with its backoff, and a workspace in backoff is still held.
//!
//! The Loom source repo installs from its own tree and is never resynced, so
//! no `install-incompatible` hold could ever clear there; it is judged for
//! W4 only.
//!
//! [`Verdict::Unknown`]: crate::workspace_hold::Verdict::Unknown

use std::path::Path;

use super::{daemon, git, Env, Memory, NotCurrent, WState, WorkspaceReport};
use crate::init::payload::{gate_metadata, materialize_with, ResyncRefusal};
use crate::install_compat::{Compat, InstallMeta, INSTALL_METADATA_PATH};
use crate::workspace_hold::{decide_hold, Finding, HoldKind, Observation, Verdict};

/// The observations for a finished pass: `reports` are its workspaces.
pub(super) fn observe(
    env: &Env<'_>,
    reports: &[WorkspaceReport],
    gate: Result<(), NotCurrent>,
    memory: &mut Memory,
) -> Vec<Observation> {
    reports
        .iter()
        .map(|report| {
            let source = crate::init::is_loom_source_repo(&report.root);
            let default_branch = if source {
                source_default_branch(env, &report.root)
            } else {
                from_report(env, report)
            };
            let checkout = checkout(env, &report.root, source, gate, memory);
            let [default_branch, checkout] = [default_branch, checkout].map(|found| {
                let w3 = found.verdict == Verdict::Hold(HoldKind::InstallIncompatible);
                if source && w3 {
                    Finding::clear()
                } else {
                    found
                }
            });
            Observation {
                root: report.root.clone(),
                repo: report.repo.clone(),
                default_branch,
                checkout,
            }
        })
        .collect()
}

fn too_old(env: &Env<'_>, installed: Option<&str>) -> String {
    let floor = env
        .floor
        .map_or_else(String::new, |f| format!(" or the fleet floor {f}"));
    format!(
        "installed {} is too old for daemon {}{floor}, and its files differ from this daemon's \
         payload",
        installed.unwrap_or("<unrecorded>"),
        env.running
    )
}

/// What a refusal says to a person, with what an interrupted resync means
/// for dispatch spelled out.
fn refusal_detail(refusal: &ResyncRefusal) -> String {
    match refusal {
        ResyncRefusal::PendingAheadOfDaemon { .. } => format!(
            "{refusal}. Its files are a mix of two releases, so nothing is dispatched into it \
             until a host on that release finishes the resync; this host does not roll for it"
        ),
        other => other.to_string(),
    }
}

/// The default-branch copy, from the state the pass gave the workspace.
fn from_report(env: &Env<'_>, report: &WorkspaceReport) -> Finding {
    match report.state {
        WState::W4 => Finding::daemon_too_old(
            report.refusal.as_ref().map_or_else(
                || "the installed files need a newer daemon".to_string(),
                refusal_detail,
            ),
            report.requires_daemon.as_deref(),
            env.running,
        ),
        WState::W3 => Finding::install_incompatible(too_old(env, report.installed.as_deref())),
        // `RepoAhead` is the ratchet guard's case: ahead, yet compatible. The
        // one exception is an interrupted newer resync: held, and no demand.
        WState::RepoAhead => match &report.refusal {
            Some(refusal @ ResyncRefusal::PendingAheadOfDaemon { .. }) => {
                Finding::install_incompatible(refusal_detail(refusal))
            }
            _ => Finding::clear(),
        },
        WState::W0 | WState::W1 | WState::Skipped => Finding::clear(),
        WState::W2 | WState::Unknown => Finding::unknown(),
    }
}

/// A finding from a gate answer, for a copy the pass did not classify.
fn from_gate(
    env: &Env<'_>,
    meta: &InstallMeta,
    gate: &Result<Compat, ResyncRefusal>,
    diff_stale: Option<bool>,
) -> Finding {
    match decide_hold(gate, diff_stale) {
        Verdict::Unknown => Finding::unknown(),
        Verdict::Clear => Finding::clear(),
        // `Ok` is W3; a refusal here is an interrupted newer resync.
        Verdict::Hold(HoldKind::InstallIncompatible) => Finding::install_incompatible(
            gate.as_ref()
                .err()
                .map_or_else(|| too_old(env, meta.loom_version.as_deref()), refusal_detail),
        ),
        Verdict::Hold(HoldKind::DaemonTooOld) => Finding::daemon_too_old(
            gate.as_ref().err().map_or_else(String::new, refusal_detail),
            meta.requires_daemon.as_deref(),
            env.running,
        ),
    }
}

/// The Loom source repo's default branch, which the pass skips. Local refs
/// only. W4 only (see the module docs).
fn source_default_branch(env: &Env<'_>, root: &Path) -> Finding {
    let Some(branch) = git::default_branch(root) else {
        return Finding::unknown();
    };
    let Some(head) = git::tracking_head(root, &branch) else {
        return Finding::unknown();
    };
    match git::metadata_at(root, &head) {
        Ok(Some(raw)) => judge(env, &raw, Some(false)),
        Ok(None) => Finding::clear(),
        Err(_) => Finding::unknown(),
    }
}

fn judge(env: &Env<'_>, raw: &str, diff_stale: Option<bool>) -> Finding {
    let gate = gate_metadata(raw, &daemon(env));
    let meta = InstallMeta::parse(raw).unwrap_or_default();
    from_gate(env, &meta, &gate, diff_stale)
}

/// The checkout copy: the working tree's install metadata.
fn checkout(
    env: &Env<'_>,
    root: &Path,
    source: bool,
    gate: Result<(), NotCurrent>,
    memory: &mut Memory,
) -> Finding {
    let raw = match std::fs::read_to_string(root.join(INSTALL_METADATA_PATH)) {
        Ok(raw) => raw,
        // Loom is not installed in this checkout: nothing to be incompatible.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Finding::clear(),
        Err(_) => return Finding::unknown(),
    };
    let too_old = gate_metadata(&raw, &daemon(env)) == Ok(Compat::InstalledTooOld);
    let diff_stale = if !too_old {
        None
    } else if source {
        Some(false)
    } else if gate == Err(NotCurrent::NotAReleaseBuild) {
        // No release payload to diff against, so it cannot be known.
        None
    } else {
        checkout_stale(env, root, &raw, memory)
    };
    judge(env, &raw, diff_stale)
}

/// Does this daemon's payload differ from the working tree's installed
/// files? Remembered per (HEAD, stamp). `None` when it cannot be computed.
fn checkout_stale(env: &Env<'_>, root: &Path, raw: &str, memory: &mut Memory) -> Option<bool> {
    let key = format!("{}\n{raw}", git::head(root).unwrap_or_default());
    if let Some((_, stale)) = memory.checkout.get(root).filter(|(known, _)| *known == key) {
        return Some(*stale);
    }
    let payload = env.payload.get().ok()?;
    let diff = materialize_with(payload, root).ok()?;
    let stale = diff.stamp_pending() || {
        let paths: Vec<String> = diff
            .added
            .iter()
            .chain(&diff.changed)
            .chain(&diff.removed)
            .cloned()
            .collect();
        let ignored = git::ignored(root, root, &paths);
        paths.iter().any(|p| !ignored.contains(p))
    };
    memory.checkout.insert(root.to_path_buf(), (key, stale));
    Some(stale)
}
