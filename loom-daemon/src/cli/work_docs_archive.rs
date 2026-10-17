//! `loom-daemon work-docs archive|check` — content-preserving rotation of
//! `WORK_LOG.md` into dated monthly archives (issue #12633, slice 1 of
//! #12589). Layout, eligibility floor and recovery: `defaults/docs/work-docs-archive.md`.
//!
//! The unit of rotation is a whole `### YYYY-MM-DD` section, moved
//! byte-for-byte. Everything else (the preamble, any other heading and its
//! body) stays where it is; the `## Archives` index is regenerated in place.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{Duration, NaiveDate};

/// Smallest accepted `--older-than-days`. Guide's candidate lookback is 30
/// days (`update_work_log()` in `guide.md`); rotating anything inside it would
/// make Guide's presence checks re-append the moved entries.
pub(crate) const MIN_OLDER_THAN_DAYS: i64 = 45;
const DEFAULT_OLDER_THAN_DAYS: i64 = 60;

const ACTIVE_FILE: &str = "WORK_LOG.md";
const ARCHIVE_DIR: &str = "work_log";
const INDEX_HEADING: &str = "## Archives";

#[derive(clap::Subcommand)]
pub(crate) enum WorkDocsCommand {
    /// Move `### YYYY-MM-DD` sections older than the cutoff out of
    /// `WORK_LOG.md` into `work_log/WORK_LOG_YYYY-MM.md`, byte-for-byte.
    ///
    /// Repeatable: a second run is a byte-identical no-op. Fails non-zero and
    /// writes nothing when an archive already holds a date being moved, an
    /// archive path is not a regular file, or conservation (`check`) would not
    /// hold afterwards. A log with no parseable date headings is a no-op.
    Archive(ArchiveArgs),
    /// Verify every `- **PR #N**` / `- **Issue #N**` bullet appears exactly
    /// once across `WORK_LOG.md` and its archives, and that the `## Archives`
    /// index links resolve.
    Check(RootArgs),
}

#[derive(clap::Args)]
pub(crate) struct RootArgs {
    /// Directory holding `WORK_LOG.md` (default: the current directory).
    #[arg(long, value_name = "DIR", default_value = ".")]
    root: PathBuf,
}

#[derive(clap::Args)]
pub(crate) struct ArchiveArgs {
    #[command(flatten)]
    root: RootArgs,

    /// Archive sections strictly older than this many days. Minimum 45, so
    /// the cutoff stays older than Guide's 30-day lookback.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_OLDER_THAN_DAYS)]
    older_than_days: i64,

    /// Report what would move; write nothing.
    #[arg(long)]
    dry_run: bool,
}

impl WorkDocsCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Archive(args) => {
                let today = chrono::Utc::now().date_naive();
                let report = archive(&args.root.root, args.older_than_days, today, args.dry_run)?;
                println!("{report}");
            }
            Self::Check(args) => {
                let report = check(&args)?;
                println!("{report}");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// Everything above the first date heading (or index).
    Preamble,
    /// A parseable `### YYYY-MM-DD` section.
    Dated(NaiveDate),
    /// The `## Archives` index.
    Index,
    /// Any other heading and its body: never moved.
    Other,
}

#[derive(Debug, Clone)]
struct Block {
    kind: Kind,
    text: String,
}

fn date_heading(line: &str) -> Option<NaiveDate> {
    let rest = line.strip_prefix("### ")?.trim_end();
    if rest.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(rest, "%Y-%m-%d").ok()
}

