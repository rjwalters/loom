//! Fork-point provenance for `.loom/resync-ignore` pins (issue #8726).
//!
//! A pin in `.loom/resync-ignore` records **that** a file is protected from
//! `resync-installed.sh`, never **what upstream revision it forked from**, so
//! "can this pin be lifted yet?" used to mean replaying every upstream revision
//! of the file and `cmp`-ing each against the local copy. This module adds the
//! missing half as an **additive sidecar**, `.loom/resync-pin-base`:
//!
//! ```text
//! # <pin label> <TAB> <upstream sha> <TAB> <source path at that sha> <TAB> <via>
//! roles/curator.md<TAB>0123…cdef<TAB>defaults/.claude/commands/loom/curator.md<TAB>install-metadata
//! ```
//!
//! Design constraints (all from the issue's curation):
//!
//! - **The pin syntax is untouched.** `resync-installed.sh`'s vendored
//!   `is_ignored()` and `init::repo_owned`'s `parse_resync_ignore` never read
//!   the sidecar, so pin *protection* works exactly as before, with or without
//!   a daemon new enough to know about it.
//! - **Written by a supported operation** (`loom-daemon resync-pin add`), not a
//!   hand-maintained field: it appends the pin and records the base in one step.
//! - **A recorded base is never replaced implicitly** — not by resync (which
//!   does not read or write the sidecar) and not by a repeated `add` (which
//!   keeps the existing entry unless `--replace-base` is passed).
//! - **No fabricated provenance.** A base is recorded only after it resolves to
//!   a commit in a Loom *source* checkout (never the consumer repo's HEAD) and
//!   the mapped source path exists at that commit. Anything unresolvable is
//!   reported as unknown, and drift is never reported as zero without proof.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pin list, read by `resync-installed.sh` and `init::repo_owned`.
pub const IGNORE_FILE: &str = ".loom/resync-ignore";
/// Fork-point sidecar written by [`add_pin`].
pub const BASE_FILE: &str = ".loom/resync-pin-base";

const BASE_HEADER: &str = "\
# .loom/resync-pin-base — upstream fork point of each .loom/resync-ignore pin.
# Written by `loom-daemon resync-pin add`; read by `loom-daemon resync-pin status`.
# Columns (tab-separated): pin label, upstream Loom commit, source path at that
# commit, how the commit was chosen (explicit | install-metadata).
# Commit this file alongside .loom/resync-ignore. Resync never rewrites it.
";

/// Installed-label prefix → (`defaults/` source prefix, installed prefix).
/// Mirrors the `resync_tree` / `sync_one` call sites in
/// `defaults/scripts/resync-installed.sh` (label = the `rel` it passes to
/// `is_ignored()`).
const PREFIX_MAP: &[(&str, &str, &str)] = &[
    ("hooks/", "defaults/hooks/", ".loom/hooks/"),
    ("scripts/", "defaults/scripts/", ".loom/scripts/"),
    ("roles/", "defaults/roles/", ".loom/roles/"),
    ("docs/", "defaults/docs/", ".loom/docs/"),
    ("runtimes/", "defaults/runtimes/", ".loom/runtimes/"),
    ("bin/", "defaults/.loom/bin/", ".loom/bin/"),
    ("commands/loom/", "defaults/.claude/commands/loom/", ".claude/commands/loom/"),
    ("agents-skills/", "defaults/.agents/skills/", ".agents/skills/"),
];

/// Single-file labels resync passes verbatim → `defaults/` source path.
const EXACT_MAP: &[(&str, &str)] = &[
    (".claude/README.md", "defaults/.claude/README.md"),
    (".github/CONFIGURATION.md", "defaults/.github/CONFIGURATION.md"),
    (".loom/biome.jsonc", "defaults/.loom/biome.jsonc"),
    (".claude/biome.jsonc", "defaults/.claude/biome.jsonc"),
    (".loom/pricing.json", "defaults/pricing.json"),
];

/// Canonical pin label for a user-supplied path: the `rel` form
/// `resync-installed.sh` passes to `is_ignored()`. Strips one leading `./`,
/// then one leading `.loom/` **only** when the remainder is still nested —
/// the same guard `is_ignored()` applies so `.loom/CLAUDE.md` never collapses
/// onto a bare top-level name.
pub fn canonical_label(raw: &str) -> String {
    let line = raw.trim();
    let no_dot = line.strip_prefix("./").unwrap_or(line);
    match no_dot.strip_prefix(".loom/") {
        Some(rest) if rest.contains('/') => rest.to_string(),
        _ => no_dot.to_string(),
    }
}

