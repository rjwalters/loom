//! Which repository a `gh` call made from a checkout actually lands on
//! (#9548), computed locally from the checkout's git config with no forge call.
//!
//! `gh` does not simply use `origin`. For every porcelain command without
//! `--repo`, for the `{owner}/{repo}` placeholders of `gh api`, and for
//! `gh repo view`, it picks a *base repository*:
//!
//! 1. the `GH_REPO` environment variable, when set (not honoured by
//!    `gh repo view`, which takes a positional argument instead);
//! 2. otherwise a `gh repo set-default` pin, stored as
//!    `remote.<name>.gh-resolved` (`base`, or an explicit `owner/repo`);
//! 3. otherwise the remotes ranked by name: `upstream` > `github` > `origin` >
//!    anything else.
//!
//! Step 3 is the hazard: a fork checkout (`origin` = the fork, `upstream` =
//! the project it was forked from) sends every unpinned call to the upstream
//! project. Reproduced with gh 2.101 on a scratch repository carrying both
//! remotes: `gh repo view`, `gh api repos/{owner}/{repo}` and `gh issue list`
//! all answered for `upstream`, never `origin`.

/// One configured remote, as far as write scoping needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Remote {
    pub(crate) name: String,
    /// Lowercased host (`github.com`, an ssh alias, a GHE host).
    pub(crate) host: String,
    /// `owner/repo` as spelled in the URL.
    pub(crate) nwo: String,
    /// The `remote.<name>.gh-resolved` value, if `gh repo set-default` set one.
    pub(crate) gh_resolved: Option<String>,
}

/// Parse a git remote URL into `(host, owner/repo)`.
///
/// Accepts scp-style (`git@host:owner/repo.git`), `ssh://[user@]host[:port]/
/// owner/repo`, `https://[user@]host/owner/repo` (also `http`, `git`). The
/// first two path segments are the repository; anything else is `None`.
pub(crate) fn parse_remote_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git" | "git+ssh") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?;
        let host = host.split(':').next()?;
        (host, path)
    } else {
        // scp-like: [user@]host:path — but never a local path.
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') || path.starts_with("//") {
            return None;
        }
        (authority.rsplit('@').next()?, path)
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    let (owner, repo) = (segs.next()?, segs.next()?);
    if host.is_empty() || segs.next().is_some() {
        return None;
    }
    Some((host.to_ascii_lowercase(), format!("{owner}/{repo}")))
}

/// Parse `git config --get-regexp '^remote\.'` output into remotes, in
/// config order. Remotes without a parseable URL are dropped (gh ignores them
/// too).
pub(crate) fn parse_remote_config(text: &str) -> Vec<Remote> {
    let mut urls: Vec<(String, String)> = Vec::new();
    let mut pins: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let Some(rest) = key.strip_prefix("remote.") else {
            continue;
        };
        let Some((name, var)) = rest.rsplit_once('.') else {
            continue;
        };
        match var {
            "url" if !urls.iter().any(|(n, _)| n == name) => {
                urls.push((name.to_string(), value.trim().to_string()));
            }
            "gh-resolved" => pins.push((name.to_string(), value.trim().to_string())),
            _ => {}
        }
    }
    urls.into_iter()
        .filter_map(|(name, url)| {
            let (host, nwo) = parse_remote_url(&url)?;
            let gh_resolved = pins
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone());
            Some(Remote {
                name,
                host,
                nwo,
                gh_resolved,
            })
        })
        .collect()
}

/// The repository `gh` will act on from a checkout, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GhTarget {
    pub(crate) nwo: String,
    /// Human-readable source: `GH_REPO`, `gh repo set-default`, or `remote
    /// \`upstream\``.
    pub(crate) via: String,
}

fn rank(name: &str) -> u8 {
    match name.to_ascii_lowercase().as_str() {
        "upstream" => 3,
        "github" => 2,
        "origin" => 1,
        _ => 0,
    }
}

/// `owner/repo` from a `GH_REPO`-style value (`[HOST/]OWNER/REPO`).
pub(crate) fn nwo_from_repo_arg(value: &str) -> Option<String> {
    let segs: Vec<&str> = value.trim().trim_matches('/').split('/').collect();
    match segs.as_slice() {
        [o, r] | [_, o, r] if !o.is_empty() && !r.is_empty() => Some(format!("{o}/{r}")),
        _ => None,
    }
}

/// Resolve the base repository the way `gh` does (see the module docs).
/// `None` when the checkout has no usable remote and no `GH_REPO`.
pub(crate) fn gh_target(remotes: &[Remote], gh_repo_env: Option<&str>) -> Option<GhTarget> {
    if let Some(nwo) = gh_repo_env
        .filter(|v| !v.trim().is_empty())
        .and_then(nwo_from_repo_arg)
    {
        return Some(GhTarget {
            nwo,
            via: "the GH_REPO environment variable".into(),
        });
    }
    for r in remotes {
        match r.gh_resolved.as_deref() {
            Some("base") => {
                return Some(GhTarget {
                    nwo: r.nwo.clone(),
                    via: format!("`gh repo set-default` (remote `{}`)", r.name),
                })
            }
            Some(v) if v.contains('/') => {
                if let Some(nwo) = nwo_from_repo_arg(v) {
                    return Some(GhTarget {
                        nwo,
                        via: "`gh repo set-default`".into(),
                    });
                }
            }
            _ => {}
        }
    }
    // gh only matches remotes on a host it serves; approximate that with
    // origin's host, which is the host this checkout's Loom works against.
    let host = remotes
        .iter()
        .find(|r| r.name == "origin")
        .map(|r| r.host.as_str());
    let best = remotes
        .iter()
        .filter(|r| host.is_none_or(|h| r.host == h))
        .fold(None::<&Remote>, |best, r| match best {
            Some(b) if rank(&b.name) >= rank(&r.name) => Some(b),
            _ => Some(r),
        })?;
    Some(GhTarget {
        nwo: best.nwo.clone(),
        via: format!("remote `{}`", best.name),
    })
}

/// `origin`'s `owner/repo`, if the checkout has one.
pub(crate) fn origin_nwo(remotes: &[Remote]) -> Option<String> {
    remotes
        .iter()
        .find(|r| r.name == "origin")
        .map(|r| r.nwo.clone())
}

/// Read a checkout's remotes (`git config --get-regexp '^remote\.'`). A
/// checkout with no remotes, or a directory that is not a checkout, is empty.
pub(crate) fn read_remotes(root: &std::path::Path) -> Vec<Remote> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["config", "--get-regexp", r"^remote\."])
        .current_dir(root)
        .stdin(std::process::Stdio::null());
    match crate::cmd_out::run_command(cmd, std::time::Duration::from_secs(10)) {
        crate::cmd_out::CmdOutcome::Ran(o) if o.status.success() => {
            parse_remote_config(&String::from_utf8_lossy(&o.stdout))
        }
        _ => Vec::new(),
    }
}
