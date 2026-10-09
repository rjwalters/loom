//! `loom-daemon resync-payload`: resync one workspace's working tree from the
//! payload embedded in this binary, on demand (#8961).
//!
//! `resync-installed.sh` needs a Loom SOURCE tree to copy from, and the only
//! pointer to one a consumer repo carries is the gitignored
//! `.loom/loom-source-path` sidecar. A clone that never ran the installer has
//! none, so the script used to stop there and the repo drifted. A host with a
//! `loom-daemon` always has that release's files inside the binary, so the
//! script now hands an unresolved source to this command instead.
//!
//! Nothing here is new resync logic: the gate, the diff and the apply are the
//! parent module's, unchanged ([`plan_workspace_with`], [`apply`]). So every
//! rule there holds on this path too: never a downgrade, only a verified
//! official release build applies, `.loom/resync-ignore` pins and symlinked
//! targets are left alone, an empty diff writes nothing (not even the
//! stamp), and the stamp never records `unknown`.
//!
//! What this adds is the caller: a report of what changed, a `--dry-run`
//! that stops after the diff, and the release-tag check that the fleet-sync
//! pass otherwise does once per daemon process. It writes the working tree
//! only. Committing, pushing and the per-workspace claim stay with the
//! fleet-sync pass (#10718).
//!
//! # Exit codes
//!
//! They are `resync-installed.sh`'s own, so the script passes them through:
//! [`EXIT_OK`] applied or already in sync, [`EXIT_FAILED`] refused or failed
//! (nothing written on a refusal), [`EXIT_DRIFT`] `--dry-run` found changes.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use super::surfaces::{SkipReason, INSTALL_TIME_ONLY};
use super::{apply, plan_workspace_with, Payload, ResyncOutcome, ResyncRefusal};

/// Applied, or nothing to do.
pub const EXIT_OK: i32 = 0;
/// Refused by the gate, or the resync failed.
pub const EXIT_FAILED: i32 = 1;
/// `--dry-run` only: one or more files would change.
pub const EXIT_DRIFT: i32 = 2;

/// The files a resync writes, by what happens to each. Repo-relative, sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// In the payload, not in the workspace.
    pub added: Vec<String>,
    /// In both, with different bytes or a different executable bit.
    pub changed: Vec<String>,
    /// Loom-owned files the payload no longer ships.
    pub removed: Vec<String>,
}

impl Changes {
    /// How many files are named.
    #[must_use]
    pub fn len(&self) -> usize {
        self.added.len() + self.changed.len() + self.removed.len()
    }

    /// No file is named. A resync can still be owed with none: an earlier
    /// run that never reached its stamp.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What one run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The gate refused. Nothing was written.
    Refused(ResyncRefusal),
    /// The installed files already equal the payload. Nothing was written.
    InSync,
    /// `--dry-run`: these would be written. Nothing was.
    WouldChange(Changes),
    /// These were written, and the metadata re-stamped.
    Applied(Changes),
}

/// One run's result and what to print for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// What happened.
    pub verdict: Verdict,
    /// The payload's release.
    pub version: String,
    /// Surfaces left as the workspace has them, with the reason.
    pub skipped: Vec<String>,
    /// Why this binary is not a release build, for that refusal.
    pub provenance: Option<String>,
}

