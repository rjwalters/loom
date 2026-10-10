//! The resync surfaces outside the installer's payload step (#10895), and the
//! one table that names them.
//!
//! [`super::install_into`] covers what `init`'s payload step writes. The shell
//! resync (`resync-installed.sh`) refreshes more than that, and a daemon
//! resync that stopped at the payload step left those files at an older
//! release under a stamp that said otherwise. Each one is decided here:
//!
//! * [`EXTRA_SURFACES`]: refreshed by the daemon resync, under the [`Rule`]
//!   beside it.
//! * the retired-payload sweep ([`sweep_retired`]): removes what
//!   `defaults/.loom-retired.list` names.
//! * [`INSTALL_TIME_ONLY`]: deliberately not refreshed, with the reason.
//!
//! Every step runs over the STAGING tree, after the installer step, and the
//! result is diffed like any other file. So the rules of the parent module
//! hold unchanged: an empty diff writes nothing, a pinned path is never
//! written or removed, and a symlink is never followed. Each step is a pure
//! function of the payload and the workspace's files (no clock, no
//! environment, no git), so the same inputs always give the same diff and a
//! second resync at the same release is empty.
//!
//! The steps only ever REMOVE two kinds of file: a `SKILL.md` that carries
//! Loom's own generation marker and that this payload no longer generates,
//! and a path the release lists as retired. Nothing else is deleted.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use super::super::scaffolding::install_agent_skills_where;
use super::super::templates::{render_dotloom_guide, LoomMetadata};
use super::super::{update_gitignore, InitReport};
use crate::agent_skills;

/// How the daemon resync refreshes one surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rule {
    /// `.agents/skills/`: each generated `loom-<name>/SKILL.md` is written
    /// when the destination is absent or carries
    /// [`agent_skills::MARKER`]. A file without the marker is the
    /// consumer's and is never written or removed. Backfilled when absent.
    MarkerGatedSkills,
    /// Verbatim copy, only when the repo already has the file: one that
    /// never received it (or removed it) is not given it back.
    CopyIfPresent,
    /// Verbatim copy, created when absent.
    CopyOrBackfill,
    /// The Loom-managed block of the repo's `.gitignore`, merged by
    /// [`update_gitignore`]. Only when the file exists. The file is the
    /// repo's, so it is never recorded as Loom-owned.
    GitignoreBlock,
    /// A template-substituted guide, re-rendered with the install date the
    /// existing file already carries. Only when the file exists.
    DatedGuide,
}

/// One surface the daemon resync refreshes beyond the installer's payload
/// step: a repo-relative path (a file, or a directory for the skills) and its
/// rule.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Surface {
    /// Repo-relative path.
    pub(crate) path: &'static str,
    /// How it is refreshed.
    pub(crate) rule: Rule,
}

/// The cross-vendor skills directory.
const SKILLS_DIR: &str = ".agents/skills";
/// The repo's ignore file, whose Loom-managed block is merged.
const GITIGNORE: &str = ".gitignore";

/// Every surface the daemon resync covers beyond the installer's payload
/// step. The staging seed, the diff and the default-branch export
/// ([`export_pathspecs`]) all read this table; the parity test checks it
/// against `resync-installed.sh`.
pub(crate) const EXTRA_SURFACES: &[Surface] = &[
    Surface {
        path: SKILLS_DIR,
        rule: Rule::MarkerGatedSkills,
    },
    Surface {
        path: ".claude/README.md",
        rule: Rule::CopyIfPresent,
    },
    Surface {
        path: ".github/CONFIGURATION.md",
        rule: Rule::CopyIfPresent,
    },
    Surface {
        path: ".claude/biome.jsonc",
        rule: Rule::CopyOrBackfill,
    },
    Surface {
        path: GITIGNORE,
        rule: Rule::GitignoreBlock,
    },
    Surface {
        path: ".loom/CLAUDE.md",
        rule: Rule::DatedGuide,
    },
    Surface {
        path: ".loom/AGENTS.md",
        rule: Rule::DatedGuide,
    },
];

