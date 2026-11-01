//! The resync waiver (#13096) as one native implementation, and the daemon's
//! landing of its own resync pull request on it (#13487).
//!
//! A `chore(loom): resync installed Loom to vX` PR copies files Loom already
//! reviewed and merged. [`evaluate`] is the mechanical proof that a PR is
//! exactly that: the title shape, an install-set-only file list with no
//! merge-gate path, the metadata at the head naming the same version, the tag
//! existing, a complete file list, and every kept-set file byte-identical
//! (blob SHA) to its `defaults/` counterpart at the tag. Every read that
//! fails, and every surprise, is `NotEligible`: the proof fails closed.
//!
//! Champion's `resync_waiver_eval` calls this through
//! `loom-daemon forge resync-waiver`, so the daemon and Champion hold one
//! copy of the rules and cannot drift.
//!
//! [`land`] is the daemon's use of the proof: eligible, and green checks on
//! the same head, merges through the repo's own `merge-pr.sh` pinned to that
//! head. Anything else is [`Waived::Lane`] and the caller labels the PR
//! `loom:review-requested`, as before.

use std::fmt::Write as _;

/// The kept set (#12528): paths no standing authorization may waive. Mirrors
/// `KEPT_SET_PATTERNS` in `champion-pr-merge.md`; bash `[[ == ]]` globs where
/// only `*` is special and it matches `/` too. A denylist, so it errs broad.
pub const KEPT_SET: &[&str] = &[
    "*champion*",
    "*judge*",
    "*merge-pr.sh",
    "*merge_pr*",
    "*verdict*",
    "*check-review-feedback.sh",
    "*comment_trust*",
    "*trusted_comments*",
    "*claim_reconciliation*",
    "*version_only_diff*",
    "*version-only-diff*",
    "*.loom/config.json",
    "*defaults/config.json",
    "*forge_identity*",
    "*credential*",
    "*tokens*",
    "*github-app-token*",
    "*token-precedence*",
    "*spawn-claude.sh",
    "*spawn-worker.sh",
    "*guard*",
    "*hook-wiring*",
    ".githooks/*",
    "*.claude/settings.json",
    "*install-loom.sh",
    "*install.sh",
    "*resync-installed.sh",
    "*auto_update*",
    "*daemon_update*",
    "*daemon-update*",
    "*install_binary*",
    "*land-resync*",
    "*worktree*",
    "*loom-clean*",
    "*cleanup-branches*",
    ".github/labels.yml",
    "*/.github/labels.yml",
    "defaults/labels.json",
    "*/defaults/labels.json",
    "*labels_migrate*",
    "*migrations/*",
    "*_migration.py",
    "*deep_clean*",
    "*reaper*",
    "*quarantine*",
    "*account*",
];

/// Merge-gate entries of the kept set (operator ruling on #12945): a resync
/// that changes one keeps the human hold, because the gate cannot vouch for a
/// change to itself. Mirrors `RESYNC_GATE_PATTERNS`.
pub const GATE: &[&str] = &[
    "*champion*",
    "*judge*",
    "*merge-pr.sh",
    "*merge_pr*",
    "*verdict*",
    "*check-review-feedback.sh",
    "*comment_trust*",
    "*trusted_comments*",
    "*claim_reconciliation*",
    "*version_only_diff*",
    "*version-only-diff*",
    "*.loom/config.json",
    "*defaults/config.json",
];

/// Bash-style glob where `*` matches any run of characters (`/` included)
/// and everything else is literal.
#[must_use]
pub fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, rest) = parts.split_first().expect("split yields one part");
    let Some(mut tail) = text.strip_prefix(first) else {
        return false;
    };
    if rest.is_empty() {
        return tail.is_empty();
    }
    let (last, middle) = rest.split_last().expect("rest is not empty");
    for part in middle {
        match tail.find(part) {
            Some(i) => tail = &tail[i + part.len()..],
            None => return false,
        }
    }
    tail.len() >= last.len() && tail.ends_with(last)
}

fn matches_any(patterns: &[&str], path: &str) -> bool {
    patterns.iter().any(|p| glob(p, path))
}

/// The `defaults/` counterpart of an installed path, per the surface map in
/// `resync-installed.sh`. `None` when the path has none. Roles and generated
/// skill copies are deliberately unmapped (#13313).
#[must_use]
pub fn upstream_path(installed: &str) -> Option<String> {
    const MAP: &[(&str, &str)] = &[
        (".loom/hooks/", "defaults/hooks/"),
        (".loom/scripts/", "defaults/scripts/"),
        (".loom/docs/", "defaults/docs/"),
        (".loom/runtimes/", "defaults/runtimes/"),
        (".loom/bin/", "defaults/.loom/bin/"),
        (".claude/commands/loom/", "defaults/.claude/commands/loom/"),
        (".claude/agents/", "defaults/.claude/agents/"),
    ];
    MAP.iter().find_map(|(from, to)| {
        installed
            .strip_prefix(from)
            .map(|rest| format!("{to}{rest}"))
    })
}

