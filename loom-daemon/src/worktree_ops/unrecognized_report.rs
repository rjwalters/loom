//! Report-only visibility for unfamiliar directories under the worktree root
//! (issue #11194, part of #11189).
//!
//! Automatic cleanup only classifies `issue-<N>` and `pr-<N>` directories.
//! Anything else (for example a hand-made `judge-123` review checkout) is
//! invisible to every pass, so its disk footprint is never surfaced. This
//! module lists each such directory individually with its apparent size and,
//! where the name strictly matches `judge-(pr-)?<N>(-suffix)?`, the state of
//! the *candidate* PR `N`.
//!
//! This is **report-only**. It never removes, adopts, or writes into a
//! directory (no `.loom-managed` creation), never follows symlinks, and its
//! output feeds no cleanup decision: a name never establishes ownership and
//! a failed probe is simply "unknown". Removal of legacy directories needs a
//! separate, explicit ownership decision.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::clean::PrStatus;
use super::naming;

/// State of a candidate PR as shown in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrState {
    Open,
    Merged,
    ClosedUnmerged,
    Unknown,
}

impl PrState {
    fn label(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Merged => "merged",
            Self::ClosedUnmerged => "closed-unmerged",
            Self::Unknown => "unknown",
        }
    }

    fn from_status(status: &PrStatus) -> Self {
        match status {
            PrStatus::Open => Self::Open,
            PrStatus::Merged { .. } => Self::Merged,
            PrStatus::ClosedNoMerge { .. } => Self::ClosedUnmerged,
            PrStatus::NoPr | PrStatus::Unknown => Self::Unknown,
        }
    }
}

/// One unfamiliar entry under the worktree root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_symlink: bool,
    /// Apparent bytes; `None` = unavailable (probe failed, or symlink).
    pub bytes: Option<u64>,
    /// Candidate PR number inferred from the name (never ownership).
    pub candidate_pr: Option<u32>,
    /// State of the candidate PR; `None` when there is no candidate.
    pub pr_state: Option<PrState>,
}

/// Candidate PR number from a strict `judge-(pr-)?<N>(-suffix)?` name, with
/// `N` a positive decimal number. Anything else yields `None` and needs no
/// forge lookup.
#[must_use]
pub(crate) fn candidate_pr(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("judge-")?;
    let rest = rest.strip_prefix("pr-").unwrap_or(rest);
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (digits, tail) = rest.split_at(digits_end);
    if digits.is_empty() {
        return None;
    }
    if !tail.is_empty() && !(tail.starts_with('-') && tail.len() > 1) {
        return None;
    }
    digits.parse::<u32>().ok().filter(|n| *n > 0)
}

/// Immediate children of `root` that are neither `issue-<N>` nor `pr-<N>`.
/// Real directories and symlinks are listed; plain files are ignored.
/// Nothing is followed or opened beyond `symlink_metadata`-style file types.
fn discover(root: &Path) -> Vec<(String, PathBuf, bool)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf, bool)> = entries
        .flatten()
        .filter_map(|e| {
            let ft = e.file_type().ok()?; // does not follow symlinks
            let is_symlink = ft.is_symlink();
            if !(ft.is_dir() || is_symlink) {
                return None;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if naming::issue_from_worktree(&name).is_some()
                || naming::pr_from_worktree(&name).is_some()
            {
                return None;
            }
            Some((name, e.path(), is_symlink))
        })
        .collect();
    out.sort();
    out
}

/// Build the report with injected probes. `pr_probe` is invoked at most once
/// per distinct candidate number within the pass.
pub(crate) fn build_report(
    root: &Path,
    size_probe: &dyn Fn(&Path) -> Option<u64>,
    pr_probe: &dyn Fn(u32) -> PrState,
) -> Vec<Entry> {
    let mut cache: HashMap<u32, PrState> = HashMap::new();
    discover(root)
        .into_iter()
        .map(|(name, path, is_symlink)| {
            let candidate = if is_symlink {
                None
            } else {
                candidate_pr(&name)
            };
            let pr_state = candidate.map(|n| *cache.entry(n).or_insert_with(|| pr_probe(n)));
            let bytes = if is_symlink { None } else { size_probe(&path) };
            Entry {
                name,
                path,
                is_symlink,
                bytes,
                candidate_pr: candidate,
                pr_state,
            }
        })
        .collect()
}

/// Render the report lines (no trailing newline).
#[must_use]
pub(crate) fn render(entries: &[Entry]) -> Vec<String> {
    if entries.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "Unrecognized directories under the worktree root ({}) - report only",
        entries.len()
    )];
    for e in entries {
        let size = e
            .bytes
            .map_or_else(|| "unavailable".to_string(), |b| format!("{b} bytes"));
        let pr = match (e.candidate_pr, e.pr_state) {
            (Some(n), Some(s)) => format!("candidate PR #{n}: {}", s.label()),
            _ => "no candidate PR".to_string(),
        };
        let kind = if e.is_symlink {
            "symlink (not followed)"
        } else {
            "directory"
        };
        lines.push(format!(
            "  {} [{kind}] size: {size}; {pr}; PRESERVED (report-only, ownership not established)",
            e.path.display()
        ));
    }
    lines
}

