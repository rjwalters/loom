//! The install payload of the RUNNING release, and the resync that diffs and
//! applies it (#10717, tracker #10698 Phase 3 step 2).
//!
//! # Where the files come from
//!
//! `build.rs` packs the tracked `defaults/` tree into a `.tar.zst` and this
//! module embeds it ([`Payload::embedded`]). A resync therefore installs the
//! files of the binary that is running, never `~/GitHub/loom`: the
//! release-download update path deliberately does not fast-forward that
//! checkout (`daemon_update/sync.rs`), so it can sit at any version. Nothing
//! here calls `resolve_defaults_path`, reads `.loom/loom-source-path`, or
//! consults `LOOM_MACHINE_CHECKOUT` / `LOOM_DAEMON_DEFAULTS_DIR`.
//!
//! # What "the installed files" are
//!
//! Exactly what `init` writes in [`super::install_payload_files`] (the README,
//! the rate card, `.loom/{roles,scripts,hooks,docs,runtimes}/`, the
//! `defaults/.loom/` walk) plus `.claude/commands/loom/`, which `init` copies
//! as part of `.claude/`. [`materialize_payload`] copies those surfaces of a
//! workspace into a staging tree, runs that same installer step over the
//! staging tree, and diffs the result against the workspace. So the ownership
//! rules (#5971: only Loom-owned files are ever removed), the internal skip
//! list and the executable bits are the installer's own.
//!
//! Consumer configuration (`.loom/config.json`), the template-substituted
//! scaffolding (`.loom/CLAUDE.md` carries an install date, so regenerating it
//! would never be an empty diff), `.gitignore` and `.agents/skills/` are not
//! part of the payload diff.
//!
//! # What a resync writes
//!
//! [`apply`] writes nothing at all when the diff is empty: no file, no
//! metadata, not even a version bump, so `loom_version` changes only when
//! files do. A non-empty diff writes exactly the added and changed files,
//! deletes the removed ones, and re-stamps `loom_version`, `loom_commit`,
//! `requires_daemon` and `last_resync` in `.loom/install-metadata.json`
//! (every other key, `installed_files` included, is kept).
//!
//! # Never a downgrade
//!
//! [`resync_gate`] refuses before anything is written when the workspace is
//! ahead of this daemon: its `requires_daemon` is above the running version
//! (W4), or its `loom_version` is (the repo was resynced by a newer host).
//! `classify` alone is not enough for the second case: a repo at 0.19.900
//! that requires 0.19.772 classifies as `Compatible` against a 0.19.880
//! daemon, and resyncing it from this payload would roll it back. Both
//! refusals are the "repo ahead of daemon" input the claim (#10718) and the
//! dispatch hold (#10719, D1) act on.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use tempfile::TempDir;

use super::file_ops::force_merge_dir_with_report_filtered;
use super::repo_owned::OwnershipBoundary;
use super::scaffolding::load_internal_skip_list;
use super::{install_payload_files, is_transient_artifact, InitReport, LOOM_TREE_SCAFFOLDED_FILES};
use crate::install_compat::{
    classify, Compat, DaemonCompat, InstallMeta, Version, INSTALL_METADATA_PATH, REQUIRES_DAEMON,
};

/// The packed `defaults/` tree this binary was built from (see `build.rs`).
static EMBEDDED_PAYLOAD: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/install-payload.tar.zst"));

/// The `.loom/` directories [`super::install_payload_files`] syncs by name.
/// `payload_surface_covers_everything_the_installer_writes` (tests) fails if
/// that step starts writing somewhere [`surface_roots`] does not look.
const MANAGED_DIRS: &[&str] = &["roles", "scripts", "hooks", "docs", "runtimes"];

/// Slash commands: copied by `init` as part of `.claude/`, and the one
/// `.claude/` surface that is pure payload.
const COMMANDS_DIR: &str = ".claude/commands/loom";

/// The pin file. Copied into staging (ownership evidence) but never diffed.
const RESYNC_IGNORE_PATH: &str = ".loom/resync-ignore";