/// A pull request as the proof reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pr {
    pub title: String,
    pub head_sha: String,
    pub changed_files: usize,
}

/// One changed file of a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub filename: String,
    pub previous_filename: Option<String>,
    /// The blob SHA at the PR head.
    pub sha: String,
    pub status: String,
}

/// Every read the proof makes. `Err` is any failure to read: the proof treats
/// it as `NotEligible`.
pub trait Source {
    /// The PR: title, head SHA, `changed_files`.
    fn pr(&self, number: u32) -> Result<Pr, String>;
    /// Every changed file, all pages.
    fn files(&self, number: u32) -> Result<Vec<File>, String>;
    /// `.loom/install-metadata.json` at `sha` in the PR's repo.
    fn metadata(&self, sha: &str) -> Result<String, String>;
    /// Whether `refs/tags/<tag>` exists in `rjwalters/loom`.
    fn tag_exists(&self, tag: &str) -> Result<bool, String>;
    /// The blob SHA of `path` at `tag` in `rjwalters/loom`; `None` when it is
    /// not a file there.
    fn upstream_blob(&self, path: &str, tag: &str) -> Result<Option<String>, String>;
}

/// What the proof concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Eligible {
        tag: String,
        head: String,
        /// One `(path, blob sha)` per kept-set file.
        record: Vec<(String, String)>,
    },
    NotEligible(String),
}

impl Verdict {
    /// The record as the `path: sha` lines Champion's comment renders.
    #[must_use]
    pub fn record_lines(record: &[(String, String)]) -> String {
        record.iter().fold(String::new(), |mut out, (p, s)| {
            let _ = writeln!(out, "{p}: {s}");
            out
        })
    }
}

fn no(why: impl Into<String>) -> Verdict {
    Verdict::NotEligible(why.into())
}

/// `chore(loom): resync installed Loom to v<X.Y.Z>` -> `X.Y.Z`.
fn title_version(title: &str) -> Option<&str> {
    let v = title.strip_prefix("chore(loom): resync installed Loom to v")?;
    let mut parts = v.split('.');
    let ok = (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    }) && parts.next().is_none();
    ok.then_some(v)
}

/// The proof, for PR `number`. Fails closed on every read error.
#[must_use]
pub fn evaluate(src: &dyn Source, number: u32) -> Verdict {
    let pr = match src.pr(number) {
        Ok(p) => p,
        Err(_) => return no("PR unreadable"),
    };
    let Some(version) = title_version(&pr.title) else {
        return no("not a resync PR");
    };
    let tag = format!("v{version}");
    if pr.head_sha.is_empty() {
        return no("head unreadable");
    }
    let meta = src.metadata(&pr.head_sha).ok().and_then(|raw| {
        serde_json::from_str::<serde_json::Value>(&raw)
            .ok()?
            .get("loom_version")?
            .as_str()
            .map(str::to_string)
    });
    if meta.as_deref() != Some(version) {
        return no(format!(
            "install-metadata names '{}', title names {version}",
            meta.as_deref().unwrap_or("none")
        ));
    }
    match src.tag_exists(&tag) {
        Ok(true) => {}
        _ => return no(format!("tag {tag} not found upstream")),
    }
    let Ok(files) = src.files(number) else {
        return no("files unreadable");
    };
    if files.is_empty() || files.len() != pr.changed_files {
        return no("files read incomplete");
    }
    let mut record = Vec::new();
    for f in &files {
        let prev = f.previous_filename.as_deref().unwrap_or(&f.filename);
        if !(f.filename.starts_with(".loom/") || f.filename.starts_with(".claude/")) {
            return no(format!("{} is outside the install set", f.filename));
        }
        if matches_any(GATE, &f.filename) || matches_any(GATE, prev) {
            return no(format!("{} is a merge-gate path", f.filename));
        }
        if !(matches_any(KEPT_SET, &f.filename) || matches_any(KEPT_SET, prev)) {
            continue;
        }
        if f.status != "added" && f.status != "modified" {
            return no(format!("{} is {}", f.filename, f.status));
        }
        let Some(up) = upstream_path(&f.filename) else {
            return no(format!("{} has no upstream counterpart", f.filename));
        };
        match src.upstream_blob(&up, &tag) {
            Ok(Some(sha)) if !sha.is_empty() && sha == f.sha => {
                record.push((f.filename.clone(), f.sha.clone()));
            }
            _ => return no(format!("{} differs from {tag}:{up}", f.filename)),
        }
    }
    if record.is_empty() {
        return no("no kept-set file to waive");
    }
    match src.pr(number) {
        Ok(again) if again.head_sha == pr.head_sha => Verdict::Eligible {
            tag,
            head: pr.head_sha,
            record,
        },
        _ => no("head moved while reading"),
    }
}

