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
//!   claims.** Same two checks the other way round, against a release binary.
//!   That binary is the OLDEST PUBLISHED release at or above `REQUIRES_DAEMON`,
//!   not necessarily `v{REQUIRES_DAEMON}` itself: releases skip versions
//!   (`release-cadence.md`), and a fleet host can only run a published one.
//!   While no release at or above it is published (the PR that raises the
//!   claim, and `main` between that merge and the next release), the new
//!   daemon stands in. The files that OLD daemon executes are read from that
//!   release's copy of `install_compat.rs`; a release predating #10716
//!   declares none.
//!
//! A subcommand is probed with `<daemon> <sub> --help`, and the answer is one
//! of three ([`Probed`], #10868): missing when clap refuses it as
//! unrecognized, broken when the binary cannot be run, is killed by a signal
//! or panics, and present otherwise. Broken is a violation of its own: a
//! crash proves nothing about the subcommand, so it is neither counted as
//! present nor listed as a per-file gap. The
//! `(file, subcommand)` pairs come from `check-daemon-subcommand-versions.sh
//! --list`, the same detector the subcommand ratchet uses, so there is one
//! definition of "this script calls the daemon". A pair the file marks
//! `optional` is skipped: the script probes for it and degrades.
//!
//! A violated claim is a [`Report::violations`] entry, and the CLI exits 1.
//!
//! A sibling of [`crate::install_compat`], not a child of it, on purpose:
//! `init` uses the constants and the classifier there, and must not reach the
//! release lookup and download here (the `.gitignore Convergence Check`
//! input set in `merge_pr/stale_checks/inputs.rs` stops at `install_compat`).

use crate::install_compat::Version;
use crate::release_fetch::{checksum, fetch};
use crate::release_resolve::resolve::asset_names;
use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

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
    /// Where direction B's old daemon comes from.
    pub old_daemon: OldDaemon,
    /// The previous release, as a git ref. `None` resolves it from the tags.
    pub prev_ref: Option<String>,
    /// The new daemon's `SUPPORTS_INSTALLED` claim.
    pub supports_installed: Version,
    /// The new installed files' `REQUIRES_DAEMON` claim.
    pub requires_daemon: Version,
    /// The installed files the new daemon executes.
    pub invoked_files: Vec<String>,
}

/// Where direction B's old daemon comes from.
#[derive(Debug, Clone)]
pub enum OldDaemon {
    /// No binary. Passes only while no release tag at or above
    /// `requires_daemon` exists, so the new daemon stands in.
    Absent,
    /// `--old-daemon`: a binary the caller downloaded. It must report a
    /// version at or above `requires_daemon` with no release tag between the
    /// two, i.e. the oldest release that satisfies the claim.
    Given(PathBuf),
    /// `--fetch-old-daemon`: download the oldest release at or above
    /// `requires_daemon` that publishes `asset` and `<asset>.sha256` from
    /// `repo`, verifying the checksum. A tag whose release has no such asset
    /// yet (still uploading, #8515) is skipped. None published: the new daemon
    /// stands in.
    Fetch {
        /// `owner/repo` the releases live in.
        repo: String,
        /// The binary asset name, e.g. `loom-daemon-x86_64-unknown-linux-gnu`.
        asset: String,
    },
}

/// The most release asset listings one `--fetch-old-daemon` run asks the
/// forge for (one `gh release view` each). Normally the first candidate
/// answers; only a release still uploading costs a second. A run that
/// exhausts this fails rather than guessing.
pub const MAX_RELEASE_LOOKUPS: usize = 5;

/// How many times `--fetch-old-daemon` tries to download a release's binary
/// and checksum before failing (#10868). The asset listing already said both
/// are there, so a failure is a transfer blip or an upload still in flight.
pub const DOWNLOAD_ATTEMPTS: u32 = 3;

/// The pause between two download attempts.
const DOWNLOAD_RETRY_PAUSE: Duration = Duration::from_secs(5);

/// Which published release direction B runs against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OldRelease {
    /// The oldest release at or above the claim that carries the asset.
    Published(Version, String),
    /// No release at or above the claim carries it yet.
    Unpublished,
}