/// The values a resync stamps into `install-metadata.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// The release whose files these are: the running daemon's version.
    pub version: Version,
    /// Full 40-hex source commit, when the build knew it. `None` leaves the
    /// recorded `loom_commit` alone rather than writing `"unknown"` (#9613).
    pub commit: Option<String>,
    /// The oldest daemon these files work with.
    pub requires_daemon: String,
}

impl Stamp {
    /// This binary's stamp. `None` only if the crate version is not
    /// `MAJOR.MINOR.PATCH`, which `install_compat`'s tests rule out.
    #[must_use]
    pub fn this_binary() -> Option<Self> {
        let full = crate::self_update::BUILT_COMMIT_FULL;
        let commit = (full.len() == 40 && full.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| full.to_string());
        Some(Self {
            version: Version::parse(env!("CARGO_PKG_VERSION"))?,
            commit,
            requires_daemon: REQUIRES_DAEMON.to_string(),
        })
    }
}

/// An installable `defaults/` tree on disk, and the stamp it installs as.
#[derive(Debug)]
pub struct Payload {
    /// Keeps the unpacked tree alive; `None` for a caller-owned tree.
    _dir: Option<TempDir>,
    defaults: PathBuf,
    stamp: Stamp,
}

impl Payload {
    /// Unpack the payload embedded in this binary into a private temp dir.
    ///
    /// # Errors
    /// The temp dir cannot be created, or the embedded archive does not
    /// unpack (a build defect).
    pub fn embedded() -> Result<Self> {
        let stamp = Stamp::this_binary().ok_or_else(|| anyhow!("crate version is not X.Y.Z"))?;
        let dir = tempfile::Builder::new()
            .prefix("loom-payload-")
            .tempdir()
            .context("create the payload temp dir")?;
        let tar = zstd::decode_all(EMBEDDED_PAYLOAD).context("decompress the embedded payload")?;
        let mut archive = tar::Archive::new(tar.as_slice());
        archive.set_preserve_permissions(true);
        archive.set_preserve_mtime(false);
        archive
            .unpack(dir.path())
            .context("unpack the embedded payload")?;
        let defaults = dir.path().join("defaults");
        anyhow::ensure!(defaults.is_dir(), "the embedded payload has no defaults/ tree");
        Ok(Self {
            _dir: Some(dir),
            defaults,
            stamp,
        })
    }

    /// A payload rooted at an existing `defaults/`-shaped tree. For tests and
    /// for callers that already hold an unpacked payload.
    #[must_use]
    pub fn from_defaults(defaults: PathBuf, stamp: Stamp) -> Self {
        Self {
            _dir: None,
            defaults,
            stamp,
        }
    }

    /// The `defaults/` root of this payload.
    #[must_use]
    pub fn defaults(&self) -> &Path {
        &self.defaults
    }

    /// The stamp this payload installs as.
    #[must_use]
    pub fn stamp(&self) -> &Stamp {
        &self.stamp
    }
}

/// What a resync would change in a workspace. Paths are repo-relative,
/// sorted. Holds the staged files until [`apply`] has copied them.
#[derive(Debug)]
pub struct PayloadDiff {
    /// In the payload, not in the workspace.
    pub added: Vec<String>,
    /// In both, with different bytes or a different executable bit.
    pub changed: Vec<String>,
    /// Loom-owned files in the workspace that the payload no longer ships.
    pub removed: Vec<String>,
    stamp: Stamp,
    staging: TempDir,
}

impl PayloadDiff {
    /// No file would change.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    /// The stamp [`apply`] would write.
    #[must_use]
    pub fn stamp(&self) -> &Stamp {
        &self.stamp
    }
}

