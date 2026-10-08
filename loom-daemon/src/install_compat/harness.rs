//! CI proof of the compatibility contract across adjacent releases (#10716).
//!
//! `loom-daemon install-compat check` runs this. Two directions, plus a
//! static check of the claims themselves:
//!
//! * **Static.** Neither claim names a version above the one this change
//!   ships as, and `REQUIRES_DAEMON` is at least every hard
//!   `# requires-daemon: <sub> >= <version>` floor the shipped shell declares
//!   (`check-daemon-subcommand-versions.sh` owns that marker, #8285).
//! * **A: previous release's installed files against the new daemon.** The
//!   previous release is the newest `v*` tag at or below `VERSION`. When it is
//!   below `SUPPORTS_INSTALLED` the break is declared and A is skipped.
//!   Otherwise every `loom-daemon <sub>` its installed shell calls must exist
//!   in the new daemon, and every file the new daemon executes must exist in
//!   its installed tree.
//! * **B: the new installed files against the oldest daemon `REQUIRES_DAEMON`
//!   claims.** Same two checks the other way round, against that release's
//!   binary. The files that OLD daemon executes are read from that release's
//!   copy of `install_compat.rs`; a release predating #10716 declares none.
//!
//! "Exists" for a subcommand means `<daemon> <sub> --help` exits 0. The
//! `(file, subcommand)` pairs come from `check-daemon-subcommand-versions.sh
//! --list`, the same detector the subcommand ratchet uses, so there is one
//! definition of "this script calls the daemon". A pair the file marks
//! `optional` is skipped: the script probes for it and degrades.
//!
//! A violated claim is a [`Report::violations`] entry, and the CLI exits 1.

use super::Version;
use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where the release's own copy of this module lives, for reading an old
/// daemon's invoked-file list out of git.
const COMPAT_SOURCE_PATH: &str = "loom-daemon/src/install_compat.rs";

/// The subcommand detector, relative to the repo root.
const DETECTOR_PATH: &str = "scripts/check-daemon-subcommand-versions.sh";

/// What one run checks.
#[derive(Debug, Clone)]
pub struct CheckOptions {
    /// The checkout whose `defaults/` are the new installed files and whose
    /// tags name the releases.
    pub repo_root: PathBuf,
    /// The new daemon (built from `repo_root`).
    pub new_daemon: PathBuf,
    /// A binary of release `requires_daemon`. `None` is allowed only when the
    /// claim names the unreleased version this change ships as.
    pub old_daemon: Option<PathBuf>,
    /// The previous release, as a git ref. `None` resolves it from the tags.
    pub prev_ref: Option<String>,
    /// The new daemon's `SUPPORTS_INSTALLED` claim.
    pub supports_installed: Version,
    /// The new installed files' `REQUIRES_DAEMON` claim.
    pub requires_daemon: Version,
    /// The installed files the new daemon executes.
    pub invoked_files: Vec<String>,
}

/// The outcome of one run.
#[derive(Debug, Default)]
pub struct Report {
    /// What was checked or skipped, in order.
    pub notes: Vec<String>,
    /// Each violated claim. Non-empty means the contract does not hold.
    pub violations: Vec<String>,
}

/// Run every check.
///
/// # Errors
/// Only for an environment that cannot be checked at all (no `VERSION`, no
/// git, no tag at or below `VERSION`). A claim that does not hold is a
/// [`Report::violations`] entry, never an `Err`.
pub fn run(opts: &CheckOptions) -> Result<Report> {
    let mut report = Report::default();
    let version_text =
        std::fs::read_to_string(opts.repo_root.join("VERSION")).context("reading VERSION")?;
    let version =
        Version::parse(&version_text).ok_or_else(|| anyhow!("VERSION is not MAJOR.MINOR.PATCH"))?;
    let ships_as = version.next_patch();
    report.notes.push(format!(
        "claims: supports_installed={} requires_daemon={} (VERSION {version}; this change ships as {ships_as})",
        opts.supports_installed, opts.requires_daemon
    ));

    let new_tree = opts.repo_root.join("defaults");
    let new_scripts = installed_shell_files(&new_tree)?;
    check_claims(opts, ships_as, &new_tree, &new_scripts, &mut report);

    let mut probe = Probe::default();
    direction_a(opts, version, &mut probe, &mut report)?;
    direction_b(opts, version, &new_tree, &new_scripts, &mut probe, &mut report)?;
    Ok(report)
}