/// What `resync-installed.sh` or the installer does that NO daemon resync
/// does, and why. `daemon-reference.md` carries the same list for operators;
/// the parity tests read this one, and `resync-payload` names its entries in
/// the note it prints ([`super::standalone`], #8961).
pub(crate) const INSTALL_TIME_ONLY: &[(&str, &str)] = &[
    (
        ".loom/config.json",
        "consumer configuration: the installer merges it, no resync migrates a key, so a release \
         that renames or retires a config key must keep reading the old one",
    ),
    (
        ".loom/config/",
        "consumer-editable configuration (skill-routes.json): the installer copies each file \
         only when it is absent (#9129), so a resync never overwrites an edit or re-creates a \
         deleted file, which is the documented way to switch the skill router off",
    ),
    (
        "package.json",
        "the loom-workspace stub's `version` removal is a one-time migration (#4285)",
    ),
    (
        "CLAUDE.md",
        "the root guide is repo-customized; its leftover `**Loom Version**` header removal is a \
         one-time migration (#6612, #8147)",
    ),
    (
        ".gitattributes",
        "the merge=ours driver exists for per-host resync commits that conflict on the stamp \
         (#4528); the daemon resync has one writer per change, and the local git config half \
         cannot be committed",
    ),
    (
        "forge labels",
        "the drift check against .github/labels.yml is a forge read and a report, not an \
         installed file; it stays with sync-labels.sh",
    ),
];

/// The two trees the installer's payload step and the slash commands live in.
const PAYLOAD_TREES: &[&str] = &[".loom", ".claude/commands/loom"];

/// Every path a resync reads from a workspace, as `git archive` pathspecs:
/// the payload trees plus each extra surface outside them. The default-branch
/// export (`fleet_sync::workspace_resync`) takes its list from here, so a
/// surface added to the table is diffed against the default branch too.
#[must_use]
pub(crate) fn export_pathspecs() -> Vec<&'static str> {
    let mut specs = PAYLOAD_TREES.to_vec();
    specs.extend(
        EXTRA_SURFACES
            .iter()
            .map(|s| s.path)
            .filter(|p| !PAYLOAD_TREES.iter().any(|tree| within(p, tree))),
    );
    specs
}

/// `path` is `root` or lies under it.
fn within(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Is `path` one of the extra surfaces (as opposed to the installer's)?
pub(super) fn is_extra(path: &str) -> bool {
    EXTRA_SURFACES.iter().any(|s| within(path, s.path))
}

/// May a resync record `path` in `installed_files`? That list is deletion
/// evidence (a later release, or an uninstall, may remove what it names), so
/// a file the repo owns and Loom only merges a block into is never listed.
pub(super) fn records_ownership(path: &str) -> bool {
    !EXTRA_SURFACES
        .iter()
        .any(|s| s.rule == Rule::GitignoreBlock && s.path == path)
}

/// The names `path` can be pinned under in `.loom/resync-ignore`: itself, and
/// the report-relative form `resync-installed.sh` prints and documents for
/// the two surfaces whose form is not a plain `.loom/` strip
/// (`commands/loom/x.md`, `agents-skills/loom-x/SKILL.md`). A pin written for
/// the script must hold for the daemon too.
pub(super) fn pin_names(path: &str) -> Vec<String> {
    let mut names = vec![path.to_string()];
    for (prefix, report) in [
        (".claude/commands/loom/", "commands/loom/"),
        (".agents/skills/", "agents-skills/"),
    ] {
        if let Some(rest) = path.strip_prefix(prefix) {
            names.push(format!("{report}{rest}"));
        }
    }
    names
}

/// One surface a resync left exactly as the workspace has it, because the
/// step could not be done there. The rest of the payload still resyncs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Skipped {
    /// Repo-relative path.
    pub(crate) path: String,
    /// Why it was left alone.
    pub(crate) why: SkipReason,
}

/// Why a surface was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// The workspace's own file cannot be merged into or replaced: it is not
    /// a regular file (a directory sits at the path), or it is not UTF-8.
    Unusable(String),
    /// A guide with no `**Installation Date**:` line cannot be re-rendered
    /// exactly, so it stays at whatever template it was rendered from.
    NoInstallDate,
}