/// Why a resync refused to touch a workspace. Nothing is written in any case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResyncRefusal {
    /// The workspace has no `.loom/install-metadata.json`: Loom is not
    /// installed there, so there is nothing to resync.
    NotInstalled,
    /// The metadata exists but does not parse.
    UnreadableMetadata(String),
    /// The Loom source checkout installs from its own `defaults/` (dogfood
    /// symlinks); a resync never writes there.
    LoomSourceRepo,
    /// The installed files declare a `requires_daemon` above this daemon
    /// (W4). The host has to roll forward first.
    NeedsNewerDaemon {
        /// The installed files' `requires_daemon`.
        requires: Version,
        /// This daemon's running version.
        running: Version,
    },
    /// The installed `loom_version` is above this daemon's: a newer host
    /// already resynced it, and this payload would roll it back. Fix-forward
    /// (#10698) never downgrades a repo; the host has to roll forward first
    /// (D1, `repo_ahead_target`).
    RepoAheadOfDaemon {
        /// The installed `loom_version`.
        installed: Version,
        /// This daemon's running version.
        running: Version,
    },
}

impl ResyncRefusal {
    /// True for the two "repo ahead of daemon" refusals: the inputs to the
    /// host's `repo_ahead_target` and the `daemon-too-old` dispatch hold.
    #[must_use]
    pub fn repo_ahead_of_daemon(&self) -> bool {
        matches!(self, Self::NeedsNewerDaemon { .. } | Self::RepoAheadOfDaemon { .. })
    }
}

impl std::fmt::Display for ResyncRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => write!(f, "Loom is not installed (no {INSTALL_METADATA_PATH})"),
            Self::UnreadableMetadata(e) => write!(f, "{INSTALL_METADATA_PATH} is unreadable: {e}"),
            Self::LoomSourceRepo => write!(f, "the Loom source checkout is never resynced"),
            Self::NeedsNewerDaemon { requires, running } => write!(
                f,
                "repo ahead of daemon: installed files require daemon {requires}, running {running}"
            ),
            Self::RepoAheadOfDaemon { installed, running } => write!(
                f,
                "repo ahead of daemon: installed loom_version {installed} is newer than running \
                 {running}; resyncing would downgrade it"
            ),
        }
    }
}

/// The result of [`apply`] / [`resync_workspace`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResyncOutcome {
    /// Refused before writing anything.
    Refused(ResyncRefusal),
    /// The installed files already equal this payload. Nothing was written.
    Unchanged,
    /// The diff was applied. `written` is every repo-relative path touched
    /// (created, overwritten or deleted), metadata last.
    Applied {
        /// Sorted payload paths, then `.loom/install-metadata.json`.
        written: Vec<String>,
    },
}

/// May a payload of version `running` be applied over `installed`? Pure.
///
/// `Ok` carries the classification for the caller's W state. A missing or
/// unparseable `loom_version` is the migration case (`ResyncOwed`) and
/// proceeds: there is no recorded version to be ahead of.
///
/// # Errors
/// The refusal, when the workspace is ahead of this daemon.
pub fn resync_gate(
    installed: &InstallMeta,
    daemon: &DaemonCompat,
) -> Result<Compat, ResyncRefusal> {
    let compat = classify(installed, daemon);
    if compat == Compat::NeedsNewerDaemon {
        // `classify` only returns this for a parsed `requires_daemon`.
        let requires = installed
            .requires_daemon
            .as_deref()
            .and_then(Version::parse)
            .unwrap_or(daemon.running);
        return Err(ResyncRefusal::NeedsNewerDaemon {
            requires,
            running: daemon.running,
        });
    }
    if let Some(v) = installed.loom_version.as_deref().and_then(Version::parse) {
        if v > daemon.running {
            return Err(ResyncRefusal::RepoAheadOfDaemon {
                installed: v,
                running: daemon.running,
            });
        }
    }
    Ok(compat)
}

/// Stage this binary's embedded payload against `dest` and report the diff.
/// Writes only to a private temp dir; `dest` is not touched.
///
/// # Errors
/// The payload cannot be unpacked, or staging / reading `dest` fails.
pub fn materialize_payload(dest: &Path) -> Result<PayloadDiff> {
    materialize_with(&Payload::embedded()?, dest)
}

