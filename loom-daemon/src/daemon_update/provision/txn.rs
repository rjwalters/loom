//! The retained previous binary and the install transaction record (#10983,
//! slice 1 of #9735; the part of #9734 that #10708 left open).
//!
//! # What an install leaves beside the binary
//!
//! | File | What it is |
//! |---|---|
//! | `<dest>.previous` | The binary that was live before the last install: a byte-for-byte copy, mode 755. Exactly one is kept; the next install replaces it. |
//! | `<dest>.install-state.json` | The [`InstallRecord`] of the last install. |
//!
//! Both sit in the destination's own directory (`~/.local/bin/` by default),
//! so they are on its filesystem and a later restore can be a rename. Neither
//! is in `/tmp`, a repo or a worktree.
//!
//! # What this module does NOT do
//!
//! It never restores anything. Nothing here reads `<dest>.previous` back,
//! decides that an install failed, or acts on an unfinished record: those are
//! the later slices of #9735. This slice only guarantees that the copy and
//! the record exist for them.
//!
//! # The phases
//!
//! | Phase | Meaning | What `dest` holds |
//! |---|---|---|
//! | `staged` | The candidate is complete beside `dest` and the recovery copy is in place. | `previous` |
//! | `published` | The rename over `dest` was issued. | `target` |
//! | `committed` | `dest` was confirmed to be the staged file. The INSTALL finished. | `target` |
//!
//! `committed` says nothing about whether the new binary starts or is
//! healthy. That verdict belongs to the roll (`roll_attempt` in #9735), not
//! to the installer.
//!
//! One invariant holds from the moment a record exists, in every phase: when
//! `previous` is present, `<dest>.previous` holds bytes with that sha256. The
//! copy is renamed into place BEFORE the record is first written.
//!
//! # Reading the record
//!
//! [`read_record`] never fails an install. A missing file, unparsable JSON
//! and a `schema_version` this binary does not know are three distinct
//! answers ([`RecordRead`]), all of which mean "no usable record". Fields
//! this binary does not know are ignored. Each install writes a fresh record,
//! so an older binary installing after a newer one replaces the newer
//! record rather than editing it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::out;

/// The only record layout this binary writes or accepts.
pub const SCHEMA_VERSION: u64 = 1;

/// What is recorded when a binary does not answer `--version`.
pub const UNKNOWN_VERSION: &str = "unknown";

/// `<dest>.previous` — where the binary an install replaces is kept.
#[must_use]
pub fn previous_path(dest: &Path) -> PathBuf {
    with_suffix(dest, ".previous")
}

/// `<dest>.install-state.json` — the [`InstallRecord`] of the last install.
#[must_use]
pub fn record_path(dest: &Path) -> PathBuf {
    with_suffix(dest, ".install-state.json")
}

fn with_suffix(dest: &Path, suffix: &str) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// How far an install got. See the module doc for what each phase means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Staged,
    Published,
    Committed,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Staged => "staged",
            Phase::Published => "published",
            Phase::Committed => "committed",
        }
    }
}

/// One binary, identified by content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryIdentity {
    /// The first line of its `--version` output, or [`UNKNOWN_VERSION`].
    pub version: String,
    /// Lowercase hex sha256 of its bytes.
    pub sha256: String,
}

/// The binary an install replaced, and where its copy is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedBinary {
    /// The first line of its `--version` output, or [`UNKNOWN_VERSION`].
    pub version: String,
    /// Lowercase hex sha256 of the retained bytes.
    pub sha256: String,
    /// `<dest>.previous`.
    pub path: String,
}

/// The record of one install, written to [`record_path`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecord {
    /// [`SCHEMA_VERSION`].
    pub schema_version: u64,
    pub phase: Phase,
    /// The destination this install published to.
    pub dest: String,
    /// The binary being installed, as staged (so after any signing).
    pub target: BinaryIdentity,
    /// The binary that was live before, or `None` on a first-ever install.
    #[serde(default)]
    pub previous: Option<RetainedBinary>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Version of the `loom-daemon` that performed the install.
    #[serde(default)]
    pub installer_version: String,
}

/// What [`read_record`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordRead {
    /// No record file. Every host before its first install under #10983.
    Absent,
    /// The file is there but is not a record this binary can parse.
    Unreadable(String),
    /// A well-formed record with a `schema_version` this binary does not know.
    UnknownVersion(u64),
    Known(InstallRecord),
}