/// Release tags (`v*` names, one per line, as `git tag -l` prints them) at or
/// above `req`, oldest first. Non-version names are ignored.
#[must_use]
pub fn tags_at_or_above(tags: &str, req: Version) -> Vec<(Version, String)> {
    let mut out: Vec<(Version, String)> = tags
        .lines()
        .map(str::trim)
        .filter_map(|t| Version::parse(t).map(|v| (v, t.to_string())))
        .filter(|(v, _)| *v >= req)
        .collect();
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// The oldest of `candidates` (oldest first, from [`tags_at_or_above`]) whose
/// release carries the asset. `has_asset` answers one tag: `Some(true)`,
/// `Some(false)` (no such asset, or not yet), or `None` when the forge could
/// not say, which is an error rather than a skip so an outage never silently
/// demotes the check to the stand-in path.
///
/// This is the `release.resolve-and-fetch` forge operation
/// (`defaults/forge/operations/fleet-delivery.toml`), whose note says a
/// delayed publication must never become a required merge check. A delayed
/// one never does here: an unpublished or still-uploading release is skipped
/// and the new daemon stands in. A forge OUTAGE during the lookup turns the
/// step red by design, because this is a proof step and must not pass
/// without having proven anything.
///
/// The second value is every tag that was skipped on the way, oldest first,
/// so the report can name the release that is still uploading (#10868).
///
/// # Errors
/// An unanswerable lookup, or [`MAX_RELEASE_LOOKUPS`] candidates without the
/// asset.
pub fn pick_oldest_published(
    candidates: &[(Version, String)],
    mut has_asset: impl FnMut(&str) -> Option<bool>,
) -> Result<(OldRelease, Vec<String>)> {
    let mut skipped = Vec::new();
    for (i, (v, tag)) in candidates.iter().enumerate() {
        anyhow::ensure!(
            i < MAX_RELEASE_LOOKUPS,
            "the {MAX_RELEASE_LOOKUPS} oldest release tags at or above {} publish no daemon \
             asset; pass --old-daemon instead",
            candidates[0].0
        );
        match has_asset(tag) {
            Some(true) => return Ok((OldRelease::Published(*v, tag.clone()), skipped)),
            Some(false) => skipped.push(tag.clone()),
            None => anyhow::bail!("could not list the assets of release {tag}"),
        }
    }
    Ok((OldRelease::Unpublished, skipped))
}

/// How the report names the releases [`pick_oldest_published`] skipped.
/// `None` when it skipped none.
#[must_use]
pub fn skipped_releases_text(skipped: &[String]) -> Option<String> {
    match skipped {
        [] => None,
        [one] => Some(format!("release {one} is tagged but its assets are not uploaded yet")),
        many => Some(format!(
            "releases {} are tagged but their assets are not uploaded yet",
            many.join(", ")
        )),
    }
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
    direction_b(opts, &new_tree, &new_scripts, &mut probe, &mut report)?;
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
    report
        .violations
        .extend(floors_above_claim(tree, scripts, opts.requires_daemon));
}

/// One entry per hard `# requires-daemon: <sub> >= <version>` floor in
/// `scripts` (from [`installed_shell_files`] over `tree`) that is above
/// `requires_daemon`. Empty means the claim covers every shipped floor.
///
/// Shared by the CI harness and by the unit test in `install_compat/tests.rs`
/// (#10868), so both read the same files the same way.
#[must_use]
pub fn floors_above_claim(
    tree: &Path,
    scripts: &[PathBuf],
    requires_daemon: Version,
) -> Vec<String> {
    let mut over = Vec::new();
    for file in scripts {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for (sub, floor) in hard_floors(&text) {
            if floor > requires_daemon {
                over.push(format!(
                    "{} declares `requires-daemon: {sub} >= {floor}`, above REQUIRES_DAEMON {requires_daemon}",
                    display(file, tree),
                ));
            }
        }
    }
    over
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
    let gaps = probe.missing_subcommands(&opts.repo_root, &opts.new_daemon, &scripts)?;
    record_gaps(&label, &gaps, &prev_tree, report);
    record_absent_files(&label, &opts.invoked_files, &prev_tree, report);
    report.notes.push(gaps.summary(&label, scripts.len()));
    Ok(())
}

fn direction_b(
    opts: &CheckOptions,
    new_tree: &Path,
    scripts: &[PathBuf],
    probe: &mut Probe,
    report: &mut Report,
) -> Result<()> {
    let req = opts.requires_daemon;
    let candidates = tags_at_or_above(&release_tags(&opts.repo_root)?, req);
    let mut skipped = Vec::new();
    // Keeps a fetched binary alive until the probes below have run.
    let scratch = tempfile::Builder::new()
        .prefix("install-compat-old-")
        .tempdir()?;
    let old = match &opts.old_daemon {
        OldDaemon::Given(bin) => match given_daemon(bin, req, &candidates) {
            Ok(v) => Some((bin.clone(), v)),
            Err(why) => {
                report.violations.push(format!("B (daemon {req}): {why}"));
                return Ok(());
            }
        },
        OldDaemon::Fetch { repo, asset } => {
            let sha = format!("{asset}.sha256");
            let (picked, passed_over) = pick_oldest_published(&candidates, |tag| {
                asset_names(&opts.repo_root, repo, Some(tag))
                    .map(|names| names.contains(asset) && names.contains(&sha))
            })?;
            skipped = passed_over;
            match picked {
                OldRelease::Published(v, tag) => {
                    let bin = download_release_binary(
                        &opts.repo_root,
                        repo,
                        &tag,
                        asset,
                        scratch.path(),
                    )?;
                    if binary_version(&bin) != Some(v) {
                        report.violations.push(format!(
                            "B (daemon {req}): {asset} from release {tag} does not report version {v}"
                        ));
                        return Ok(());
                    }
                    Some((bin, v))
                }
                OldRelease::Unpublished => None,
            }
        }
        OldDaemon::Absent => {
            if let Some((_, tag)) = candidates.first() {
                report.violations.push(format!(
                    "B (daemon {req}): no binary given; pass --old-daemon <loom-daemon from \
                     release {tag}> or --fetch-old-daemon"
                ));
                return Ok(());
            }
            None
        }
    };
    let (label, daemon, invoked) = if let Some((bin, v)) = old {
        let label = if v == req {
            format!("B (new installed files vs daemon {req})")
        } else {
            format!(
                "B (new installed files vs daemon {v}, the oldest published release at or above {req})"
            )
        };
        if let Some(why) = skipped_releases_text(&skipped) {
            report.notes.push(format!("{label}: skipped, {why}"));
        }
        let invoked = old_invoked_files(&opts.repo_root, v, report)?;
        (label, bin, invoked)
    } else {
        let label = format!("B (new installed files vs daemon {req})");
        report.notes.push(stand_in_note(&label, req, &skipped));
        (label, opts.new_daemon.clone(), opts.invoked_files.clone())
    };
    let gaps = probe.missing_subcommands(&opts.repo_root, &daemon, scripts)?;
    record_gaps(&label, &gaps, new_tree, report);
    record_absent_files(&label, &invoked, new_tree, report);
    report.notes.push(gaps.summary(&label, scripts.len()));
    Ok(())
}

/// The note for direction B running against the new daemon because no
/// release at or above `req` carries the asset. It names the releases that
/// are tagged but still uploading, so a release in flight reads as that and
/// not as "nothing was ever released" (#10868).
#[must_use]
pub fn stand_in_note(label: &str, req: Version, skipped: &[String]) -> String {
    let why = skipped_releases_text(skipped).map_or_else(String::new, |t| format!(" ({t})"));
    format!(
        "{label}: no release at or above {req} is published yet{why}, so the new daemon stands in for it"
    )
}

/// Check a caller-supplied old daemon: it must report a version at or above
/// `req`, and no release tag may lie between the two (that one is older and
/// still satisfies the claim). Returns the version it reports.
fn given_daemon(
    bin: &Path,
    req: Version,
    candidates: &[(Version, String)],
) -> std::result::Result<Version, String> {
    let v = match binary_version(bin) {
        Some(v) if v >= req => v,
        other => {
            return Err(format!(
                "{} reports version {}, below the claimed {req}",
                bin.display(),
                other.map_or_else(|| "unknown".to_string(), |v| v.to_string())
            ))
        }
    };
    if let Some((_, tag)) = candidates.iter().find(|(c, _)| *c < v) {
        return Err(format!(
            "{} reports version {v}, but release {tag} is older and still at or above {req}; \
             pass that one",
            bin.display()
        ));
    }
    Ok(v)
}

/// Run `attempt` (one download of `asset` from release `tag`) until it
/// succeeds, at most [`DOWNLOAD_ATTEMPTS`] times, `pause` apart.
///
/// # Errors
/// Every attempt failed. The message names the tag and the asset.
pub fn download_with_retry(
    tag: &str,
    asset: &str,
    pause: Duration,
    mut attempt: impl FnMut() -> bool,
) -> Result<()> {
    for n in 1..=DOWNLOAD_ATTEMPTS {
        if attempt() {
            return Ok(());
        }
        if n < DOWNLOAD_ATTEMPTS {
            std::thread::sleep(pause);
        }
    }
    anyhow::bail!(
        "downloading {asset} from release {tag} failed {DOWNLOAD_ATTEMPTS} times; the release \
         lists the asset, so it may still be uploading: rerun, or pass --old-daemon"
    )
}

/// Download `asset` and `<asset>.sha256` from release `tag` into `dest`
/// (retried, [`download_with_retry`]), verify the checksum, and make the
/// binary executable. A checksum mismatch is not retried.
fn download_release_binary(
    repo_root: &Path,
    repo: &str,
    tag: &str,
    asset: &str,
    dest: &Path,
) -> Result<PathBuf> {
    let sha = format!("{asset}.sha256");
    download_with_retry(tag, asset, DOWNLOAD_RETRY_PAUSE, || {
        fetch::download(repo_root, repo, tag, &[asset, &sha], dest)
    })?;
    let bin = dest.join(asset);
    anyhow::ensure!(
        checksum::verify(&bin, &dest.join(&sha)),
        "{asset} from release {tag} does not match its .sha256"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(bin)
}

/// The files daemon `release` executes, from that release's copy of this module.
fn old_invoked_files(
    repo_root: &Path,
    release: Version,
    report: &mut Report,
) -> Result<Vec<String>> {
    let spec = format!("v{release}:{COMPAT_SOURCE_PATH}");
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
            "daemon {release} predates the invoked-file declaration; only subcommands are checked"
        ));
        Ok(Vec::new())
    }
}