/// [`materialize_payload`] for an explicit payload.
///
/// # Errors
/// Staging or reading `dest` fails, or the installer step fails.
pub fn materialize_with(payload: &Payload, dest: &Path) -> Result<PayloadDiff> {
    let staging = tempfile::Builder::new()
        .prefix("loom-resync-staging-")
        .tempdir()
        .context("create the resync staging dir")?;
    let stage = staging.path();
    let roots = surface_roots(payload.defaults());

    // Seed staging with the workspace's current surfaces plus the ownership
    // evidence, so the installer's ownership-gated clean sees what it would
    // see in the workspace itself. Symlinks are never copied (a dogfood
    // symlink would make the installer write through it, out of staging) and
    // their paths are left out of the diff.
    let mut skipped: Vec<String> = Vec::new();
    for rel in roots
        .iter()
        .map(String::as_str)
        .chain([INSTALL_METADATA_PATH, RESYNC_IGNORE_PATH])
    {
        seed(&dest.join(rel), &stage.join(rel), rel, &mut skipped)
            .with_context(|| format!("stage {rel}"))?;
    }

    let shipped =
        install_into(payload.defaults(), stage).map_err(|e| anyhow!("stage the payload: {e}"))?;

    let ownership = OwnershipBoundary::load(dest);
    let mut diff = PayloadDiff {
        added: Vec::new(),
        changed: Vec::new(),
        removed: Vec::new(),
        stamp: payload.stamp().clone(),
        staging,
    };
    let mut staged = BTreeMap::new();
    let mut installed = BTreeMap::new();
    for rel in &roots {
        collect_files(&diff.staging.path().join(rel), rel, &mut staged)?;
        collect_files(&dest.join(rel), rel, &mut installed)?;
    }
    let paths: BTreeSet<&String> = staged.keys().chain(installed.keys()).collect();
    for path in paths {
        if diff_excludes(path, &skipped, &ownership) {
            continue;
        }
        match (staged.get(path), installed.get(path)) {
            (Some(_), None) => diff.added.push(path.clone()),
            (None, Some(_)) => diff.removed.push(path.clone()),
            // Only a file the installer itself wrote can be "changed". Its
            // `chmod +x` pass also reaches repo-owned `*.sh` in the managed
            // dirs; a resync leaves those exactly as the repo has them.
            (Some(s), Some(i)) if shipped.contains(path) && !same_file(s, i)? => {
                diff.changed.push(path.clone());
            }
            _ => {}
        }
    }
    Ok(diff)
}

/// Write `diff` into `dest`. An empty diff writes nothing and returns
/// [`ResyncOutcome::Unchanged`]. The downgrade gate is re-checked against the
/// workspace's metadata first, with the diff's own version as the running
/// one, so no caller can apply an older payload over a newer install.
///
/// # Errors
/// A file cannot be written or removed, or the metadata cannot be re-stamped.
/// Files already written stay written; the next resync converges them.
pub fn apply(dest: &Path, diff: &PayloadDiff) -> Result<ResyncOutcome> {
    if diff.is_empty() {
        return Ok(ResyncOutcome::Unchanged);
    }
    if let Err(refusal) = gate_workspace(dest, &diff.stamp.version) {
        log::warn!("resync: {}: refused, nothing written: {refusal}", dest.display());
        return Ok(ResyncOutcome::Refused(refusal));
    }

    let mut written: Vec<String> = Vec::new();
    for rel in diff.added.iter().chain(&diff.changed) {
        write_file(&diff.staging.path().join(rel), &dest.join(rel))
            .with_context(|| format!("write {rel}"))?;
        written.push(rel.clone());
    }
    for rel in &diff.removed {
        let path = dest.join(rel);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {rel}")),
        }
        prune_empty_parents(dest, rel);
        written.push(rel.clone());
    }
    written.sort();
    restamp_metadata(dest, &diff.stamp).context("re-stamp install metadata")?;
    written.push(INSTALL_METADATA_PATH.to_string());

    log::info!(
        "resync: {}: applied payload {} ({} added, {} changed, {} removed)",
        dest.display(),
        diff.stamp.version,
        diff.added.len(),
        diff.changed.len(),
        diff.removed.len()
    );
    Ok(ResyncOutcome::Applied { written })
}