/// Run every extra step over `stage`, which already holds the workspace's
/// copy of each surface and the installer step's output. Paths a step wrote
/// are added to `shipped` (the files this payload ships).
///
/// A surface whose own file in the workspace is unusable (see
/// [`SkipReason`]) is added to `skipped` and left byte for byte as it is: it
/// is never written and never removed, and every other surface still
/// resyncs. One odd file must not hold the whole repo at an older release.
///
/// # Errors
/// A step could not read the payload or write staging. Nothing outside
/// staging is touched either way.
pub(super) fn stage_extras(
    defaults: &Path,
    stage: &Path,
    shipped: &mut BTreeSet<String>,
    skipped: &mut Vec<Skipped>,
) -> Result<(), String> {
    for surface in EXTRA_SURFACES {
        let dst = stage.join(surface.path);
        if surface.rule == Rule::MarkerGatedSkills {
            stage_skills(defaults, stage, shipped, skipped)?;
            continue;
        }
        if let Some(why) = not_a_file(&dst) {
            skipped.push(Skipped::unusable(surface.path, why));
            continue;
        }
        let present = is_regular_file(&dst);
        let wrote = match surface.rule {
            Rule::MarkerGatedSkills => false,
            Rule::CopyIfPresent => present && copy_file(&defaults.join(surface.path), &dst)?,
            Rule::CopyOrBackfill => copy_file(&defaults.join(surface.path), &dst)?,
            Rule::GitignoreBlock => {
                // `update_gitignore` fails on a file that is not UTF-8. Such
                // a file is the repo's to keep, not a reason to stop.
                match fs::read_to_string(&dst) {
                    Ok(_) if present => {
                        update_gitignore(stage)?;
                        true
                    }
                    Err(e) if present => {
                        skipped.push(Skipped::unusable(surface.path, &e.to_string()));
                        false
                    }
                    _ => false,
                }
            }
            Rule::DatedGuide => {
                present && stage_guide(&defaults.join(surface.path), &dst, surface.path, skipped)?
            }
        };
        if wrote {
            shipped.insert(surface.path.to_string());
        }
    }
    sweep_retired(defaults, stage, shipped)
}

impl Skipped {
    fn unusable(path: &str, why: &str) -> Self {
        Self {
            path: path.to_string(),
            why: SkipReason::Unusable(why.to_string()),
        }
    }
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// Something other than a regular file sits at `path` (staging never holds a
/// symlink, so in practice a directory). `None` when it is a regular file or
/// absent.
fn not_a_file(path: &Path) -> Option<&'static str> {
    fs::symlink_metadata(path)
        .is_ok_and(|m| !m.file_type().is_file())
        .then_some("not a regular file")
}

/// The `(repo, path)` pairs already reported by [`log_skipped_once`].
fn reported() -> &'static Mutex<BTreeSet<(PathBuf, String)>> {
    static REPORTED: OnceLock<Mutex<BTreeSet<(PathBuf, String)>>> = OnceLock::new();
    REPORTED.get_or_init(Mutex::default)
}

/// Is this the first report of `path` in `repo` since the daemon started?
fn first_report(repo: &Path, path: &str) -> bool {
    reported()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert((repo.to_path_buf(), path.to_string()))
}

/// Log each surface a resync of `repo` skipped, once per repo and path per
/// daemon lifetime: the condition is the repo's to fix and does not change
/// from one tick to the next, so repeating it would only bury it. An
/// unusable file is a WARN; a guide that can never be refreshed is INFO, so a
/// permanently stale one is visible without being an alarm.
///
/// The caller names the repo: staging runs over an export or a throwaway
/// worktree, whose path says nothing to an operator.
pub(crate) fn log_skipped_once(repo: &Path, skipped: &[Skipped]) {
    for skip in skipped {
        if !first_report(repo, &skip.path) {
            continue;
        }
        match &skip.why {
            SkipReason::Unusable(why) => log::warn!(
                "resync: {}: {} is skipped ({why}); it is left as is and the rest of the \
                 payload still resyncs",
                repo.display(),
                skip.path
            ),
            SkipReason::NoInstallDate => log::info!(
                "resync: {}: {} records no install date, so it is not re-rendered and stays \
                 at its current template; reinstall to refresh it",
                repo.display(),
                skip.path
            ),
        }
    }
}