fn check_claims(
    opts: &CheckOptions,
    ships_as: Version,
    tree: &Path,
    scripts: &[PathBuf],
    report: &mut Report,
) {
    for (name, claim) in [
        ("SUPPORTS_INSTALLED", opts.supports_installed),
        ("REQUIRES_DAEMON", opts.requires_daemon),
    ] {
        if claim > ships_as {
            report.violations.push(format!(
                "{name} {claim} is above {ships_as}, the version this change ships as"
            ));
        }
    }
    for file in scripts {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for (sub, floor) in hard_floors(&text) {
            if floor > opts.requires_daemon {
                report.violations.push(format!(
                    "{} declares `requires-daemon: {sub} >= {floor}`, above REQUIRES_DAEMON {}",
                    display(file, tree),
                    opts.requires_daemon
                ));
            }
        }
    }
}

fn direction_a(
    opts: &CheckOptions,
    version: Version,
    probe: &mut Probe,
    report: &mut Report,
) -> Result<()> {
    let prev_ref = match &opts.prev_ref {
        Some(r) => r.clone(),
        None => newest_tag_at_or_below(&opts.repo_root, version)?,
    };
    let prev = Version::parse(&prev_ref)
        .ok_or_else(|| anyhow!("previous release ref {prev_ref} is not a version tag"))?;
    if prev < opts.supports_installed {
        report.notes.push(format!(
            "A: skipped, declared break: previous release {prev} is below SUPPORTS_INSTALLED {}",
            opts.supports_installed
        ));
        return Ok(());
    }
    let scratch = tempfile::Builder::new()
        .prefix("install-compat-prev-")
        .tempdir()?;
    extract_defaults(&opts.repo_root, &prev_ref, scratch.path())?;
    let prev_tree = scratch.path().join("defaults");
    let scripts = installed_shell_files(&prev_tree)?;
    let label = format!("A ({prev_ref} installed files vs new daemon)");
    let missing = probe.missing_subcommands(&opts.repo_root, &opts.new_daemon, &scripts)?;
    record_missing(&label, &missing, &prev_tree, report);
    record_absent_files(&label, &opts.invoked_files, &prev_tree, report);
    report.notes.push(format!(
        "{label}: {} shell files, {} subcommand gaps",
        scripts.len(),
        missing.len()
    ));
    Ok(())
}

fn direction_b(
    opts: &CheckOptions,
    version: Version,
    new_tree: &Path,
    scripts: &[PathBuf],
    probe: &mut Probe,
    report: &mut Report,
) -> Result<()> {
    let req = opts.requires_daemon;
    let label = format!("B (new installed files vs daemon {req})");
    let (daemon, invoked) = if let Some(old) = &opts.old_daemon {
        match binary_version(old) {
            Some(v) if v == req => {}
            other => {
                report.violations.push(format!(
                    "{label}: {} reports version {}, not the claimed {req}",
                    old.display(),
                    other.map_or_else(|| "unknown".to_string(), |v| v.to_string())
                ));
                return Ok(());
            }
        }
        (old.clone(), old_invoked_files(&opts.repo_root, req, report)?)
    } else if req > version {
        report.notes.push(format!(
            "{label}: {req} is unreleased (this change), so the new daemon stands in for it"
        ));
        (opts.new_daemon.clone(), opts.invoked_files.clone())
    } else {
        report.violations.push(format!(
            "{label}: no binary given; pass --old-daemon <loom-daemon from release v{req}>"
        ));
        return Ok(());
    };
    let missing = probe.missing_subcommands(&opts.repo_root, &daemon, scripts)?;
    record_missing(&label, &missing, new_tree, report);
    record_absent_files(&label, &invoked, new_tree, report);
    report.notes.push(format!(
        "{label}: {} shell files, {} subcommand gaps",
        scripts.len(),
        missing.len()
    ));
    Ok(())
}