/// Gate, materialize and apply this binary's payload over `dest`'s working
/// tree. The single entry point for a resync; committing and pushing the
/// result (and the per-workspace claim) are the caller's (#10718).
///
/// # Errors
/// See [`materialize_payload`] and [`apply`].
pub fn resync_workspace(dest: &Path) -> Result<ResyncOutcome> {
    let payload = Payload::embedded()?;
    resync_workspace_with(&payload, dest)
}

/// [`resync_workspace`] for an explicit payload.
///
/// # Errors
/// See [`materialize_with`] and [`apply`].
pub fn resync_workspace_with(payload: &Payload, dest: &Path) -> Result<ResyncOutcome> {
    if let Err(refusal) = gate_workspace(dest, &payload.stamp().version) {
        log::warn!("resync: {}: refused, nothing written: {refusal}", dest.display());
        return Ok(ResyncOutcome::Refused(refusal));
    }
    let diff = materialize_with(payload, dest)?;
    if diff.is_empty() {
        log::info!(
            "resync: {}: installed files already match payload {}; nothing written",
            dest.display(),
            payload.stamp().version
        );
        return Ok(ResyncOutcome::Unchanged);
    }
    apply(dest, &diff)
}

/// [`resync_gate`] over a workspace's working-tree metadata.
fn gate_workspace(dest: &Path, running: &Version) -> Result<Compat, ResyncRefusal> {
    if super::is_loom_source_repo(dest) {
        return Err(ResyncRefusal::LoomSourceRepo);
    }
    let raw = match fs::read_to_string(dest.join(INSTALL_METADATA_PATH)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(ResyncRefusal::NotInstalled),
        Err(e) => return Err(ResyncRefusal::UnreadableMetadata(e.to_string())),
    };
    let installed =
        InstallMeta::parse(&raw).map_err(|e| ResyncRefusal::UnreadableMetadata(e.to_string()))?;
    let daemon = DaemonCompat {
        running: *running,
        supports_installed: Version::parse(crate::install_compat::SUPPORTS_INSTALLED)
            .unwrap_or(*running),
        floor: None,
    };
    resync_gate(&installed, &daemon)
}

/// Every repo-relative path the payload step can write, in a stable order.
fn surface_roots(defaults: &Path) -> Vec<String> {
    let mut roots = vec![
        ".loom/README.md".to_string(),
        ".loom/pricing.json".to_string(),
    ];
    roots.extend(MANAGED_DIRS.iter().map(|d| format!(".loom/{d}")));
    if let Ok(entries) = fs::read_dir(defaults.join(".loom")) {
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                !is_transient_artifact(n) && !LOOM_TREE_SCAFFOLDED_FILES.contains(&n.as_str())
            })
            .collect();
        names.sort();
        roots.extend(names.into_iter().map(|n| format!(".loom/{n}")));
    }
    roots.push(COMMANDS_DIR.to_string());
    roots.dedup();
    roots
}

/// The installer step, run over a staging tree. Returns the repo-relative
/// paths it wrote: the files this payload ships.
fn install_into(defaults: &Path, stage: &Path) -> Result<BTreeSet<String>, String> {
    let loom = stage.join(".loom");
    fs::create_dir_all(&loom).map_err(|e| format!("create {}: {e}", loom.display()))?;
    let mut report = InitReport::default();
    install_payload_files(stage, defaults, &loom, true, &mut report)?;

    let commands_src = defaults.join(COMMANDS_DIR);
    if commands_src.is_dir() {
        let skip = load_internal_skip_list(defaults);
        force_merge_dir_with_report_filtered(
            &commands_src,
            &stage.join(COMMANDS_DIR),
            COMMANDS_DIR,
            &mut report,
            &|rel| skip.contains(rel),
        )
        .map_err(|e| format!("stage {COMMANDS_DIR}: {e}"))?;
    }
    Ok(report.added.into_iter().chain(report.updated).collect())
}