/// Copy `src` over `dst`. `false` when the payload does not ship `src`.
fn copy_file(src: &Path, dst: &Path) -> Result<bool, String> {
    if !src.is_file() {
        return Ok(false);
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    fs::copy(src, dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
    Ok(true)
}

/// The skills step: the installer's own marker-gated write, then the removal
/// of marker-carrying skills this payload no longer generates.
fn stage_skills(
    defaults: &Path,
    stage: &Path,
    shipped: &mut BTreeSet<String>,
    skipped: &mut Vec<Skipped>,
) -> Result<(), String> {
    // A payload whose skills cannot be generated says nothing about which
    // skills exist, so nothing is written and (above all) nothing is removed.
    let Ok(skills) = agent_skills::generate_all(defaults) else {
        log::debug!("resync: the payload generates no agent skills; {SKILLS_DIR} is left as is");
        return Ok(());
    };
    let mut report = InitReport::default();
    // A directory at a `SKILL.md` path, or a file where the skill's own
    // directory should be, is the workspace's: that one skill is skipped.
    let mut usable = |dst: &Path, rel: &str| {
        let odd = not_a_file(dst).or_else(|| {
            dst.ancestors()
                .skip(1)
                .take_while(|dir| *dir != stage && dir.starts_with(stage))
                .any(|dir| fs::symlink_metadata(dir).is_ok_and(|m| !m.is_dir()))
                .then_some("a file sits where its directory should be")
        });
        if let Some(why) = odd {
            skipped.push(Skipped::unusable(rel, why));
        }
        odd.is_none()
    };
    install_agent_skills_where(defaults, stage, &mut report, &mut usable)?;
    shipped.extend(report.added.into_iter().chain(report.updated));

    let generated: BTreeSet<String> = skills.iter().map(|s| format!("loom-{}", s.name)).collect();
    let Ok(entries) = fs::read_dir(stage.join(SKILLS_DIR)) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("loom-") || generated.contains(&name) {
            continue;
        }
        let skill = entry.path().join("SKILL.md");
        if is_regular_file(&skill) && carries_skill_marker(&skill) {
            fs::remove_file(&skill).map_err(|e| format!("remove {}: {e}", skill.display()))?;
        }
    }
    Ok(())
}

/// Loom generated this `SKILL.md` (and nobody detached it since).
fn carries_skill_marker(path: &Path) -> bool {
    fs::read(path).is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(agent_skills::MARKER))
}

/// The line of a rendered guide that carries its install date.
const INSTALL_DATE_LINE: &str = "**Installation Date**:";
/// The one placeholder a guide template may hold: its value is read back from
/// the installed file, so re-rendering is exact. Any other placeholder would
/// be filled from this host or this moment and could never be an empty diff.
const INSTALL_DATE_PLACEHOLDER: &str = "{{INSTALL_DATE}}";

/// The install date an installed guide records; `None` when it has no such
/// line or the line holds no usable value.
pub(super) fn recorded_install_date(guide: &str) -> Option<&str> {
    guide
        .lines()
        .find_map(|line| line.trim().strip_prefix(INSTALL_DATE_LINE))
        .map(str::trim)
        .filter(|date| !date.is_empty() && !date.contains("{{"))
}

/// The `{{UPPER_CASE}}` placeholders of `template` that a resync cannot
/// reproduce: every one but the install date.
pub(super) fn unreproducible_placeholders(template: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        rest = &rest[open + 2..];
        let len = rest
            .bytes()
            .take_while(|b| b.is_ascii_uppercase() || *b == b'_')
            .count();
        if len > 0 && rest[len..].starts_with("}}") {
            let placeholder = format!("{{{{{}}}}}", &rest[..len]);
            if placeholder != INSTALL_DATE_PLACEHOLDER && !found.contains(&placeholder) {
                found.push(placeholder);
            }
        }
    }
    found
}