/// `defaults/`-relative source path for a canonical label, if Loom ships one.
pub fn default_source_path(label: &str) -> Option<String> {
    if let Some((_, src)) = EXACT_MAP.iter().find(|(l, _)| *l == label) {
        return Some((*src).to_string());
    }
    PREFIX_MAP
        .iter()
        .find_map(|(p, src, _)| label.strip_prefix(p).map(|rest| format!("{src}{rest}")))
        .filter(|s| !s.ends_with('/'))
}

/// Repo-relative installed path for a canonical label.
pub fn installed_path(label: &str) -> String {
    PREFIX_MAP
        .iter()
        .find_map(|(p, _, inst)| label.strip_prefix(p).map(|rest| format!("{inst}{rest}")))
        .unwrap_or_else(|| label.to_string())
}

/// Canonical labels of every pin in `.loom/resync-ignore`, in file order,
/// deduplicated. Same comment/blank-line handling as `is_ignored()`.
pub fn read_pins(workspace: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(workspace.join(IGNORE_FILE)) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let label = canonical_label(line);
        if !out.contains(&label) {
            out.push(label);
        }
    }
    out
}

/// One recorded fork point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseEntry {
    pub label: String,
    pub sha: String,
    pub source_path: String,
    pub via: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SidecarLine {
    Entry(BaseEntry),
    /// Comments, blanks, and lines this version cannot parse — preserved
    /// byte-for-byte on rewrite so nothing a human wrote is ever dropped.
    Raw(String),
}

/// Parsed `.loom/resync-pin-base`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sidecar {
    lines: Vec<SidecarLine>,
}

fn is_full_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Sidecar {
    pub fn load(workspace: &Path) -> Self {
        let text = fs::read_to_string(workspace.join(BASE_FILE)).unwrap_or_default();
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Self {
        let lines = text
            .lines()
            .map(|raw| {
                let body = raw.split('#').next().unwrap_or("").trim();
                let cols: Vec<&str> = body.split('\t').map(str::trim).collect();
                match cols.as_slice() {
                    [label, sha, src, rest @ ..]
                        if !label.is_empty() && is_full_sha(sha) && !src.is_empty() =>
                    {
                        SidecarLine::Entry(BaseEntry {
                            label: canonical_label(label),
                            sha: sha.to_ascii_lowercase(),
                            source_path: (*src).to_string(),
                            via: rest.first().copied().unwrap_or("unknown").to_string(),
                        })
                    }
                    _ => SidecarLine::Raw(raw.to_string()),
                }
            })
            .collect();
        Self { lines }
    }

    pub fn get(&self, label: &str) -> Option<&BaseEntry> {
        self.entries().find(|e| e.label == label)
    }

    pub fn entries(&self) -> impl Iterator<Item = &BaseEntry> {
        self.lines.iter().filter_map(|l| match l {
            SidecarLine::Entry(e) => Some(e),
            SidecarLine::Raw(_) => None,
        })
    }

    /// Non-comment, non-blank lines that did not parse as an entry.
    pub fn malformed(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                SidecarLine::Raw(r) if !r.split('#').next().unwrap_or("").trim().is_empty() => {
                    Some(r.as_str())
                }
                _ => None,
            })
            .collect()
    }

    /// Insert or replace the entry for `entry.label`.
    pub fn upsert(&mut self, entry: BaseEntry) {
        if self.lines.is_empty() {
            self.lines = BASE_HEADER
                .lines()
                .map(|l| SidecarLine::Raw(l.to_string()))
                .collect();
        }
        match self
            .lines
            .iter_mut()
            .find(|l| matches!(l, SidecarLine::Entry(e) if e.label == entry.label))
        {
            Some(slot) => *slot = SidecarLine::Entry(entry),
            None => self.lines.push(SidecarLine::Entry(entry)),
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for l in &self.lines {
            match l {
                SidecarLine::Raw(r) => out.push_str(r),
                SidecarLine::Entry(e) => {
                    out.push_str(&format!("{}\t{}\t{}\t{}", e.label, e.sha, e.source_path, e.via))
                }
            }
            out.push('\n');
        }
        out
    }

    pub fn save(&self, workspace: &Path) -> std::io::Result<()> {
        write_atomic(&workspace.join(BASE_FILE), &self.render())
    }
}

fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Source checkout (the Loom repo the pinned content came from)
// ---------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn git_str(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args).map(|b| String::from_utf8_lossy(&b).trim().to_string())
}

/// Locate the Loom source checkout: explicit `--source`, else the
/// `.loom/loom-source-path` sidecar `resync-installed.sh` itself uses, else the
/// workspace when it *is* the Loom source repo (it has a `defaults/` tree).
pub fn find_source(workspace: &Path, explicit: Option<&Path>) -> Result<PathBuf, String> {
    let candidate = if let Some(p) = explicit {
        p.to_path_buf()
    } else if let Ok(s) = fs::read_to_string(workspace.join(".loom/loom-source-path")) {
        PathBuf::from(s.trim())
    } else if workspace.join("defaults").is_dir() {
        workspace.to_path_buf()
    } else {
        return Err("no Loom source checkout: pass --source <path-to-loom-clone> \
                    (or write its path to .loom/loom-source-path)"
            .into());
    };
    if git_str(&candidate, &["rev-parse", "--git-dir"]).is_none() {
        return Err(format!("source checkout {} is not a git repository", candidate.display()));
    }
    Ok(candidate)
}

/// Resolve `rev` to a full commit sha in `source`, or `None`.
pub fn resolve_commit(source: &Path, rev: &str) -> Option<String> {
    if rev.is_empty() || rev.starts_with('-') {
        return None;
    }
    git_str(
        source,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
    )
    .filter(|s| is_full_sha(s))
}

/// Normalise `a/b/../c` → `a/c`; `None` if it escapes the repo root.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// The path of the regular file `path` names at `sha`, following in-tree
/// symlinks (every `defaults/roles/*.md` is a symlink into
/// `defaults/.claude/commands/loom/`, so diffing the link itself would only
/// ever show a link-target change). `None` if absent at that commit.
pub fn resolve_source_path(source: &Path, sha: &str, path: &str) -> Option<String> {
    let mut cur = normalize(path)?;
    for _ in 0..8 {
        let out = git(source, &["ls-tree", "-z", sha, "--", &cur])?;
        let entry = String::from_utf8_lossy(&out);
        let mode = entry.split(' ').next().filter(|m| !m.is_empty())?;
        match mode {
            "120000" => {
                let target = git(source, &["cat-file", "blob", &format!("{sha}:{cur}")])?;
                let target = String::from_utf8_lossy(&target).trim().to_string();
                let dir = cur.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                cur = normalize(&format!("{dir}/{target}"))?;
            }
            "100644" | "100755" => return Some(cur),
            _ => return None,
        }
    }
    None
}

fn blob(source: &Path, sha: &str, path: &str) -> Option<Vec<u8>> {
    git(source, &["cat-file", "blob", &format!("{sha}:{path}")])
}

