//! What a workspace pass tells the dispatch hold (#10719): one
//! [`Observation`] per registered workspace, covering both copies of the
//! installed files that dispatch executes.
//!
//! * **The default branch** is what the pass already classified. Its W state
//!   is the verdict: W4 holds as `daemon-too-old`, W3 as
//!   `install-incompatible`. W3 already means "too old AND the payload
//!   differs" (an empty diff is W0), so the hold keys on the diff. A W3 copy
//!   that is behind but compatible ([`behind_compatible`]) is not held
//!   (#11052); it stays W3, so the pass still resyncs it first. A
//!   repo-ahead workspace is clear unless what the gate refused is an
//!   interrupted newer resync, which holds and asks for no roll.
//! * **The checkout** is `<root>/.loom/install-metadata.json` in the working
//!   tree, gated exactly like the default branch's. The payload is diffed
//!   against the working tree only when the stamp is too old and not behind
//!   but compatible, and the answer is remembered per (HEAD, stamp). It is
//!   judged again right after the checkout step fast-forwards the checkout
//!   ([`rejudge_checkouts`]), so its hold clears in that same pass.
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
//! [`behind_compatible`]: crate::workspace_hold::behind_compatible

use std::path::{Path, PathBuf};

use super::{git, Env, LazyPayload, Memory, NotCurrent, WState, WorkspaceReport};
use crate::init::payload::{gate_metadata, materialize_with, ResyncRefusal};
use crate::install_compat::{
    Compat, DaemonCompat, InstallMeta, Version, INSTALL_METADATA_PATH, SUPPORTS_INSTALLED,
};
use crate::workspace_hold::{
    behind_compatible, decide_hold, Finding, HoldKind, Observation, Verdict,
};

/// What judging a copy needs: the running version, the floor and the
/// payload. A pass builds it from its [`Env`]; the re-judge after a
/// fast-forward builds it on its own.
pub(in crate::fleet_sync) struct Judge<'a> {
    pub running: Version,
    pub floor: Option<Version>,
    pub payload: &'a LazyPayload,
}

impl Judge<'_> {
    fn daemon(&self) -> DaemonCompat {
        DaemonCompat {
            running: self.running,
            supports_installed: Version::parse(SUPPORTS_INSTALLED).unwrap_or(self.running),
            floor: self.floor,
        }
    }
}

/// The observations for a finished pass: `reports` are its workspaces.
pub(super) fn observe(
    env: &Env<'_>,
    reports: &[WorkspaceReport],
    gate: Result<(), NotCurrent>,
    memory: &mut Memory,
) -> Vec<Observation> {
    let judge = Judge {
        running: env.running,
        floor: env.floor,
        payload: env.payload,
    };
    reports
        .iter()
        .map(|report| {
            let source = crate::init::is_loom_source_repo(&report.root);
            let default_branch = if source {
                source_default_branch(&judge, &report.root)
            } else {
                from_report(&judge, report)
            };
            let checkout = checkout(&judge, &report.root, source, gate, memory);
            Observation {
                root: report.root.clone(),
                repo: report.repo.clone(),
                default_branch: unless_source(source, default_branch),
                checkout: unless_source(source, checkout),
            }
        })
        .collect()
}

/// The Loom source repo is never resynced, so no W3 hold could clear there.
fn unless_source(source: bool, found: Finding) -> Finding {
    if source && found.verdict == Verdict::Hold(HoldKind::InstallIncompatible) {
        Finding::clear()
    } else {
        found
    }
}

/// The checkout copy of each of `roots`, judged now, for
/// [`crate::workspace_hold::rejudge`]: right after the checkout step
/// fast-forwarded them (#11052). The default-branch copy is
/// [`Finding::unknown`], so it keeps what the pass found.
pub(in crate::fleet_sync) fn checkout_observations(
    judge: &Judge<'_>,
    roots: &[PathBuf],
    gate: Result<(), NotCurrent>,
    memory: &mut Memory,
    nwo: &dyn Fn(&Path) -> Option<String>,
) -> Vec<Observation> {
    roots
        .iter()
        .map(|root| {
            let source = crate::init::is_loom_source_repo(root);
            Observation {
                root: root.clone(),
                repo: nwo(root),
                default_branch: Finding::unknown(),
                checkout: unless_source(source, checkout(judge, root, source, gate, memory)),
            }
        })
        .collect()
}