/// Split `text` into blocks whose concatenation is exactly `text`.
fn parse(text: &str) -> Vec<Block> {
    let mut blocks = vec![Block {
        kind: Kind::Preamble,
        text: String::new(),
    }];
    for line in text.split_inclusive('\n') {
        let current = blocks
            .last()
            .map(|b| b.kind.clone())
            .unwrap_or(Kind::Preamble);
        let next = if let Some(date) = date_heading(line) {
            Some(Kind::Dated(date))
        } else if line.trim_end() == INDEX_HEADING {
            Some(Kind::Index)
        } else if (matches!(current, Kind::Index) && line.starts_with('#'))
            || (matches!(current, Kind::Dated(_))
                && (line.starts_with("### ") || line.starts_with("## ") || line.starts_with("# ")))
        {
            Some(Kind::Other)
        } else {
            None
        };
        match next {
            Some(kind) => blocks.push(Block {
                kind,
                text: line.to_string(),
            }),
            None => {
                if let Some(last) = blocks.last_mut() {
                    last.text.push_str(line);
                }
            }
        }
    }
    blocks.retain(|b| !b.text.is_empty());
    blocks
}

/// Concatenate blocks, adding a newline only where a block that lacks one is
/// followed by another (only ever the file's last line).
fn render(blocks: &[Block]) -> String {
    let mut out = String::new();
    for b in blocks {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&b.text);
    }
    out
}