fn install_metadata_commit(workspace: &Path) -> Option<String> {
    let text = fs::read_to_string(workspace.join(".loom/install-metadata.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("loom_commit")?.as_str().map(str::to_string)
}

// ---------------------------------------------------------------------------
// `resync-pin add`
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct AddOptions {
    pub path: String,
    pub base: Option<String>,
    pub source: Option<PathBuf>,
    pub source_path: Option<String>,
    pub replace_base: bool,
}

/// What [`add_pin`] did with the fork point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseOutcome {
    Recorded(BaseEntry),
    /// An entry already existed and `--replace-base` was not given.
    Kept(BaseEntry),
    /// Nothing recorded; the reason is actionable.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddReport {
    pub label: String,
    /// `true` when this call appended the pin (it was not already present).
    pub pinned_now: bool,
    pub base: BaseOutcome,
    /// Informational: how the installed file compares to the base blob.
    pub content_note: Option<String>,
}

pub fn validate_label(label: &str) -> Result<(), String> {
    if label.is_empty()
        || label.starts_with('/')
        || label.contains('#')
        || label.chars().any(char::is_whitespace)
        || label.split('/').any(|s| s == ".." || s.is_empty())
    {
        return Err(format!("invalid pin path {label:?}: expected a relative file path"));
    }
    Ok(())
}

/// Pin `opts.path` in `.loom/resync-ignore` (if not already pinned) and record
/// its upstream fork point in `.loom/resync-pin-base`. The pin is written
/// **first**, so a base that cannot be resolved never costs the protection.
pub fn add_pin(workspace: &Path, opts: &AddOptions) -> Result<AddReport, String> {
    let label = canonical_label(&opts.path);
    validate_label(&label)?;

    let pinned_before = read_pins(workspace).contains(&label);
    if !pinned_before {
        let path = workspace.join(IGNORE_FILE);
        let mut text = fs::read_to_string(&path).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&label);
        text.push('\n');
        write_atomic(&path, &text).map_err(|e| format!("write {IGNORE_FILE}: {e}"))?;
    }

    let mut sidecar = Sidecar::load(workspace);
    let report = |base, content_note| AddReport {
        label: label.clone(),
        pinned_now: !pinned_before,
        base,
        content_note,
    };
    if let Some(existing) = sidecar.get(&label).cloned() {
        if !opts.replace_base {
            return Ok(report(BaseOutcome::Kept(existing), None));
        }
    }

    let (rev, via) = match (&opts.base, pinned_before) {
        (Some(b), _) => (b.clone(), "explicit"),
        (None, false) => match install_metadata_commit(workspace) {
            Some(c) => (c, "install-metadata"),
            None => {
                return Ok(report(
                    BaseOutcome::Unknown(
                        ".loom/install-metadata.json records no loom_commit; pass --base <rev>"
                            .into(),
                    ),
                    None,
                ))
            }
        },
        (None, true) => {
            return Ok(report(
                BaseOutcome::Unknown(
                    "path was already pinned, so install metadata (the last resync, which \
                     skipped this file) is not evidence of its fork point; pass --base <rev>"
                        .into(),
                ),
                None,
            ))
        }
    };

    let source = match find_source(workspace, opts.source.as_deref()) {
        Ok(s) => s,
        Err(e) => return Ok(report(BaseOutcome::Unknown(e), None)),
    };
    let Some(sha) = resolve_commit(&source, &rev) else {
        return Ok(report(
            BaseOutcome::Unknown(format!(
                "revision {rev:?} does not resolve to a commit in source checkout {} \
                 (fetch it, or pass --base <rev>)",
                source.display()
            )),
            None,
        ));
    };
    let Some(mapped) = opts
        .source_path
        .clone()
        .or_else(|| default_source_path(&label))
    else {
        return Ok(report(
            BaseOutcome::Unknown(format!(
                "no known defaults/ source for {label:?}; pass --source-path <path-in-loom-repo>"
            )),
            None,
        ));
    };
    let Some(source_path) = resolve_source_path(&source, &sha, &mapped) else {
        return Ok(report(BaseOutcome::Unknown(format!("{mapped} does not exist at {sha}")), None));
    };

    let content_note = match fs::read(workspace.join(installed_path(&label))) {
        Err(_) => Some("installed file not found".to_string()),
        Ok(local) if Some(&local) == blob(&source, &sha, &source_path).as_ref() => {
            Some("installed file is byte-identical to the base".to_string())
        }
        Ok(_) => Some("installed file differs from the base (local patch)".to_string()),
    };
    let entry = BaseEntry {
        label: label.clone(),
        sha,
        source_path,
        via: via.to_string(),
    };
    sidecar.upsert(entry.clone());
    sidecar
        .save(workspace)
        .map_err(|e| format!("write {BASE_FILE}: {e}"))?;
    Ok(report(BaseOutcome::Recorded(entry), content_note))
}

// ---------------------------------------------------------------------------
// `resync-pin status` — drift from the recorded fork point
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// Measured: `commits` upstream commits touched the file since the base,
    /// changing `added`/`removed` lines. `upstream_path` is `None` when the
    /// file no longer exists upstream.
    Measured {
        commits: u64,
        added: u64,
        removed: u64,
        upstream_path: Option<String>,
        diff_cmd: String,
    },
    /// Not measurable; the reason is actionable. Never rendered as zero.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinStatus {
    pub label: String,
    pub base: Option<BaseEntry>,
    pub drift: Drift,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusReport {
    pub pins: Vec<PinStatus>,
    /// Sidecar entries whose pin is no longer in `.loom/resync-ignore`.
    pub orphaned: Vec<BaseEntry>,
    pub malformed: Vec<String>,
    pub upstream: Option<String>,
}

