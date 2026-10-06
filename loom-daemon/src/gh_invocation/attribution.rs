//! W1 bucket attribution for the facade's accounting rows: which credential
//! a call spent, where its repository came from, and how many requests it
//! stood for. Everything here is local: path shapes, memoised `git` reads
//! of a checkout's remotes, and the call's own argv and stdout — never a
//! forge call, a token or a header body.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::forge_bucket_book::{self, DirClass};
use crate::gh_invocation::GhInvocation;

/// The credential a call ran under, as booked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredAttr {
    /// `app-<id>`, `app-unknown`, `env-token` or `ambient`.
    pub account: String,
    /// The owner the App installation covers (lowercased), when known.
    pub owner: Option<String>,
    /// `reader`, `writer`, `env` or `ambient`.
    pub kind: &'static str,
}

impl CredAttr {
    fn ambient() -> Self {
        Self {
            account: "ambient".to_string(),
            owner: None,
            kind: "ambient",
        }
    }

    /// Whether the call spent a known App installation's bucket.
    #[must_use]
    pub fn is_app(&self) -> bool {
        matches!(self.kind, "reader" | "writer") && self.owner.is_some()
    }
}

/// The credential `inv` runs under (see [`cred_of_with`]).
#[must_use]
pub fn cred_of(inv: &GhInvocation) -> CredAttr {
    let dir = inv
        .env_plan(None)
        .into_iter()
        .find(|e| e.key == "GH_CONFIG_DIR")
        .and_then(|e| e.value)
        .or_else(|| std::env::var_os("GH_CONFIG_DIR"))
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    let env_token = !inv.strip_token_env
        && ["GH_TOKEN", "GITHUB_TOKEN"]
            .iter()
            .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
    cred_of_with(dir.as_deref(), env_token)
}

/// Pure core of [`cred_of`]: the effective `GH_CONFIG_DIR` and whether an
/// env token reaches the child (`gh` prefers one over the directory).
///
/// - env token ⇒ `env-token` / `env`;
/// - `…/.loom/gh-config-by-owner/<owner>/<digits>` ⇒ `app-<digits>`, reader;
/// - `…/.loom/gh-config-by-owner/<owner>` ⇒ the writer App, that owner;
/// - `…/.loom/gh-config` ⇒ the writer App, the workspace's own owner;
/// - anything else, or none ⇒ `ambient`.
#[must_use]
pub fn cred_of_with(dir: Option<&Path>, env_token: bool) -> CredAttr {
    if env_token {
        return CredAttr {
            account: "env-token".to_string(),
            owner: None,
            kind: "env",
        };
    }
    let Some(dir) = dir else {
        return CredAttr::ambient();
    };
    match forge_bucket_book::classify_dir(dir) {
        DirClass::Reader { owner, app_id } => {
            let label = crate::observability::ops::ratelimit::app_account_label(&app_id);
            CredAttr {
                account: if label == "unknown" {
                    "app-unknown".to_string()
                } else {
                    label
                },
                owner: Some(owner),
                kind: "reader",
            }
        }
        DirClass::OwnerWriter { root, owner } => CredAttr {
            account: forge_bucket_book::writer_account(&root),
            owner: Some(owner),
            kind: "writer",
        },
        DirClass::PrimaryWriter { root } => CredAttr {
            account: forge_bucket_book::writer_account(&root),
            owner: forge_bucket_book::primary_owner(&root),
            kind: "writer",
        },
        DirClass::Other => CredAttr::ambient(),
    }
}

/// Where a row's `owner/repo` came from (`ro`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoOrigin {
    /// The site set it ([`GhInvocation::identity_scope`]).
    Site,
    /// The typed [`super::super::GhTarget`].
    Target,
    /// The machine-global `LOOM_REPO`.
    LoomRepo,
    /// The working directory's `origin` remote.
    Remote,
    /// Not resolved.
    None,
}

impl RepoOrigin {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Site => "site",
            Self::Target => "target",
            Self::LoomRepo => "loom_repo",
            Self::Remote => "remote",
            Self::None => "none",
        }
    }
}