/// Read the install record beside `dest`. Never an error: see [`RecordRead`].
#[must_use]
pub fn read_record(dest: &Path) -> RecordRead {
    let text = match std::fs::read_to_string(record_path(dest)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RecordRead::Absent,
        Err(e) => return RecordRead::Unreadable(e.to_string()),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(e) => return RecordRead::Unreadable(e.to_string()),
    };
    match value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
    {
        Some(SCHEMA_VERSION) => {}
        Some(other) => return RecordRead::UnknownVersion(other),
        None => return RecordRead::Unreadable("no schema_version".to_string()),
    }
    match serde_json::from_value(value) {
        Ok(record) => RecordRead::Known(record),
        Err(e) => RecordRead::Unreadable(e.to_string()),
    }
}

/// Why an install did not happen. Every variant leaves the destination
/// exactly as it was.
#[derive(Debug)]
pub enum InstallError {
    /// The candidate could not be copied beside the destination, or read back.
    Stage(std::io::Error),
    /// The file handed to `publish` is not one `stage` made for this
    /// destination.
    NotStaged(PathBuf),
    /// The live binary could not be kept. The install is refused.
    Retain {
        previous: PathBuf,
        source: std::io::Error,
    },
    /// The record could not be written. The install is refused.
    Record {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The rename over the destination failed.
    Publish(std::io::Error),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const UNTOUCHED: &str = "The installed binary was left untouched.";
        match self {
            Self::Stage(e) => write!(
                f,
                "Could not stage the new binary beside the destination: {e}. {UNTOUCHED}"
            ),
            Self::NotStaged(p) => write!(
                f,
                "Refusing to publish {}: it is not a staged install file for this destination. {UNTOUCHED}",
                p.display()
            ),
            Self::Retain { previous, source } => write!(
                f,
                "Refusing to install: the current binary could not be kept at {} ({source}), so there would be nothing to roll back to. {UNTOUCHED} Free space or fix permissions in that directory, then retry.",
                previous.display()
            ),
            Self::Record { path, source } => write!(
                f,
                "Refusing to install: the install record {} could not be written ({source}). {UNTOUCHED}",
                path.display()
            ),
            Self::Publish(e) => write!(
                f,
                "Could not rename the staged binary over the destination: {e}. {UNTOUCHED}"
            ),
        }
    }
}

impl std::error::Error for InstallError {}

/// An install whose rename happened.
#[derive(Debug)]
pub struct Published {
    /// The record as last written. Phase `committed`, except when a
    /// concurrent install displaced this one (`published`).
    pub record: InstallRecord,
}

impl Published {
    /// One line naming the retained binary, or `None` on a first-ever install.
    #[must_use]
    pub fn retained_line(&self) -> Option<String> {
        let kept = self.record.previous.as_ref()?;
        Some(format!(
            "Kept the previous binary at {} ({}; sha256 {})",
            kept.path, kept.version, kept.sha256
        ))
    }
}