/// The files daemon `req` executes, from that release's copy of this module.
fn old_invoked_files(repo_root: &Path, req: Version, report: &mut Report) -> Result<Vec<String>> {
    let spec = format!("v{req}:{COMPAT_SOURCE_PATH}");
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["show", &spec])
        .output()?;
    let parsed = out
        .status
        .success()
        .then(|| invoked_files_from_source(&String::from_utf8_lossy(&out.stdout)))
        .flatten();
    if let Some(files) = parsed {
        Ok(files)
    } else {
        report.notes.push(format!(
            "daemon {req} predates the invoked-file declaration; only subcommands are checked"
        ));
        Ok(Vec::new())
    }
}

fn record_missing(
    label: &str,
    missing: &BTreeMap<String, Vec<PathBuf>>,
    tree: &Path,
    report: &mut Report,
) {
    for (sub, files) in missing {
        let users: Vec<String> = files.iter().map(|f| display(f, tree)).collect();
        report.violations.push(format!(
            "{label}: daemon has no `{sub}` subcommand, called by {}",
            users.join(", ")
        ));
    }
}

fn record_absent_files(label: &str, invoked: &[String], tree: &Path, report: &mut Report) {
    for file in invoked {
        let Some(rel) = file.strip_prefix(".loom/") else {
            continue;
        };
        if !tree.join(rel).is_file() {
            report.violations.push(format!(
                "{label}: the daemon executes {file}, which the installed tree lacks"
            ));
        }
    }
}

/// `<daemon> <sub> --help` results, cached per binary and subcommand.
#[derive(Default)]
struct Probe {
    seen: HashMap<(PathBuf, String), bool>,
}

impl Probe {
    /// A subcommand exists unless clap refuses it as unrecognized. Exit 0 is
    /// not required: a few subcommands (`gh-shim`) are dispatched before clap
    /// and answer `--help` with their own usage and exit 2.
    fn has(&mut self, daemon: &Path, sub: &str) -> bool {
        *self
            .seen
            .entry((daemon.to_path_buf(), sub.to_string()))
            .or_insert_with(|| {
                let Ok(out) = Command::new(daemon)
                    .args([sub, "--help"])
                    .stdin(Stdio::null())
                    .output()
                else {
                    return false;
                };
                out.status.success()
                    || !subcommand_unrecognized(&String::from_utf8_lossy(&out.stderr))
            })
    }

    /// Hard subcommand dependencies of `scripts` that `daemon` lacks, each
    /// with the files that call it.
    fn missing_subcommands(
        &mut self,
        repo_root: &Path,
        daemon: &Path,
        scripts: &[PathBuf],
    ) -> Result<BTreeMap<String, Vec<PathBuf>>> {
        let mut missing: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for (file, sub) in hard_pairs(repo_root, scripts)? {
            if !self.has(daemon, &sub) {
                missing.entry(sub).or_default().push(file);
            }
        }
        Ok(missing)
    }
}

/// Whether a daemon's stderr is clap refusing the subcommand itself.
#[must_use]
pub fn subcommand_unrecognized(stderr: &str) -> bool {
    stderr.contains("unrecognized subcommand")
}

