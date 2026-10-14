//! The locally resolved base repository and the fingerprint that bounds its
//! memo.
//!
//! Resolution is [`crate::write_scope::target::gh_target`] — the one port of
//! gh's own order (`GH_REPO` > `gh repo set-default` > `upstream` > `github` >
//! `origin`), shared with write scoping so a gh drift is fixed once. What this
//! file adds is (1) reading the remotes and the fingerprint from ONE
//! `git config --list --show-origin -z`, (2) the `ambiguous` marker for every
//! shape this port does not model exactly, and (3) [`ConfigFp`], which lets a
//! memo hit cost a few `stat`s instead of a fork.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::write_scope::target::{gh_target, nwo_from_repo_arg, parse_remote_config, Remote};

use super::{state, GhRepoEnv};

/// Deadline for one resolver `git` child.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// `(dev, inode, length, mtime in ns)` of one file — enough to see a
/// rewrite: git writes config through a lock file and a rename, so even a
/// same-length edit moves the inode and the mtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FileStamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime_ns: i128,
}

/// What a resolved base repo depends on: every config file git read (system,
/// global, includes, the repo's own `config` and `config.worktree`, present
/// or not) plus the effective `GH_REPO`. A memo is valid only while all of it
/// is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ConfigFp {
    files: Vec<(PathBuf, Option<FileStamp>)>,
    gh_repo: Option<String>,
}

impl ConfigFp {
    /// Re-stat every file (no fork) and compare.
    pub(crate) fn still_valid(&self, gh_repo: Option<&str>) -> bool {
        self.gh_repo.as_deref() == gh_repo && self.files.iter().all(|(p, s)| stamp(p) == *s)
    }
}

fn stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(FileStamp {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        })
    }
    #[cfg(not(unix))]
    {
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i128);
        Some(FileStamp {
            dev: 0,
            ino: 0,
            len: meta.len(),
            mtime_ns,
        })
    }
}

/// The repository `gh` would act on from a checkout, resolved locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaseRepo {
    /// The forge host (lowercased).
    pub(crate) host: String,
    /// `owner/repo` as configured (pre-redirect).
    pub(crate) nwo: String,
    /// Why this repo: `remote \`origin\``, `GH_REPO`, `gh repo set-default`.
    pub(crate) via: String,
    /// The local answer may differ from gh's: several remotes, a set-default
    /// pin, a non-`github.com` (alias or GHE) host, a `GH_REPO` that differs
    /// from origin, or a `url.*.insteadOf` rewrite. Such a root is
    /// cross-checked against gh before its fact is used.
    pub(crate) ambiguous: bool,
    /// `origin`'s `owner/repo`, when there is an origin.
    pub(crate) origin_nwo: Option<String>,
    pub(crate) fp: ConfigFp,
}

/// One `git config` entry: `(origin, key, value)`.
type Entry = (String, String, String);

/// A checkout's config entries and their fingerprint.
pub(super) struct Snapshot {
    entries: Vec<Entry>,
    pub(super) fp: ConfigFp,
}

/// Run one resolver `git` child in `root`; its stdout on success.
pub(super) fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    state::count_git_fork();
    let mut cmd = std::process::Command::new("git");
    cmd.args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null());
    for (k, v) in state::git_env() {
        cmd.env(k, v);
    }
    match crate::cmd_out::run_command(cmd, GIT_TIMEOUT) {
        crate::cmd_out::CmdOutcome::Ran(o) if o.status.success() => Some(o.stdout),
        _ => None,
    }
}

/// Parse `git config --list --show-origin -z`: NUL-terminated pairs of
/// `origin` and `key\nvalue` (a valueless key has no newline).
pub(super) fn parse_config_z(raw: &[u8]) -> Vec<Entry> {
    let text = String::from_utf8_lossy(raw);
    let mut tokens = text.split('\0');
    let mut out = Vec::new();
    while let (Some(origin), Some(kv)) = (tokens.next(), tokens.next()) {
        if origin.is_empty() && kv.is_empty() {
            break;
        }
        let (key, value) = kv.split_once('\n').unwrap_or((kv, ""));
        out.push((origin.to_string(), key.to_string(), value.to_string()));
    }
    out
}

/// Resolve `p` (as git printed it, relative to `root` when not absolute).
fn absolute(root: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// The file an `include.path` / `includeIf.*.path` value names: `~/` is the
/// home directory, a relative path is relative to the including file.
fn include_target(including: &Path, value: &str) -> Option<PathBuf> {
    if let Some(rest) = value.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest));
    }
    let p = Path::new(value);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        including.parent().map(|d| d.join(p))
    }
}