impl Report {
    /// The process exit code for this result.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match &self.verdict {
            Verdict::InSync | Verdict::Applied(_) => EXIT_OK,
            Verdict::Refused(_) => EXIT_FAILED,
            Verdict::WouldChange(_) => EXIT_DRIFT,
        }
    }

    /// The gate refused: the caller prints [`Self::render`] to stderr.
    #[must_use]
    pub fn refused(&self) -> bool {
        matches!(self.verdict, Verdict::Refused(_))
    }

    /// The report, one line per file, ending in a newline.
    #[must_use]
    pub fn render(&self, dest: &Path) -> String {
        let mut out = String::new();
        let v = &self.version;
        let (changes, dry) = match &self.verdict {
            Verdict::Refused(refusal) => {
                let _ = writeln!(
                    out,
                    "resync-payload: {}: refused, nothing written: {refusal}",
                    dest.display()
                );
                if let Some(why) = &self.provenance {
                    let _ = writeln!(out, "resync-payload: {why}");
                }
                return out;
            }
            Verdict::InSync => (None, false),
            Verdict::WouldChange(c) => (Some(c), true),
            Verdict::Applied(c) => (Some(c), false),
        };
        let _ = writeln!(
            out,
            "resync-payload: {}: from the payload embedded in loom-daemon {v} (no Loom source \
             tree is read)",
            dest.display()
        );
        if let Some(c) = changes {
            let would = if dry { "would " } else { "" };
            for (verb, paths) in [
                ("add", &c.added),
                ("update", &c.changed),
                ("remove", &c.removed),
            ] {
                for path in paths {
                    let _ = writeln!(out, "  {would}{verb} {path}");
                }
            }
        }
        for skip in &self.skipped {
            let _ = writeln!(out, "  skipped {skip}");
        }
        match (changes, dry) {
            (None, _) => {
                let _ = writeln!(out, "resync-payload: already in sync with {v}; nothing written.");
            }
            (Some(c), true) if c.is_empty() => {
                let _ = writeln!(
                    out,
                    "resync-payload: DRY RUN: an earlier resync never reached its stamp; a run \
                     would stamp {v}. Nothing was written."
                );
            }
            (Some(c), true) => {
                let _ = writeln!(
                    out,
                    "resync-payload: DRY RUN: {} file(s) would change and the install would be \
                     stamped {v}. Nothing was written; run without --dry-run to apply.",
                    c.len()
                );
            }
            (Some(c), false) => {
                let _ = writeln!(
                    out,
                    "resync-payload: {} file(s) written; install stamped {v}. Review and commit \
                     the result.",
                    c.len()
                );
            }
        }
        let left: Vec<&str> = INSTALL_TIME_ONLY.iter().map(|(path, _)| *path).collect();
        let _ = writeln!(
            out,
            "resync-payload: never refreshed on this path (the installer, or \
             resync-installed.sh run with a Loom source tree, owns them): {}",
            left.join(", ")
        );
        out
    }
}

/// Resync `dest` from `payload`, or with `dry_run` only say what would
/// change.
///
/// # Errors
/// Staging or reading `dest` fails, or a write does (see [`apply`]: files
/// already written stay, the stamp is left alone, and the next run retries).
pub fn run_with(payload: &Payload, dest: &Path, dry_run: bool) -> Result<Report> {
    let mut report = Report {
        verdict: Verdict::InSync,
        version: payload.stamp().version.to_string(),
        skipped: Vec::new(),
        provenance: None,
    };
    let diff = match plan_workspace_with(payload, dest)? {
        Ok(diff) => diff,
        Err(refusal) => {
            report.verdict = Verdict::Refused(refusal);
            return Ok(report);
        }
    };
    report.skipped = diff
        .skipped_surfaces
        .iter()
        .map(|skip| match &skip.why {
            SkipReason::Unusable(why) => format!("{} ({why})", skip.path),
            SkipReason::NoInstallDate => format!(
                "{} (records no install date, so it is not re-rendered; reinstall to refresh it)",
                skip.path
            ),
        })
        .collect();
    if diff.is_empty() {
        return Ok(report);
    }
    let changes = Changes {
        added: diff.added.clone(),
        changed: diff.changed.clone(),
        removed: diff.removed.clone(),
    };
    if dry_run {
        report.verdict = Verdict::WouldChange(changes);
        return Ok(report);
    }
    report.verdict = match apply(dest, &diff)? {
        ResyncOutcome::Refused(refusal) => Verdict::Refused(refusal),
        ResyncOutcome::Unchanged => Verdict::InSync,
        ResyncOutcome::Applied { .. } => Verdict::Applied(changes),
    };
    Ok(report)
}

/// [`run_with`] for the payload embedded in this binary.
///
/// Asks the forge what this binary's release tag names first, at most once
/// and only for a binary the release workflow stamped
/// ([`crate::release_provenance`]): a one-shot process has no earlier answer
/// to reuse. Any other build, and a stamped one whose tag gets no answer, is
/// refused by the gate like everywhere else.
///
/// # Errors
/// `dest` is not a directory, the payload does not unpack, or see
/// [`run_with`].
pub fn run(dest: &Path, dry_run: bool) -> Result<Report> {
    anyhow::ensure!(dest.is_dir(), "{} is not a directory", dest.display());
    let provenance =
        crate::release_provenance::ensure(chrono::Utc::now(), Duration::ZERO, &|repo, tag| {
            crate::release_fetch::source::resolve_tag_commit(
                &|path| crate::release_fetch::source::gh_api(dest, path),
                repo,
                tag,
            )
        });
    let payload = Payload::embedded().context("unpack the embedded payload")?;
    let mut report = run_with(&payload, dest, dry_run)?;
    if report.verdict == Verdict::Refused(ResyncRefusal::NotAReleaseBuild) {
        report.provenance = Some(provenance.to_string());
    }
    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "standalone_tests.rs"]
mod tests;