/// Print the report for `worktrees_dir` using production probes. Called from
/// the standard clean worktree pass, including `--dry-run`.
pub(crate) fn report(repo_root: &Path, worktrees_dir: &Path) {
    let entries = build_report(worktrees_dir, &crate::worktree_disk_status::dir_size_bytes, &|n| {
        PrState::from_status(&super::clean::check_pr_status_by_number_rest(repo_root, n))
    });
    for line in render(&entries) {
        println!("{line}");
    }
    if !entries.is_empty() {
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn candidate_parsing_is_strict() {
        assert_eq!(candidate_pr("judge-123"), Some(123));
        assert_eq!(candidate_pr("judge-pr-123"), Some(123));
        assert_eq!(candidate_pr("judge-123-abc"), Some(123));
        assert_eq!(candidate_pr("judge-pr-9-current"), Some(9));
        for bad in [
            "judge-",
            "judge-0",
            "judge-pr-",
            "judge-abc",
            "judge-12x",
            "judge-123-",
            "judge-pr-0-x",
            "judge-99999999999",
            "xjudge-1",
            "review-5",
            "judge--5",
        ] {
            assert_eq!(candidate_pr(bad), None, "{bad}");
        }
    }

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for d in [
            "judge-123",
            "judge-123-abcd",
            "judge-pr-123",
            "judge-bad",
            "scratch",
            "issue-5",
            "pr-7",
            "judge-9",
        ] {
            std::fs::create_dir(tmp.path().join(d)).unwrap();
            std::fs::write(tmp.path().join(d).join("f"), b"hello").unwrap();
        }
        std::fs::write(tmp.path().join("plain-file"), b"x").unwrap();
        tmp
    }

    #[test]
    fn lists_unfamiliar_dirs_individually_and_dedupes_probes() {
        let tmp = fixture();
        let calls = RefCell::new(Vec::new());
        let entries = build_report(tmp.path(), &|_| Some(5), &|n| {
            calls.borrow_mut().push(n);
            if n == 123 {
                PrState::Merged
            } else {
                PrState::Open
            }
        });
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "judge-123",
                "judge-123-abcd",
                "judge-9",
                "judge-bad",
                "judge-pr-123",
                "scratch"
            ]
        );
        let mut c = calls.into_inner();
        c.sort_unstable();
        assert_eq!(c, vec![9, 123], "one lookup per distinct candidate");
        let bad = entries.iter().find(|e| e.name == "judge-bad").unwrap();
        assert_eq!((bad.candidate_pr, bad.pr_state), (None, None));
        let out = render(&entries).join("\n");
        assert!(out.contains("candidate PR #123: merged"));
        assert!(out.contains("PRESERVED"));
        assert!(!out.contains("issue-5") && !out.contains("pr-7"));
    }

    #[test]
    fn failed_probes_are_unknown_or_unavailable() {
        let tmp = fixture();
        let entries = build_report(tmp.path(), &|_| None, &|_| PrState::Unknown);
        let e = entries.iter().find(|e| e.name == "judge-123").unwrap();
        assert_eq!(e.bytes, None);
        assert_eq!(e.pr_state, Some(PrState::Unknown));
        let out = render(&entries).join("\n");
        assert!(out.contains("size: unavailable"));
        assert!(out.contains("candidate PR #123: unknown"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_or_probed() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::write(target.path().join("big"), vec![0u8; 4096]).unwrap();
        std::os::unix::fs::symlink(target.path(), tmp.path().join("judge-55")).unwrap();
        let sized = RefCell::new(0);
        let probed = RefCell::new(0);
        let entries = build_report(
            tmp.path(),
            &|_| {
                *sized.borrow_mut() += 1;
                Some(1)
            },
            &|_| {
                *probed.borrow_mut() += 1;
                PrState::Open
            },
        );
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_symlink);
        assert_eq!((entries[0].bytes, entries[0].candidate_pr), (None, None));
        assert_eq!((*sized.borrow(), *probed.borrow()), (0, 0));
        assert!(target.path().join("big").exists());
    }

    #[test]
    fn reporting_changes_nothing_on_disk() {
        let tmp = fixture();
        let snapshot = |p: &Path| {
            let mut v: Vec<_> = walk(p);
            v.sort();
            v
        };
        fn walk(p: &Path) -> Vec<String> {
            let mut out = Vec::new();
            for e in std::fs::read_dir(p).unwrap().flatten() {
                out.push(e.path().display().to_string());
                if e.file_type().unwrap().is_dir() {
                    out.extend(walk(&e.path()));
                }
            }
            out
        }
        let before = snapshot(tmp.path());
        let entries = build_report(tmp.path(), &|_| Some(1), &|_| PrState::Merged);
        let _ = render(&entries);
        assert_eq!(before, snapshot(tmp.path()));
        assert!(!tmp.path().join("judge-123/.loom-managed").exists());
    }

    #[test]
    fn missing_root_yields_empty_report() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = build_report(&tmp.path().join("nope"), &|_| None, &|_| PrState::Unknown);
        assert!(entries.is_empty() && render(&entries).is_empty());
    }
}