/// Every `(file, subcommand)` the detector finds in `scripts`, minus the
/// subcommands a file marks `optional`.
fn hard_pairs(repo_root: &Path, scripts: &[PathBuf]) -> Result<BTreeSet<(PathBuf, String)>> {
    if scripts.is_empty() {
        return Ok(BTreeSet::new());
    }
    let out = Command::new("bash")
        .arg(repo_root.join(DETECTOR_PATH))
        .arg("--list")
        .args(scripts)
        .current_dir(repo_root)
        .output()
        .context("running the subcommand detector")?;
    anyhow::ensure!(
        out.status.success(),
        "{DETECTOR_PATH} --list failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let mut optional: HashMap<PathBuf, BTreeSet<String>> = HashMap::new();
    let mut pairs = BTreeSet::new();
    for (file, sub) in parse_detector_list(&String::from_utf8_lossy(&out.stdout)) {
        let skip = optional.entry(file.clone()).or_insert_with(|| {
            std::fs::read_to_string(&file)
                .map(|t| optional_subs(&t))
                .unwrap_or_default()
        });
        if !skip.contains(&sub) {
            pairs.insert((file, sub));
        }
    }
    Ok(pairs)
}

/// Parse the detector's `--list` output: `<file>:<line>\t<subcommand>`.
#[must_use]
pub fn parse_detector_list(out: &str) -> Vec<(PathBuf, String)> {
    out.lines()
        .filter_map(|line| {
            let (loc, sub) = line.rsplit_once('\t')?;
            let (file, _line) = loc.rsplit_once(':')?;
            Some((PathBuf::from(file), sub.trim().to_string()))
        })
        .collect()
}

fn marker_bodies(text: &str) -> impl Iterator<Item = Vec<&str>> {
    text.lines().filter_map(|line| {
        let body = line
            .trim_start()
            .strip_prefix('#')?
            .trim_start()
            .strip_prefix("requires-daemon:")?;
        Some(body.split_whitespace().collect())
    })
}

/// Hard floors declared in a script: `# requires-daemon: <sub> >= <version>`.
#[must_use]
pub fn hard_floors(text: &str) -> Vec<(String, Version)> {
    marker_bodies(text)
        .filter_map(|w| match w.as_slice() {
            [sub, ">=", v, ..] => Some(((*sub).to_string(), Version::parse(v)?)),
            _ => None,
        })
        .collect()
}

/// Subcommands a script marks `# requires-daemon: <sub> optional`.
#[must_use]
pub fn optional_subs(text: &str) -> BTreeSet<String> {
    marker_bodies(text)
        .filter_map(|w| match w.as_slice() {
            [sub, "optional", ..] => Some((*sub).to_string()),
            _ => None,
        })
        .collect()
}

/// The invoked-file list from a copy of `install_compat.rs`: every quoted
/// string between the `compat:invoked-files` markers. `None` when the markers
/// are absent (a release predating #10716).
#[must_use]
pub fn invoked_files_from_source(src: &str) -> Option<Vec<String>> {
    let start = src.find("// compat:invoked-files:begin")?;
    let end = start + src[start..].find("// compat:invoked-files:end")?;
    let files = src[start..end]
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with("//") {
                return None;
            }
            let rest = l.strip_prefix('"')?;
            Some(rest[..rest.find('"')?].to_string())
        })
        .collect();
    Some(files)
}

/// Installed shell under `tree` (a `defaults/` directory), test suites
/// excluded, sorted. Symlinks are not followed.
fn installed_shell_files(tree: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![tree.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() {
                if entry.file_name() != "tests" {
                    stack.push(path);
                }
            } else if kind.is_file() && path.extension().is_some_and(|e| e == "sh") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Unpack `<git_ref>:defaults` into `dest/defaults`.
fn extract_defaults(repo_root: &Path, git_ref: &str, dest: &Path) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["archive", "--format=tar", git_ref, "defaults"])
        .output()
        .context("running git archive")?;
    anyhow::ensure!(
        out.status.success(),
        "git archive {git_ref} defaults failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    tar::Archive::new(out.stdout.as_slice())
        .unpack(dest)
        .with_context(|| format!("unpacking {git_ref}:defaults"))
}

/// The newest `v*` tag whose version is at or below `version`.
fn newest_tag_at_or_below(repo_root: &Path, version: Version) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["tag", "-l", "v*"])
        .output()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|t| Version::parse(t).map(|v| (v, t.to_string())))
        .filter(|(v, _)| *v <= version)
        .max()
        .map(|(_, t)| t)
        .ok_or_else(|| anyhow!("no v* release tag at or below {version} (a shallow clone has none: fetch with tags)"))
}

/// The version a daemon binary reports: `loom-daemon <version>…` on stdout.
fn binary_version(bin: &Path) -> Option<Version> {
    let out = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .find_map(Version::parse)
}

fn display(file: &Path, tree: &Path) -> String {
    file.strip_prefix(tree.parent().unwrap_or(tree))
        .unwrap_or(file)
        .display()
        .to_string()
}