/// Read `root`'s config entries and fingerprint (two forks). `None` when
/// `root` is not a checkout.
pub(super) fn snapshot(root: &Path, gh_repo: Option<String>) -> Option<Snapshot> {
    let raw = git(root, &["config", "--list", "--show-origin", "-z"])?;
    let entries = parse_config_z(&raw);
    let dirs = git(root, &["rev-parse", "--git-common-dir", "--git-dir"])?;
    let dirs = String::from_utf8_lossy(&dirs);
    let mut lines = dirs.lines().map(str::trim).filter(|l| !l.is_empty());
    let (common, own) = (lines.next()?, lines.next()?);
    let mut files: BTreeSet<PathBuf> = BTreeSet::new();
    files.insert(absolute(root, common).join("config"));
    files.insert(absolute(root, own).join("config.worktree"));
    for (origin, key, value) in &entries {
        let Some(path) = origin.strip_prefix("file:") else {
            continue;
        };
        let file = absolute(root, path);
        let is_include =
            key == "include.path" || (key.starts_with("includeif.") && key.ends_with(".path"));
        if is_include {
            if let Some(target) = include_target(&file, value) {
                files.insert(target);
            }
        }
        files.insert(file);
    }
    let files = files.into_iter().map(|p| {
        let s = stamp(&p);
        (p, s)
    });
    Some(Snapshot {
        entries,
        fp: ConfigFp {
            files: files.collect(),
            gh_repo,
        },
    })
}

/// The host of the repo `gh` picked: the `HOST/` of a three-segment
/// `GH_REPO`, else the remote naming that repo, else origin's, else
/// `github.com`.
fn host_for(nwo: &str, remotes: &[Remote], gh_repo: Option<&str>, via_env: bool) -> String {
    if via_env {
        let segs: Vec<&str> = gh_repo
            .unwrap_or("")
            .trim()
            .trim_matches('/')
            .split('/')
            .collect();
        if let [host, _, _] = segs.as_slice() {
            return host.to_ascii_lowercase();
        }
    }
    remotes
        .iter()
        .find(|r| r.nwo.eq_ignore_ascii_case(nwo))
        .or_else(|| remotes.iter().find(|r| r.name == "origin"))
        .map_or_else(|| "github.com".to_string(), |r| r.host.clone())
}

/// Resolve the base repo from a snapshot.
pub(super) fn resolve(snap: &Snapshot, gh_repo: Option<&str>) -> Option<BaseRepo> {
    let remote_text: String = snap
        .entries
        .iter()
        .filter(|(_, k, _)| k.starts_with("remote."))
        .map(|(_, k, v)| format!("{k} {v}\n"))
        .collect();
    let remotes = parse_remote_config(&remote_text);
    let target = gh_target(&remotes, gh_repo)?;
    let origin_nwo = remotes
        .iter()
        .find(|r| r.name == "origin")
        .map(|r| r.nwo.clone());
    let remote_names: BTreeSet<&str> = snap
        .entries
        .iter()
        .filter_map(|(_, k, _)| k.strip_prefix("remote.")?.strip_suffix(".url"))
        .collect();
    let pinned = snap
        .entries
        .iter()
        .any(|(_, k, _)| k.starts_with("remote.") && k.ends_with(".gh-resolved"));
    let rewrites = snap.entries.iter().any(|(_, k, _)| {
        k.starts_with("url.") && (k.ends_with(".insteadof") || k.ends_with(".pushinsteadof"))
    });
    let foreign_host = remotes.iter().any(|r| r.host != "github.com");
    let env_nwo = gh_repo
        .filter(|v| !v.trim().is_empty())
        .and_then(nwo_from_repo_arg);
    let env_differs = env_nwo.as_deref().is_some_and(|e| {
        origin_nwo
            .as_deref()
            .is_none_or(|o| !o.eq_ignore_ascii_case(e))
    });
    let via_env = env_nwo.is_some();
    Some(BaseRepo {
        host: host_for(&target.nwo, &remotes, gh_repo, via_env),
        nwo: target.nwo,
        via: target.via,
        ambiguous: remote_names.len() > 1 || pinned || rewrites || foreign_host || env_differs,
        origin_nwo,
        fp: snap.fp.clone(),
    })
}

/// The base repo `gh` resolves from `root` under `env`, memoised per
/// `(root, env)` until its fingerprint changes. A hit forks nothing.
pub(crate) fn base_repo(root: &Path, env: GhRepoEnv) -> Option<BaseRepo> {
    let gh_repo = env.effective_gh_repo();
    let key = (root.to_path_buf(), env);
    let hit = state::with(|s| s.base_memo.get(&key).cloned());
    if let Some((fp, base)) = hit {
        if fp.still_valid(gh_repo.as_deref()) {
            return base;
        }
    }
    let snap = snapshot(root, gh_repo.clone())?;
    let base = resolve(&snap, gh_repo.as_deref());
    state::with(|s| s.base_memo.insert(key, (snap.fp.clone(), base.clone())));
    base
}

/// `origin`'s `(host, owner/repo)` as `git remote get-url origin` reports it
/// (insteadOf applied), memoised per root until the fingerprint changes.
/// Only successes are memoised, as before.
pub(crate) fn origin_identity(root: &Path) -> Option<(String, String)> {
    let key = root.to_path_buf();
    if let Some((fp, v)) = state::with(|s| s.origin_memo.get(&key).cloned()) {
        if fp.still_valid(None) {
            return Some(v);
        }
    }
    let snap = snapshot(root, None)?;
    let url = git(root, &["remote", "get-url", "origin"])?;
    let resolved = crate::forge_etag_store::parse_remote_url(&String::from_utf8_lossy(&url))?;
    state::with(|s| s.origin_memo.insert(key, (snap.fp, resolved.clone())));
    Some(resolved)
}