/// Re-render the guide at `dst` from `template_path`, keeping the install
/// date `dst` already records. `false` (and `dst` untouched) when that cannot
/// be done exactly: no template, no recorded date, or a template that needs
/// a value only the installer has. A guide left alone for a reason in the
/// workspace's own file is added to `skipped` under `rel`.
fn stage_guide(
    template_path: &Path,
    dst: &Path,
    rel: &str,
    skipped: &mut Vec<Skipped>,
) -> Result<bool, String> {
    let Ok(template) = fs::read_to_string(template_path) else {
        return Ok(false);
    };
    let existing = match fs::read_to_string(dst) {
        Ok(existing) => existing,
        Err(e) => {
            skipped.push(Skipped::unusable(rel, &e.to_string()));
            return Ok(false);
        }
    };
    let blocked = unreproducible_placeholders(&template);
    if !blocked.is_empty() {
        log::warn!(
            "resync: {} needs {blocked:?}, which only the installer can fill; left as is",
            template_path.display()
        );
        return Ok(false);
    }
    let Some(date) = recorded_install_date(&existing) else {
        skipped.push(Skipped {
            path: rel.to_string(),
            why: SkipReason::NoInstallDate,
        });
        return Ok(false);
    };
    let metadata = LoomMetadata {
        install_date: date.to_string(),
        ..LoomMetadata::default()
    };
    let rendered = render_dotloom_guide(&template, None, None, &metadata);
    fs::write(dst, rendered).map_err(|e| format!("write {}: {e}", dst.display()))?;
    Ok(true)
}

/// The list of paths a release retired, inside the payload.
const RETIRED_LIST: &str = ".loom-retired.list";

/// Where a retired-list entry lives in an installed repo. The same mapping
/// as `retired_target_path` in `resync-installed.sh`; an entry of any other
/// shape maps to nothing and is skipped, never guessed at.
pub(super) fn retired_target(entry: &str) -> Option<String> {
    const MOVED: &[(&str, &str)] = &[
        ("hooks/", ".loom/hooks/"),
        ("scripts/", ".loom/scripts/"),
        ("roles/", ".loom/roles/"),
        ("docs/", ".loom/docs/"),
        ("runtimes/", ".loom/runtimes/"),
        ("bin/", ".loom/bin/"),
        ("commands/loom/", ".claude/commands/loom/"),
    ];
    if entry
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    if entry == ".claude/README.md" || entry == ".github/CONFIGURATION.md" {
        return Some(entry.to_string());
    }
    MOVED
        .iter()
        .find_map(|(from, to)| entry.strip_prefix(from).map(|rest| format!("{to}{rest}")))
}

/// The entries of a retired list: one per line, `#` comments and blank lines
/// dropped (the script's parse).
pub(super) fn retired_entries(list: &str) -> impl Iterator<Item = &str> {
    list.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
}

/// The retired-payload sweep: remove from staging every path the release
/// lists as retired, whether or not `installed_files` ever recorded it. The
/// diff then reports it removed, unless it is pinned or a symlink (neither is
/// ever in the diff). A path this payload ships is not removed: a list that
/// still names an un-retired file must not delete it on every resync.
fn sweep_retired(defaults: &Path, stage: &Path, shipped: &BTreeSet<String>) -> Result<(), String> {
    let list = match fs::read_to_string(defaults.join(RETIRED_LIST)) {
        Ok(list) => list,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read {RETIRED_LIST}: {e}")),
    };
    for target in retired_entries(&list).filter_map(retired_target) {
        if shipped.contains(&target) {
            continue;
        }
        let path = stage.join(&target);
        if is_regular_file(&path) {
            fs::remove_file(&path).map_err(|e| format!("remove {target}: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "surfaces_tests.rs"]
mod tests;