/// How long a checkout whose `origin` did not resolve is left alone before
/// it is read again (a resolved one is memoised for the process lifetime).
const UNRESOLVED_RETRY: Duration = Duration::from_secs(300);

/// `cwd`'s `origin` remote as `owner/repo`, memoised: a resolved remote by
/// [`crate::forge_etag_store::remote_identity`], an unresolved one here for
/// [`UNRESOLVED_RETRY`], so a non-git working directory costs one local
/// `git` read per interval rather than one per call. Never a forge call.
#[must_use]
pub fn remote_repo(cwd: &Path) -> Option<String> {
    static UNRESOLVED: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
    let unresolved = UNRESOLVED.get_or_init(|| Mutex::new(HashMap::new()));
    let recently_failed = unresolved
        .lock()
        .ok()
        .and_then(|m| m.get(cwd).copied())
        .is_some_and(|at| at.elapsed() < UNRESOLVED_RETRY);
    if recently_failed {
        return None;
    }
    match crate::forge_etag_store::remote_identity(cwd) {
        Some((_, nwo)) => Some(nwo),
        None => {
            if let Ok(mut m) = unresolved.lock() {
                m.insert(cwd.to_path_buf(), Instant::now());
            }
            None
        }
    }
}

/// Whether the repo `gh` itself would resolve in `cwd` — `GH_REPO`, then a
/// `gh repo set-default` pin, then its remote order — differs from
/// `remote_nwo` (the `origin` remote a row was attributed to). Memoised per
/// `(cwd, GH_REPO)`; a checkout `gh` cannot resolve never disagrees.
#[must_use]
pub fn cwd_route_disagrees(cwd: &Path, remote_nwo: &str) -> bool {
    type Memo = HashMap<(PathBuf, Option<String>), Option<String>>;
    static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
    let gh_repo = ["GH_REPO", "LOOM_REPO"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()));
    let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (cwd.to_path_buf(), gh_repo.clone());
    let cached = memo.lock().ok().and_then(|m| m.get(&key).cloned());
    let gh_nwo = match cached {
        Some(v) => v,
        None => {
            let remotes = crate::write_scope::target::read_remotes(cwd);
            let resolved =
                crate::write_scope::target::gh_target(&remotes, gh_repo.as_deref()).map(|t| t.nwo);
            if let Ok(mut m) = memo.lock() {
                m.insert(key, resolved.clone());
            }
            resolved
        }
    };
    gh_nwo.is_some_and(|g| !g.eq_ignore_ascii_case(remote_nwo))
}

/// Whether `line` is an HTTP status line (`^HTTP/[0-9.]+ [0-9]{3}`).
fn is_status_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("HTTP/") else {
        return false;
    };
    let version_len = rest
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b'.')
        .count();
    if version_len == 0 {
        return false;
    }
    let Some(status) = rest[version_len..].strip_prefix(' ') else {
        return false;
    };
    status.len() >= 3 && status.as_bytes()[..3].iter().all(u8::is_ascii_digit)
}

/// `(pg, pu)` for one call: the request count when the call is known to
/// stand for more than one, and whether that count is unknown.
///
/// - `--paginate` with `--include`: one HTTP status block per page.
/// - `--paginate` without `--include`: unknown (`pu`), counted as one.
/// - `run download`: at least 2 (the artifact lookup and its redirect).
#[must_use]
pub fn pages(
    args: &[OsString],
    include: bool,
    stdout: Option<&[u8]>,
) -> (Option<u32>, Option<bool>) {
    let first_two: Vec<&str> = args.iter().take(2).filter_map(|a| a.to_str()).collect();
    if first_two == ["run", "download"] {
        return (Some(2), None);
    }
    if !args.iter().any(|a| a == "--paginate") {
        return (None, None);
    }
    if !include {
        return (None, Some(true));
    }
    let count = stdout.map_or(0, |out| {
        String::from_utf8_lossy(out)
            .lines()
            .filter(|l| is_status_line(l))
            .count()
    });
    (u32::try_from(count).ok().filter(|n| *n > 0), None)
}