/// How the daemon's attempt to land its own resync PR ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waived {
    /// Merged (or handed to the repo's merge queue) by the daemon on the
    /// waiver proof. The PR carries `loom:pr`, never `loom:review-requested`.
    Landed(String),
    /// Not provable or not green: the ordinary lane (`loom:review-requested`).
    Lane(String),
    /// A gate says not now (`fleet.autoApply` off, not H0): nothing is done
    /// and the ordinary lane is left to the caller.
    Held(String),
}

impl Waived {
    /// The status word: `landed-by-daemon`, `handed-to-lane` or `held`.
    #[must_use]
    pub fn word(&self) -> &'static str {
        match self {
            Self::Landed(_) => "landed-by-daemon",
            Self::Lane(_) => "handed-to-lane",
            Self::Held(_) => "held",
        }
    }

    /// The reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Landed(r) | Self::Lane(r) | Self::Held(r) => r,
        }
    }
}

/// Checks on one head, as `forge wait-checks` reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checks {
    Green,
    /// Red, pending, none, moved or unreadable: the reason.
    NotGreen(String),
}

/// The writes of a landing.
pub trait Landing {
    /// Checks on exactly `head`.
    fn checks(&self, number: u32, head: &str) -> Checks;
    /// Add and remove labels on the PR.
    fn relabel(&self, number: u32, add: &[&str], remove: &[&str]) -> Result<(), String>;
    /// Post a comment on the PR.
    fn comment(&self, number: u32, body: &str) -> Result<(), String>;
    /// Merge through the repo's `merge-pr.sh`, pinned to `head`.
    fn merge(&self, number: u32, head: &str) -> Result<(), String>;
}

/// The waiver record posted as the merge comment.
#[must_use]
pub fn record_comment(tag: &str, head: &str, record: &[(String, String)]) -> String {
    format!(
        "**Resync waiver (daemon): ELIGIBLE** against `{tag}` at head `{head}`.\n\n\
         Every changed kept-set file is byte-identical to its `defaults/` counterpart at the tag \
         (blob SHA), and checks are green on this head. The daemon merges this resync PR itself \
         through `merge-pr.sh`; no Judge or Champion decision applies to a byte-copy of a tag.\n\n\
         ```\n{}```\n",
        Verdict::record_lines(record)
    )
}

/// Run the proof and, if it holds with green checks on the same head, merge.
/// The caller labels `loom:review-requested` on anything but [`Waived::Landed`].
pub fn land(src: &dyn Source, landing: &dyn Landing, number: u32) -> Waived {
    let (tag, head, record) = match evaluate(src, number) {
        Verdict::Eligible { tag, head, record } => (tag, head, record),
        Verdict::NotEligible(why) => return Waived::Lane(format!("waiver proof failed: {why}")),
    };
    if let Checks::NotGreen(why) = landing.checks(number, &head) {
        return Waived::Lane(format!("checks not green on {head}: {why}"));
    }
    if let Err(e) = landing.comment(number, &record_comment(&tag, &head, &record)) {
        return Waived::Lane(format!("could not post the waiver record: {e}"));
    }
    if let Err(e) = landing.relabel(number, &["loom:pr"], &[]) {
        return Waived::Lane(format!("could not label loom:pr: {e}"));
    }
    match landing.merge(number, &head) {
        Ok(()) => Waived::Landed(format!("merged on the waiver proof against {tag}")),
        Err(e) => {
            // Put the PR back on the ordinary lane's label.
            let _ = landing.relabel(number, &[], &["loom:pr"]);
            Waived::Lane(format!("merge-pr.sh refused: {e}"))
        }
    }
}

#[cfg(test)]
mod glob_tests {
    use super::*;

    #[test]
    fn glob_matches_bash_star_semantics() {
        assert!(glob("*judge*", ".loom/roles/judge.md"));
        assert!(glob("*merge-pr.sh", ".loom/scripts/merge-pr.sh"));
        assert!(!glob("*merge-pr.sh", ".loom/scripts/merge-pr.sh.bak"));
        assert!(glob(".githooks/*", ".githooks/pre-push"));
        assert!(!glob(".githooks/*", "x/.githooks/pre-push"));
        assert!(glob("*a*b*", "xaxbx"));
        assert!(!glob("*ab", "b"));
        assert!(!glob("a*a", "a"));
    }

    #[test]
    fn upstream_map_follows_the_surface_map() {
        assert_eq!(
            upstream_path(".loom/hooks/guard-x.sh").as_deref(),
            Some("defaults/hooks/guard-x.sh")
        );
        assert_eq!(upstream_path(".loom/roles/judge.md"), None);
        assert_eq!(upstream_path(".claude/settings.json"), None);
    }
}

#[cfg(test)]
#[path = "../tests/workspace_resync_waiver.rs"]
mod tests;