fn too_old(judge: &Judge<'_>, installed: Option<&str>) -> String {
    let floor = judge
        .floor
        .map_or_else(String::new, |f| format!(" or the fleet floor {f}"));
    format!(
        "installed {} is too old for daemon {}{floor}, its files differ from this daemon's \
         payload, and it records no requires_daemon this daemon meets",
        installed.unwrap_or("<unrecorded>"),
        judge.running
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
fn from_report(judge: &Judge<'_>, report: &WorkspaceReport) -> Finding {
    match report.state {
        WState::W4 => Finding::daemon_too_old(
            report.refusal.as_ref().map_or_else(
                || "the installed files need a newer daemon".to_string(),
                refusal_detail,
            ),
            report.requires_daemon.as_deref(),
            judge.running,
        ),
        // Behind, but its files say they work here: not held, still W3 to
        // the resync (#11052).
        WState::W3 if behind_compatible(&recorded(report), &judge.daemon()) => Finding::clear(),
        WState::W3 => Finding::install_incompatible(too_old(judge, report.installed.as_deref())),
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

/// The contract fields the pass recorded for the default branch.
fn recorded(report: &WorkspaceReport) -> InstallMeta {
    InstallMeta {
        loom_version: report.installed.clone(),
        requires_daemon: report.requires_daemon.clone(),
    }
}

/// A finding from a gate answer, for a copy the pass did not classify.
fn from_gate(
    judge: &Judge<'_>,
    meta: &InstallMeta,
    gate: &Result<Compat, ResyncRefusal>,
    diff_stale: Option<bool>,
) -> Finding {
    match decide_hold(gate, diff_stale, behind_compatible(meta, &judge.daemon())) {
        Verdict::Unknown => Finding::unknown(),
        Verdict::Clear => Finding::clear(),
        // `Ok` is W3; a refusal here is an interrupted newer resync.
        Verdict::Hold(HoldKind::InstallIncompatible) => Finding::install_incompatible(
            gate.as_ref()
                .err()
                .map_or_else(|| too_old(judge, meta.loom_version.as_deref()), refusal_detail),
        ),
        Verdict::Hold(HoldKind::DaemonTooOld) => Finding::daemon_too_old(
            gate.as_ref().err().map_or_else(String::new, refusal_detail),
            meta.requires_daemon.as_deref(),
            judge.running,
        ),
        // Never a pass verdict: the operator's mark, read from the registry
        // (#11186), never judged here.
        Verdict::Hold(HoldKind::MaintainOnly) => Finding::clear(),
    }
}

/// The Loom source repo's default branch, which the pass skips. Local refs
/// only. W4 only (see the module docs).
fn source_default_branch(judge: &Judge<'_>, root: &Path) -> Finding {
    let Some(branch) = git::default_branch(root) else {
        return Finding::unknown();
    };
    let Some(head) = git::tracking_head(root, &branch) else {
        return Finding::unknown();
    };
    match git::metadata_at(root, &head) {
        Ok(Some(raw)) => judged(judge, &raw, Some(false)),
        Ok(None) => Finding::clear(),
        Err(_) => Finding::unknown(),
    }
}

fn judged(judge: &Judge<'_>, raw: &str, diff_stale: Option<bool>) -> Finding {
    let gate = gate_metadata(raw, &judge.daemon());
    let meta = InstallMeta::parse(raw).unwrap_or_default();
    from_gate(judge, &meta, &gate, diff_stale)
}

/// The release an interrupted resync was heading for, if the metadata records
/// one (`resync_pending` present and not null).
fn pending_resync(raw: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    match value.get("resync_pending")? {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// The checkout copy: the working tree's install metadata.
fn checkout(
    judge: &Judge<'_>,
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
    let daemon = judge.daemon();
    // A half-applied resync is held whatever its versions say: the stamp is
    // written last, so it still names the old release (#11109).
    if let Some(pending) = pending_resync(&raw) {
        if gate_metadata(&raw, &daemon).is_ok() {
            return Finding::install_incompatible(format!(
                "a resync to {pending} was interrupted: the installed files are a mix of two releases"
            ));
        }
    }
    let too_old = gate_metadata(&raw, &daemon) == Ok(Compat::InstalledTooOld);
    // Behind but compatible is not held whatever the files are, so there is
    // no payload to unpack for it.
    let compatible = InstallMeta::parse(&raw).is_ok_and(|m| behind_compatible(&m, &daemon));
    let diff_stale = if !too_old || compatible {
        None
    } else if source {
        Some(false)
    } else if gate == Err(NotCurrent::NotAReleaseBuild) {
        // No release payload to diff against, so it cannot be known.
        None
    } else {
        checkout_stale(judge, root, &raw, memory)
    };
    judged(judge, &raw, diff_stale)
}

/// Does this daemon's payload differ from the working tree's installed
/// files? Remembered per (HEAD, stamp). `None` when it cannot be computed.
fn checkout_stale(judge: &Judge<'_>, root: &Path, raw: &str, memory: &mut Memory) -> Option<bool> {
    let key = format!("{}\n{raw}", git::head(root).unwrap_or_default());
    if let Some((_, stale)) = memory.checkout.get(root).filter(|(known, _)| *known == key) {
        return Some(*stale);
    }
    let payload = judge.payload.get().ok()?;
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