pub fn status(workspace: &Path, source: Option<&Path>, upstream_rev: &str) -> StatusReport {
    let pins = read_pins(workspace);
    let sidecar = Sidecar::load(workspace);
    let src = find_source(workspace, source);
    let upstream = src
        .as_ref()
        .ok()
        .and_then(|s| resolve_commit(s, upstream_rev));

    let measure = |e: &BaseEntry| -> Drift {
        let s = match &src {
            Ok(s) => s,
            Err(err) => return Drift::Unknown(err.clone()),
        };
        let Some(up) = &upstream else {
            return Drift::Unknown(format!(
                "upstream {upstream_rev:?} does not resolve in {}",
                s.display()
            ));
        };
        if resolve_commit(s, &e.sha).is_none() {
            return Drift::Unknown(format!(
                "recorded base {} is not in source checkout {} (fetch it)",
                e.sha,
                s.display()
            ));
        }
        measure_drift(s, &e.sha, &e.source_path, up)
    };

    let pins_out = pins
        .iter()
        .map(|label| {
            let base = sidecar.get(label).cloned();
            let drift = match &base {
                None => Drift::Unknown(format!(
                    "no recorded fork point (legacy pin); record one with \
                     `loom-daemon resync-pin add {label} --base <rev>`"
                )),
                Some(e) => measure(e),
            };
            PinStatus {
                label: label.clone(),
                base,
                drift,
            }
        })
        .collect();
    StatusReport {
        pins: pins_out,
        orphaned: sidecar
            .entries()
            .filter(|e| !pins.contains(&e.label))
            .cloned()
            .collect(),
        malformed: sidecar
            .malformed()
            .into_iter()
            .map(str::to_string)
            .collect(),
        upstream,
    }
}

/// Commits and line changes between `base:path` and the same file at `up`.
pub fn measure_drift(source: &Path, base: &str, path: &str, up: &str) -> Drift {
    let up_path = resolve_source_path(source, up, path);
    let mut paths = vec![path.to_string()];
    if let Some(p) = &up_path {
        if p != path {
            paths.push(p.clone());
        }
    }
    let range = format!("{base}..{up}");
    let mut args = vec!["rev-list", "--count", range.as_str(), "--"];
    args.extend(paths.iter().map(String::as_str));
    let Some(commits) = git_str(source, &args).and_then(|s| s.parse::<u64>().ok()) else {
        return Drift::Unknown(format!("git rev-list failed for {range}"));
    };
    let (added, removed, diff_cmd) = match &up_path {
        Some(p) => {
            let (a, b) = (format!("{base}:{path}"), format!("{up}:{p}"));
            let Some(ns) = git_str(source, &["diff", "--numstat", &a, &b]) else {
                return Drift::Unknown(format!("git diff failed for {a} {b}"));
            };
            let mut it = ns.split_whitespace();
            let n = |x: Option<&str>| x.and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            let (add, del) = (n(it.next()), n(it.next()));
            let cmd = if p == path {
                format!("git -C {} diff {base} {up} -- {path}", source.display())
            } else {
                format!("git -C {} diff {a} {b}", source.display())
            };
            (add, del, cmd)
        }
        None => (0, 0, format!("git -C {} show {base}:{path}", source.display())),
    };
    Drift::Measured {
        commits,
        added,
        removed,
        upstream_path: up_path,
        diff_cmd,
    }
}

impl StatusReport {
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.pins.is_empty() {
            out.push_str("No pins in .loom/resync-ignore.\n");
        }
        for p in &self.pins {
            let base = p
                .base
                .as_ref()
                .map(|b| format!("base {} ({}, via {})", &b.sha[..12], b.source_path, b.via))
                .unwrap_or_else(|| "base UNKNOWN".into());
            out.push_str(&format!("{}: {base}\n", p.label));
            match &p.drift {
                Drift::Measured {
                    commits,
                    added,
                    removed,
                    upstream_path,
                    diff_cmd,
                } => {
                    match upstream_path {
                        Some(_) => out.push_str(&format!(
                            "  drift: {commits} upstream commit(s), +{added}/-{removed} lines\n"
                        )),
                        None => out.push_str(&format!(
                            "  drift: {commits} upstream commit(s); file REMOVED upstream\n"
                        )),
                    }
                    out.push_str(&format!("  review: {diff_cmd}\n"));
                }
                Drift::Unknown(why) => out.push_str(&format!("  drift: unknown — {why}\n")),
            }
        }
        for e in &self.orphaned {
            out.push_str(&format!(
                "orphaned base entry (no matching pin): {} {}\n",
                e.label, e.sha
            ));
        }
        for m in &self.malformed {
            out.push_str(&format!("malformed {BASE_FILE} line (ignored): {m}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests;