fn record_gaps(label: &str, gaps: &Gaps, tree: &Path, report: &mut Report) {
    for (sub, files) in &gaps.missing {
        let users: Vec<String> = files.iter().map(|f| display(f, tree)).collect();
        report.violations.push(format!(
            "{label}: daemon has no `{sub}` subcommand, called by {}",
            users.join(", ")
        ));
    }
    for broken in &gaps.broken {
        report.violations.push(format!("{label}: {broken}"));
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

/// What `<daemon> <sub> --help` says about one subcommand (#10868).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probed {
    /// The daemon has it.
    Present,
    /// Clap refused it as unrecognized: a compatibility gap.
    Missing,
    /// The probe proved nothing, and this is how it failed. Never "present".
    Broken(String),
}

/// The exit code of a Rust process that panicked.
const PANIC_EXIT: i32 = 101;

/// Read one `<daemon> <sub> --help` outcome.
///
/// | Outcome | Result |
/// |---|---|
/// | stderr has clap's unrecognized-subcommand text | [`Probed::Missing`] |
/// | not spawned, killed by a signal, or exit 101 with `panicked at` on stderr | [`Probed::Broken`] |
/// | anything else | [`Probed::Present`] |
///
/// Exit 0 is not required for "present": a few subcommands (`gh-shim`) are
/// dispatched before clap and answer `--help` with their own usage and exit 2.
#[must_use]
pub fn classify_probe(outcome: &std::io::Result<Output>) -> Probed {
    let out = match outcome {
        Ok(out) => out,
        Err(e) => return Probed::Broken(format!("could not be executed ({e})")),
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    if subcommand_unrecognized(&stderr) {
        return Probed::Missing;
    }
    match out.status.code() {
        // No exit code: the process did not exit, a signal ended it.
        None => Probed::Broken(format!("was killed ({})", out.status)),
        Some(PANIC_EXIT) if stderr.contains("panicked at") => {
            Probed::Broken(format!("panicked ({})", out.status))
        }
        Some(_) => Probed::Present,
    }
}

/// What one set of scripts found wrong with one daemon.
#[derive(Debug, Default)]
struct Gaps {
    /// Subcommands the daemon lacks, each with the files that call it.
    missing: BTreeMap<String, Vec<PathBuf>>,
    /// Probes that proved nothing, as violation text. Each is listed once per
    /// run, under the first direction that hit it.
    broken: Vec<String>,
}

impl Gaps {
    fn summary(&self, label: &str, scripts: usize) -> String {
        let line =
            format!("{label}: {scripts} shell files, {} subcommand gaps", self.missing.len());
        if self.broken.is_empty() {
            line
        } else {
            format!("{line}, {} broken probes", self.broken.len())
        }
    }
}

/// `<daemon> <sub> --help` results, cached per binary and subcommand.
#[derive(Default)]
struct Probe {
    seen: HashMap<(PathBuf, String), Probed>,
    /// Binaries that could not be spawned at all, with why. Never run again:
    /// the answer is the same for every subcommand.
    unusable: HashMap<PathBuf, String>,
    /// Broken-probe violations already handed out this run.
    reported: BTreeSet<String>,
}

impl Probe {
    fn probe(&mut self, daemon: &Path, sub: &str) -> Probed {
        if let Some(why) = self.unusable.get(daemon) {
            return Probed::Broken(why.clone());
        }
        let key = (daemon.to_path_buf(), sub.to_string());
        if let Some(known) = self.seen.get(&key) {
            return known.clone();
        }
        let outcome = Command::new(daemon)
            .args([sub, "--help"])
            .stdin(Stdio::null())
            .output();
        let result = classify_probe(&outcome);
        match (&outcome, &result) {
            (Err(_), Probed::Broken(why)) => {
                self.unusable.insert(key.0, why.clone());
            }
            _ => {
                self.seen.insert(key, result.clone());
            }
        }
        result
    }

    /// Sort `pairs` (hard `(file, subcommand)` dependencies) by what `daemon`
    /// answers for each subcommand.
    fn gaps(&mut self, daemon: &Path, pairs: BTreeSet<(PathBuf, String)>) -> Gaps {
        let mut gaps = Gaps::default();
        for (file, sub) in pairs {
            match self.probe(daemon, &sub) {
                Probed::Present => {}
                Probed::Missing => gaps.missing.entry(sub).or_default().push(file),
                Probed::Broken(why) => {
                    let text = if self.unusable.contains_key(daemon) {
                        format!(
                            "daemon {} {why}, so no subcommand could be probed",
                            daemon.display()
                        )
                    } else {
                        format!(
                            "`{} {sub} --help` {why}; a crash does not show that `{sub}` exists",
                            daemon.display()
                        )
                    };
                    if self.reported.insert(text.clone()) {
                        gaps.broken.push(text);
                    }
                }
            }
        }
        gaps
    }

    /// What `daemon` lacks, or could not answer, of the hard subcommand
    /// dependencies of `scripts`.
    fn missing_subcommands(
        &mut self,
        repo_root: &Path,
        daemon: &Path,
        scripts: &[PathBuf],
    ) -> Result<Gaps> {
        Ok(self.gaps(daemon, hard_pairs(repo_root, scripts)?))
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
///
/// # Errors
/// A directory under `tree` could not be read.
pub fn installed_shell_files(tree: &Path) -> Result<Vec<PathBuf>> {
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

/// Every `v*` tag in the checkout, one per line.
fn release_tags(repo_root: &Path) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["tag", "-l", "v*"])
        .output()
        .context("running git tag")?;
    anyhow::ensure!(
        out.status.success(),
        "git tag -l failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The newest `v*` tag whose version is at or below `version`.
fn newest_tag_at_or_below(repo_root: &Path, version: Version) -> Result<String> {
    release_tags(repo_root)?
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

#[cfg(test)]
mod tests;