/// `PR #N` / `Issue #N` for a line that starts a bullet entry. Anchored at
/// the line start (#6087): a title mentioning `PR #123` never counts.
fn bullet_key(line: &str) -> Option<String> {
    let rest = line.strip_prefix("- **")?;
    let (kind, rest) = if let Some(r) = rest.strip_prefix("PR #") {
        ("PR", r)
    } else {
        ("Issue", rest.strip_prefix("Issue #")?)
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || !rest[digits.len()..].starts_with("**") {
        return None;
    }
    Some(format!("{kind} #{digits}"))
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Tally {
    keys: BTreeMap<String, usize>,
    bullets: usize,
    /// Lines in Dated and Other blocks (preamble and index excluded).
    lines: usize,
}

impl Tally {
    fn add(&mut self, blocks: &[Block]) {
        for b in blocks {
            if matches!(b.kind, Kind::Preamble | Kind::Index) {
                continue;
            }
            for line in b.text.lines() {
                self.lines += 1;
                if line.starts_with("- ") {
                    self.bullets += 1;
                }
                if let Some(k) = bullet_key(line) {
                    *self.keys.entry(k).or_default() += 1;
                }
            }
        }
    }

    fn duplicates(&self) -> Vec<&str> {
        self.keys
            .iter()
            .filter(|(_, n)| **n > 1)
            .map(|(k, _)| k.as_str())
            .collect()
    }
}

fn archive_name(month: &str) -> String {
    format!("WORK_LOG_{month}.md")
}

fn month_of(date: NaiveDate) -> String {
    date.format("%Y-%m").to_string()
}

/// `WORK_LOG_YYYY-MM.md` -> `YYYY-MM`.
fn archive_month(file_name: &str) -> Option<String> {
    let month = file_name.strip_prefix("WORK_LOG_")?.strip_suffix(".md")?;
    NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d").ok()?;
    Some(month.to_string())
}

/// Read a regular file; error if the path exists and is not one.
fn read_regular(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some(
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?,
        )),
        Ok(_) => bail!("{} exists but is not a regular file", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

/// Archive months present on disk, with their parsed blocks.
fn load_archives(dir: &Path) -> Result<BTreeMap<String, (String, Vec<Block>)>> {
    let mut out = BTreeMap::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(month) = archive_month(&name) else {
            continue;
        };
        let text = read_regular(&entry.path())?.unwrap_or_default();
        let blocks = parse(&text);
        out.insert(month, (text, blocks));
    }
    Ok(out)
}

fn index_block(months: &[String]) -> Block {
    let mut text = format!("{INDEX_HEADING}\n\n");
    for m in months {
        text.push_str(&format!("- [{m}]({ARCHIVE_DIR}/{})\n", archive_name(m)));
    }
    text.push('\n');
    Block {
        kind: Kind::Index,
        text,
    }
}

/// Months newest first.
fn months_desc<'a>(months: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut v: Vec<String> = months.cloned().collect();
    v.sort_unstable_by(|a, b| b.cmp(a));
    v
}

/// Put the (regenerated) index where the old one was, else just before the
/// first non-preamble block.
fn set_index(blocks: &mut Vec<Block>, months: &[String]) {
    let new = index_block(months);
    if let Some(pos) = blocks.iter().position(|b| b.kind == Kind::Index) {
        blocks[pos] = new;
    } else {
        let pos = blocks
            .iter()
            .position(|b| b.kind != Kind::Preamble)
            .unwrap_or(blocks.len());
        blocks.insert(pos, new);
    }
}

fn new_archive_preamble(month: &str) -> Block {
    Block {
        kind: Kind::Preamble,
        text: format!(
            "# Work Log Archive — {month}\n\nSections moved byte-for-byte from \
             [WORK_LOG.md](../WORK_LOG.md) by `loom-daemon work-docs archive`.\n\n"
        ),
    }
}

fn atomic_write(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("md.tmp");
    fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
}

/// Rotate eligible sections of `<root>/WORK_LOG.md`. `today` is injected for
/// tests. Returns the human-readable report.
pub(crate) fn archive(
    root: &Path,
    older_than_days: i64,
    today: NaiveDate,
    dry_run: bool,
) -> Result<String> {
    if older_than_days < MIN_OLDER_THAN_DAYS {
        bail!(
            "--older-than-days {older_than_days} is below the minimum {MIN_OLDER_THAN_DAYS}: \
             the cutoff must stay older than Guide's 30-day candidate lookback"
        );
    }
    let cutoff = today - Duration::days(older_than_days);
    let active_path = root.join(ACTIVE_FILE);
    let Some(active_text) = read_regular(&active_path)? else {
        bail!("{} not found", active_path.display());
    };
    let active = parse(&active_text);
    if !active.iter().any(|b| matches!(b.kind, Kind::Dated(_))) {
        return Ok(format!("{ACTIVE_FILE} has no date headings: nothing to do"));
    }

    let dir = root.join(ARCHIVE_DIR);
    if let Ok(meta) = fs::symlink_metadata(&dir) {
        if !meta.is_dir() {
            bail!("{} exists but is not a directory", dir.display());
        }
    }
    let archives = load_archives(&dir)?;

    // Pre-move tally over everything.
    let mut before = Tally::default();
    before.add(&active);
    for (_, blocks) in archives.values() {
        before.add(blocks);
    }

    // Partition.
    let mut kept = Vec::new();
    let mut moving: BTreeMap<String, Vec<Block>> = BTreeMap::new();
    for b in active {
        match b.kind {
            Kind::Dated(d) if d < cutoff => moving.entry(month_of(d)).or_default().push(b),
            _ => kept.push(b),
        }
    }
    if moving.is_empty() {
        return Ok(format!("no sections older than {cutoff}: nothing to do"));
    }

    // Build new archive contents, refusing on any date conflict.
    let mut new_archives: BTreeMap<String, String> = BTreeMap::new();
    for (month, moved) in &moving {
        let (mut preamble, mut dated, other) = match archives.get(month) {
            Some((_, blocks)) => {
                let mut pre = Vec::new();
                let mut dated = Vec::new();
                let mut other = Vec::new();
                for b in blocks {
                    match b.kind {
                        Kind::Preamble => pre.push(b.clone()),
                        Kind::Dated(_) => dated.push(b.clone()),
                        _ => other.push(b.clone()),
                    }
                }
                (pre, dated, other)
            }
            None => (vec![new_archive_preamble(month)], Vec::new(), Vec::new()),
        };
        for m in moved {
            if let Kind::Dated(d) = m.kind {
                if dated.iter().any(|e| e.kind == Kind::Dated(d)) {
                    bail!(
                        "conflict: {} already holds a section dated {d}; nothing written",
                        dir.join(archive_name(month)).display()
                    );
                }
            }
        }
        // Moved sections first so a stable newest-first sort keeps their order.
        let mut merged: Vec<Block> = moved.clone();
        merged.append(&mut dated);
        merged.sort_by_key(|b| match b.kind {
            Kind::Dated(d) => std::cmp::Reverse(d),
            _ => std::cmp::Reverse(NaiveDate::MIN),
        });
        preamble.extend(merged);
        preamble.extend(other);
        new_archives.insert(month.clone(), render(&preamble));
    }

    // Regenerate the index from the archives that will exist.
    let mut all_months: Vec<&String> = archives.keys().collect();
    all_months.extend(new_archives.keys());
    let months = months_desc(all_months.into_iter());
    set_index(&mut kept, &months);
    let new_active = render(&kept);

    // Conservation over the post-move state.
    let mut after = Tally::default();
    after.add(&parse(&new_active));
    for (month, (_, blocks)) in &archives {
        if !new_archives.contains_key(month) {
            after.add(blocks);
        }
    }
    for text in new_archives.values() {
        after.add(&parse(text));
    }
    verify_conservation(&before, &after)?;

    let moved_sections: usize = moving.values().map(Vec::len).sum();
    let report = format!(
        "{}{moved_sections} sections older than {cutoff} -> {} archive file(s); \
         {ACTIVE_FILE}: {} -> {} lines",
        if dry_run { "dry run: " } else { "" },
        new_archives.len(),
        active_text.lines().count(),
        new_active.lines().count(),
    );
    if dry_run {
        return Ok(report);
    }

    // Write archives first, the active file last; restore on failure.
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut done: Vec<(PathBuf, Option<String>)> = Vec::new();
    let mut write_all = || -> Result<()> {
        for (month, text) in &new_archives {
            let path = dir.join(archive_name(month));
            let original = archives.get(month).map(|(t, _)| t.clone());
            atomic_write(&path, text)?;
            done.push((path, original));
        }
        atomic_write(&active_path, &new_active)
    };
    if let Err(e) = write_all() {
        for (path, original) in done {
            let _ = match original {
                Some(t) => fs::write(&path, t),
                None => fs::remove_file(&path),
            };
        }
        return Err(e.context("write failed; archives rolled back"));
    }
    Ok(report)
}

fn verify_conservation(before: &Tally, after: &Tally) -> Result<()> {
    if before.keys != after.keys || before.bullets != after.bullets || before.lines != after.lines {
        bail!(
            "conservation failed: before {} bullets / {} lines, after {} bullets / {} lines; \
             nothing written",
            before.bullets,
            before.lines,
            after.bullets,
            after.lines
        );
    }
    let dups = after.duplicates();
    if !dups.is_empty() {
        bail!("conservation failed: entries appear more than once: {}", dups.join(", "));
    }
    Ok(())
}

/// Uniqueness across active plus archives, and index link integrity.
pub(crate) fn check(args: &RootArgs) -> Result<String> {
    let root = &args.root;
    let active_path = root.join(ACTIVE_FILE);
    let Some(text) = read_regular(&active_path)? else {
        bail!("{} not found", active_path.display());
    };
    let active = parse(&text);
    let archives = load_archives(&root.join(ARCHIVE_DIR))?;
    let mut tally = Tally::default();
    tally.add(&active);
    for (_, blocks) in archives.values() {
        tally.add(blocks);
    }
    let dups = tally.duplicates();
    if !dups.is_empty() {
        bail!("entries appear more than once: {}", dups.join(", "));
    }
    for b in active.iter().filter(|b| b.kind == Kind::Index) {
        for line in b.text.lines() {
            let Some(target) = line.split("](").nth(1).and_then(|r| r.strip_suffix(')')) else {
                continue;
            };
            let missing = match fs::symlink_metadata(root.join(target)) {
                Ok(m) => !m.is_file(),
                Err(_) => true,
            };
            if missing {
                bail!("index link {target} does not resolve to a regular file");
            }
        }
    }
    Ok(format!(
        "ok: {} entries unique across {ACTIVE_FILE} and {} archive file(s)",
        tally.keys.len(),
        archives.len()
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "work_docs_archive_tests.rs"]
mod tests;