/// Copy `src` (file or directory) to `dst`, recording symlinks instead of
/// copying them. A missing `src` is not an error.
fn seed(src: &Path, dst: &Path, rel: &str, skipped: &mut Vec<String>) -> io::Result<()> {
    let meta = match fs::symlink_metadata(src) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        skipped.push(rel.to_string());
    } else if ft.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let name = entry.file_name();
            let child_rel = format!("{rel}/{}", name.to_string_lossy());
            seed(&entry.path(), &dst.join(&name), &child_rel, skipped)?;
        }
    } else if ft.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
    }
    Ok(())
}

/// Regular files under `root` (a file or a directory), keyed by repo-relative
/// path. Symlinks are not followed and not listed.
fn collect_files(root: &Path, rel: &str, out: &mut BTreeMap<String, PathBuf>) -> io::Result<()> {
    let meta = match fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_dir() {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let child_rel = format!("{rel}/{}", entry.file_name().to_string_lossy());
            collect_files(&entry.path(), &child_rel, out)?;
        }
    } else if meta.file_type().is_file() {
        out.insert(rel.to_string(), root.to_path_buf());
    }
    Ok(())
}

/// Left out of the diff: under a symlink the workspace keeps, or pinned in
/// `.loom/resync-ignore` (never overwritten, never removed).
fn diff_excludes(path: &str, skipped: &[String], ownership: &OwnershipBoundary) -> bool {
    skipped.iter().any(|s| {
        path == s
            || path
                .strip_prefix(s.as_str())
                .is_some_and(|r| r.starts_with('/'))
    }) || ownership.is_declared_repo_owned(path)
}

fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    Ok(fs::read(a)? == fs::read(b)? && is_executable(a)? == is_executable(b)?)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

/// Replace `dst` with `src`'s bytes and mode, via a sibling temp file and a
/// rename, so a reader never sees a half-written script.
fn write_file(src: &Path, dst: &Path) -> io::Result<()> {
    let parent = dst
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    fs::create_dir_all(parent)?;
    let name = dst
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let tmp = parent.join(format!(".{name}.loom-resync.tmp"));
    fs::copy(src, &tmp)?;
    fs::set_permissions(&tmp, fs::metadata(src)?.permissions())?;
    fs::rename(&tmp, dst).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Remove directories emptied by a removal, up to (not including) the
/// workspace root. `remove_dir` refuses a non-empty directory, which stops it.
fn prune_empty_parents(dest: &Path, rel: &str) {
    let mut current = Path::new(rel).parent();
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() || fs::remove_dir(dest.join(dir)).is_err() {
            break;
        }
        current = dir.parent();
    }
}

/// Re-stamp the contract fields, keeping every other key as it was.
fn restamp_metadata(dest: &Path, stamp: &Stamp) -> Result<()> {
    let path = dest.join(INSTALL_METADATA_PATH);
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("{INSTALL_METADATA_PATH} is not a JSON object"))?;
    obj.insert("loom_version".into(), Value::String(stamp.version.to_string()));
    if let Some(commit) = &stamp.commit {
        obj.insert("loom_commit".into(), Value::String(commit.clone()));
    }
    obj.insert("requires_daemon".into(), Value::String(stamp.requires_daemon.clone()));
    obj.insert(
        "last_resync".into(),
        Value::String(chrono::Utc::now().format("%Y-%m-%d").to_string()),
    );
    let mut out = serde_json::to_string_pretty(&value)?;
    out.push('\n');
    let tmp = path.with_extension("json.loom-resync.tmp");
    fs::write(&tmp, out)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}