/// Everything `publish` does before its rename: identify the candidate, keep
/// the live binary, and write the record at phase `staged`.
pub(super) fn begin(staged: &Path, dest: &Path) -> Result<InstallRecord, InstallError> {
    note_earlier_record(dest);
    let target = identify(staged).map_err(InstallError::Stage)?;
    let previous = retain(dest, &target.sha256).map_err(|source| InstallError::Retain {
        previous: previous_path(dest),
        source,
    })?;
    let now = Utc::now();
    let record = InstallRecord {
        schema_version: SCHEMA_VERSION,
        phase: Phase::Staged,
        dest: dest.display().to_string(),
        target,
        previous,
        started_at: now,
        updated_at: now,
        installer_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    write_record(dest, &record).map_err(|source| InstallError::Record {
        path: record_path(dest),
        source,
    })?;
    Ok(record)
}

/// Move the record to `phase`. Best-effort: the binary is already published,
/// so a record that cannot be rewritten is reported and left one phase behind.
pub(super) fn advance(dest: &Path, record: &mut InstallRecord, phase: Phase) {
    record.phase = phase;
    record.updated_at = Utc::now();
    if let Err(e) = write_record(dest, record) {
        out::warn(&format!(
            "Could not update the install record {} to phase {}: {e}",
            record_path(dest).display(),
            phase.as_str()
        ));
    }
}

/// Say so, once, when the record already beside `dest` is unfinished or
/// unusable. Informational only: this slice never acts on it.
fn note_earlier_record(dest: &Path) {
    let path = record_path(dest);
    match read_record(dest) {
        RecordRead::Known(r) if r.phase != Phase::Committed => out::warn(&format!(
            "An earlier install to {} did not finish (recorded phase: {}). Continuing with this one.",
            dest.display(),
            r.phase.as_str()
        )),
        RecordRead::Unreadable(why) => out::warn(&format!(
            "Ignoring the unreadable install record {} ({why}); this install replaces it.",
            path.display()
        )),
        RecordRead::UnknownVersion(v) => out::warn(&format!(
            "Ignoring the install record {} (schema_version {v}, this binary knows {SCHEMA_VERSION}); this install replaces it.",
            path.display()
        )),
        RecordRead::Known(_) | RecordRead::Absent => {}
    }
}

/// Keep the binary at `dest` as `<dest>.previous`.
///
/// `Ok(None)` when there is no live binary (a first-ever install). When the
/// live binary is byte-identical to the candidate (`target_sha256`) nothing
/// is rotated and the EXISTING `<dest>.previous`, if any, is what is
/// reported: a repeated install must not replace the recovery copy with the
/// candidate itself.
///
/// The copy is written to a temp file, fsync'ed and renamed, never written
/// over `<dest>.previous` in place: a daemon that has not restarted since an
/// earlier install may still be executing that file.
fn retain(dest: &Path, target_sha256: &str) -> std::io::Result<Option<RetainedBinary>> {
    let mut live = match std::fs::File::open(dest) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let keep = previous_path(dest);
    let dir = super::staging_dir(dest);
    super::sweep_stale_staging(dir, &keep);
    let (tmp_path, mut tmp) = super::create_staging_file(dir, &keep)?;
    let guard = super::StagedFile(Some(tmp_path.clone()));

    let sha256 = copy_hashing(&mut live, &mut tmp)?;
    if sha256 == target_sha256 {
        drop(tmp);
        drop(guard);
        return existing(&keep);
    }
    super::make_executable(&tmp)?;
    tmp.sync_all()?;
    drop(tmp);
    // Read while `dest` still names the live binary.
    let version = version_of(dest);

    std::fs::rename(&tmp_path, &keep)?;
    guard.disarm();
    super::sync_dir(dir);
    Ok(Some(RetainedBinary {
        version,
        sha256,
        path: keep.display().to_string(),
    }))
}

/// Describe the `<dest>.previous` already on disk, if there is one.
fn existing(keep: &Path) -> std::io::Result<Option<RetainedBinary>> {
    match identify(keep) {
        Ok(id) => Ok(Some(RetainedBinary {
            version: id.version,
            sha256: id.sha256,
            path: keep.display().to_string(),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// sha256 and `--version` of the file at `path`.
fn identify(path: &Path) -> std::io::Result<BinaryIdentity> {
    let mut file = std::fs::File::open(path)?;
    let sha256 = copy_hashing(&mut file, &mut std::io::sink())?;
    Ok(BinaryIdentity {
        version: version_of(path),
        sha256,
    })
}

/// Copy `from` to `to`, returning the sha256 of exactly the bytes written.
fn copy_hashing(from: &mut impl Read, to: &mut impl Write) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        to.write_all(&buf[..n])?;
    }
    Ok(hex::encode(hasher.finalize()))
}

/// The first line of `<bin> --version`, or [`UNKNOWN_VERSION`].
///
/// A binary that cannot be executed, exits non-zero or prints nothing is
/// `unknown`, never an error: installing over a broken binary is a repair,
/// and it must not be refused for being broken.
fn version_of(bin: &Path) -> String {
    // A sibling thread forking while a just-written file's write fd was still
    // open makes its first exec fail with ETXTBSY until that child execs.
    for _ in 0..25 {
        let run = Command::new(bin)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match run {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                let line = text.lines().next().unwrap_or_default().trim();
                if !line.is_empty() {
                    return line.to_string();
                }
                break;
            }
            Err(e) if text_file_busy(&e) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => break,
        }
    }
    UNKNOWN_VERSION.to_string()
}

#[cfg(unix)]
fn text_file_busy(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::ETXTBSY)
}

#[cfg(not(unix))]
fn text_file_busy(_e: &std::io::Error) -> bool {
    false
}

/// Write the record atomically: a temp file beside it, fsync, rename, then
/// fsync the directory. A reader sees the old record or the new one.
fn write_record(dest: &Path, record: &InstallRecord) -> std::io::Result<()> {
    let path = record_path(dest);
    let dir = super::staging_dir(dest);
    let mut body = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    body.push(b'\n');

    super::sweep_stale_staging(dir, &path);
    let (tmp_path, mut tmp) = super::create_staging_file(dir, &path)?;
    let guard = super::StagedFile(Some(tmp_path.clone()));
    tmp.write_all(&body)?;
    tmp.sync_all()?;
    drop(tmp);
    std::fs::rename(&tmp_path, &path)?;
    guard.disarm();
    super::sync_dir(dir);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "txn_tests.rs"]
mod tests;
