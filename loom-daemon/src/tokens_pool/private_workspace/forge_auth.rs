//! The forge credential one private-session `docker exec` carries (#13916).
//!
//! The worker's git credential helper and its `gh` read `GH_TOKEN` from the
//! exec's own environment (`repository::credential`). Until #13916 that token
//! existed only when the daemon's own process had one, which an App-only
//! worker never does: its credential is a GitHub App installation token
//! published into a `GH_CONFIG_DIR`, so every private clone fetch failed.
//!
//! [`ExecAuth::resolve`] closes that gap. When the process carries neither
//! `GH_TOKEN` nor `GITHUB_TOKEN` and the target repository is on GitHub, it
//! reads the installation token for the repository's OWNER (`gh auth token`
//! under that owner's App `GH_CONFIG_DIR`, through the one `gh` choke point)
//! and hands it to the exec. The value is set only on the `docker` client
//! child's environment and named in argv as a bare `--env GH_TOKEN`, so it
//! never appears in argv, on disk, in a log, in captured output or on the
//! container. It is resolved per exec, never cached per container:
//! installation tokens last about an hour, so each prepare and each job gets
//! a fresh one.
use super::FORGE_ENV;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The variable a resolved token is exported as.
pub(super) const TOKEN_ENV: &str = "GH_TOKEN";

/// Bound on the local `gh auth token` read (no network request is sent).
const TOKEN_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// What one exec forwards: the forge variable names for argv, and the values
/// this host resolved itself, for the `docker` child's environment only.
///
/// Deliberately not `Debug`/`Clone`/`Serialize`: the value must not be
/// printable or copied into state by accident.
pub(super) struct ExecAuth {
    names: Vec<&'static str>,
    secret: Option<String>,
}

impl ExecAuth {
    /// The credential for an exec acting on `repository` from `workspace`.
    pub(super) fn resolve(workspace: &Path, repository: &str) -> Self {
        Self::resolve_with_root(
            workspace,
            repository,
            &|name| std::env::var_os(name),
            &|dir| read_token(dir, None),
            crate::credential_preflight::primary_workspace_root().as_deref(),
        )
    }

    /// [`Self::resolve`] over an injected process environment and token
    /// reader (tests). The daemon's registered root is read from the
    /// process-global registry, exactly as production does.
    #[cfg(test)]
    pub(super) fn resolve_with(
        workspace: &Path,
        repository: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
        read: &dyn Fn(&Path) -> Option<String>,
    ) -> Self {
        Self::resolve_with_root(
            workspace,
            repository,
            env,
            read,
            crate::credential_preflight::primary_workspace_root().as_deref(),
        )
    }

    /// [`Self::resolve_with`] with the registered primary root injected too,
    /// so a test can pin the pristine-dispatch state — no
    /// `LOOM_MACHINE_CHECKOUT` in the process env, the daemon's registered
    /// root the only source (#13965) — without touching the set-once global.
    fn resolve_with_root(
        workspace: &Path,
        repository: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
        read: &dyn Fn(&Path) -> Option<String>,
        registered: Option<&Path>,
    ) -> Self {
        let mut names: Vec<&'static str> = FORGE_ENV
            .iter()
            .copied()
            .filter(|name| env(name).is_some())
            .collect();
        let has_token = ["GH_TOKEN", "GITHUB_TOKEN"]
            .iter()
            .any(|name| env(name).is_some_and(|v| !v.is_empty()));
        if has_token {
            return Self {
                names,
                secret: None,
            };
        }
        let gh_host = env("GH_HOST")
            .and_then(|v| v.into_string().ok())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "github.com".into());
        let Some(slug) = github_slug(repository, &gh_host) else {
            return Self {
                names,
                secret: None,
            };
        };
        // No directory proven to hold THIS owner's installation token: read
        // nothing. The process `GH_CONFIG_DIR` is the workspace-root owner's
        // credential on a daemon host, and handing it to another owner's
        // repository would forward the wrong installation's token.
        let primary = primary_root(env, registered);
        let secret = config_dir_for(workspace, primary.as_deref(), &slug, &gh_host)
            .and_then(|dir| read(&dir));
        if secret.is_some() && !names.contains(&TOKEN_ENV) {
            names.push(TOKEN_ENV);
        }
        Self { names, secret }
    }

    /// The variable names to pass as bare `--env NAME` arguments.
    pub(super) fn names(&self) -> &[&'static str] {
        &self.names
    }

    /// The values to set on the `docker` client child (never argv).
    pub(super) fn child_env(&self) -> Vec<(String, String)> {
        self.secret
            .iter()
            .map(|value| (TOKEN_ENV.to_owned(), value.clone()))
            .collect()
    }

    /// `true` when this host resolved a token for the exec itself.
    #[cfg(test)]
    pub(super) fn resolved(&self) -> bool {
        self.secret.is_some()
    }
}

/// A workspace whose `.loom/gh-config-by-owner/<owner>` holds a published
/// `hosts.yml`, so [`ExecAuth::resolve_with`] has a directory matched to
/// `owner` (tests).
#[cfg(test)]
pub(super) fn published_owner_workspace(owner: &str) -> tempfile::TempDir {
    let ws = tempfile::tempdir().expect("tempdir");
    let dir = crate::credential_preflight::github_app_gh_config_dir_for_owner(ws.path(), owner);
    std::fs::create_dir_all(&dir).expect("owner dir");
    std::fs::write(dir.join("hosts.yml"), "github.com: {}\n").expect("hosts.yml");
    ws
}

/// The `owner/repo` of a GitHub repository URL, or `None` for any other forge
/// (a Gitea under a sub-path, a different host).
pub(super) fn github_slug(repository: &str, gh_host: &str) -> Option<String> {
    let url = reqwest::Url::parse(repository).ok()?;
    if url.scheme() != "https" || url.host_str()? != gh_host || url.port().is_some() {
        return None;
    }
    let segments: Vec<&str> = url.path().trim_matches('/').split('/').collect();
    let [owner, name] = segments[..] else {
        return None;
    };
    let name = name.strip_suffix(".git").unwrap_or(name);
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// The daemon's primary workspace root, where the refresh tick publishes every
/// owner's App directory: `LOOM_MACHINE_CHECKOUT` when the process carries it,
/// else the root the daemon process registered at startup. A pristine
/// `fleet add-worker` unit does NOT set the variable — it renders only `PATH`
/// and `LOOM_DAEMON_SUPERVISOR` (`unit_env_tests::STRUCTURAL`) — so a
/// short-lived `session-exec host` sees it only because the daemon pins its
/// registered root into every dispatched child's environment
/// (`launch_env::apply_launch_env`, #13965). A unit re-rendered by
/// `loom-daemon-start.sh` in machine mode carries the same value directly
/// (#13937), as does any operator export.
fn primary_root(
    env: &dyn Fn(&str) -> Option<OsString>,
    registered: Option<&Path>,
) -> Option<PathBuf> {
    env("LOOM_MACHINE_CHECKOUT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or(registered.map(Path::to_path_buf))
}

/// The `GH_CONFIG_DIR` proven to hold the installation token for `slug`'s
/// owner: the daemon's per-owner registry, else the owner's App directory
/// under the daemon's `primary` root's (then this workspace's) `.loom/`, else,
/// when the owner IS that root's own owner (read from an `origin` on the same
/// GitHub host), the root's `.loom/gh-config` — the root owner is
/// deliberately absent from `gh-config-by-owner`. `None` when no directory is
/// matched to the owner. There is deliberately no process-`GH_CONFIG_DIR`
/// fallback: on a daemon host that is the root owner's credential.
///
/// The primary root comes first (#13933, #13937): it is the only copy the
/// daemon's refresh tick keeps fresh. A managed repository's own
/// `.loom/gh-config-by-owner/<owner>` is published once and never refreshed,
/// so preferring it handed the exec an expired installation token (`HTTP 401`)
/// whenever the process had no daemon registry to consult — every
/// `session-exec host` and hand-run `accounts session job`. The workspace copy
/// remains a fallback for a host with no primary root at all.
///
/// The root-owner step is what a hand-run `accounts session job` from an SSH
/// shell needs: that shell has no `GH_CONFIG_DIR`, and the daemon's own
/// registry is not in its process.
fn config_dir_for(
    workspace: &Path,
    primary: Option<&Path>,
    slug: &str,
    gh_host: &str,
) -> Option<PathBuf> {
    use crate::credential_preflight as creds;
    let owner = creds::owner_of_nwo(slug);
    if let Some(dir) = creds::gh_config_dir_for_owner_slug(slug) {
        return Some(dir);
    }
    let published = |root: &Path| {
        let dir = creds::github_app_gh_config_dir_for_owner(root, owner);
        dir.join("hosts.yml").is_file().then_some(dir)
    };
    let root_owned = |root: &Path| {
        let dir = creds::github_app_gh_config_dir(root);
        if !dir.join("hosts.yml").is_file() {
            return None;
        }
        root_github_owner(root, gh_host)?
            .eq_ignore_ascii_case(owner)
            .then_some(dir)
    };
    primary
        .and_then(published)
        .or_else(|| published(workspace))
        .or_else(|| primary.and_then(root_owned))
        .or_else(|| root_owned(workspace))
}

/// The owner of `root`'s `origin`, only when that remote is on `gh_host`
/// (HTTPS, `ssh://` or scp-style `git@host:owner/repo`). A root cloned from
/// any other forge owns no GitHub installation, whatever its owner is named.
fn root_github_owner(root: &Path, gh_host: &str) -> Option<String> {
    let url = crate::forge_cmd::get_remote_url(root)?;
    remote_github_owner(url.trim(), gh_host)
}

fn remote_github_owner(url: &str, gh_host: &str) -> Option<String> {
    let path = if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        (host == gh_host).then_some(path)?
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let rest = rest.strip_prefix("git@").unwrap_or(rest);
        let (host, path) = rest.split_once('/')?;
        (host == gh_host).then_some(path)?
    } else {
        let slug = github_slug(url, gh_host)?;
        return Some(crate::credential_preflight::owner_of_nwo(&slug).to_owned());
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/')).then(|| owner.to_owned())
}

/// `gh auth token` under `dir`, through the `gh` choke point. stdout is held
/// privately and returned only when it is a single printable token; it is
/// never logged, and a failure says nothing about its content.
pub(super) fn read_token(dir: &Path, program: Option<&Path>) -> Option<String> {
    use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
    use crate::proc_exec::Completion;
    let mut invocation = GhInvocation::new(
        Operation::new("auth.token"),
        AccessIntent::Read,
        GhTarget::None,
        TOKEN_READ_TIMEOUT,
    )
    .args(["auth", "token"])
    .gh_config_dir(Some(dir))
    // A local answer about THIS credential: never routed to a reader App.
    .writer_identity();
    if let Some(program) = program {
        invocation = invocation.program(program);
    }
    let output = match invocation.execute() {
        Ok(GhCompletion::Captured(Completion::Exited(output))) if output.status.success() => output,
        _ => {
            log::warn!(
                "private workspace: no GitHub App token could be read for this exec; \
                 the worker's forge access will fail"
            );
            return None;
        }
    };
    let token = String::from_utf8(output.stdout).ok()?;
    let token = token.trim();
    (!token.is_empty() && !token.chars().any(|c| c.is_control() || c.is_whitespace()))
        .then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::collections::HashMap;

    const SECRET: &str = "ghs_0123456789abcdefTESTONLY";

    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).map(OsString::from)
    }

    #[test]
    fn an_app_only_host_resolves_a_token_for_the_child_and_names_it_bare() {
        let ws = published_owner_workspace("acme");
        let env = env_of(&[]);
        let auth =
            ExecAuth::resolve_with(ws.path(), "https://github.com/acme/widget", &env, &|_| {
                Some(SECRET.into())
            });
        assert!(auth.resolved());
        assert_eq!(auth.names(), &["GH_TOKEN"]);
        assert_eq!(auth.child_env(), vec![("GH_TOKEN".into(), SECRET.into())]);
    }

    #[test]
    fn a_token_already_in_the_process_env_adds_nothing() {
        let ws = tempfile::tempdir().unwrap();
        for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
            let env = env_of(&[(var, "from-process")]);
            let read = |_: &Path| -> Option<String> {
                panic!("must not read a token when the process has one")
            };
            let auth =
                ExecAuth::resolve_with(ws.path(), "https://github.com/acme/widget", &env, &read);
            assert!(!auth.resolved());
            assert_eq!(auth.names(), &[var]);
            assert!(auth.child_env().is_empty());
        }
    }

    #[test]
    fn a_non_github_forge_never_reads_a_github_token() {
        let ws = tempfile::tempdir().unwrap();
        let env = env_of(&[("GITEA_TOKEN", "x")]);
        let read = |_: &Path| -> Option<String> { panic!("not a GitHub repository") };
        let auth = ExecAuth::resolve_with(ws.path(), "https://forge.example/o/r", &env, &read);
        assert!(!auth.resolved());
        assert_eq!(auth.names(), &["GITEA_TOKEN"]);
    }

    #[test]
    fn the_owner_app_dir_is_read_and_an_unmatched_owner_reads_nothing() {
        let ws = tempfile::tempdir().unwrap();
        let owner_dir = crate::credential_preflight::github_app_gh_config_dir_for_owner(
            ws.path(),
            "acme-unregistered-13916",
        );
        std::fs::create_dir_all(&owner_dir).unwrap();
        std::fs::write(owner_dir.join("hosts.yml"), "github.com: {}\n").unwrap();
        let env = env_of(&[("GH_CONFIG_DIR", "/process/gh")]);
        let seen = std::cell::RefCell::new(None);
        let read = |dir: &Path| {
            *seen.borrow_mut() = Some(dir.to_path_buf());
            Some(SECRET.to_owned())
        };
        ExecAuth::resolve_with(
            ws.path(),
            "https://github.com/acme-unregistered-13916/widget.git",
            &env,
            &read,
        );
        assert_eq!(seen.borrow().as_deref(), Some(owner_dir.as_path()));
        // No directory matched to this owner: nothing is read, nothing is
        // forwarded. The process `GH_CONFIG_DIR` (the root owner's credential
        // on a daemon host) is never a fallback.
        let never = |_: &Path| -> Option<String> { panic!("read a token for an unmatched owner") };
        let auth = ExecAuth::resolve_with(
            ws.path(),
            "https://github.com/other-unregistered-13916/widget",
            &env,
            &never,
        );
        assert!(!auth.resolved());
        assert!(auth.names().is_empty());
        assert!(auth.child_env().is_empty());
    }

    /// The workspace-root owner has no `gh-config-by-owner` entry: its token
    /// lives in the root's `.loom/gh-config` (a hand-run `accounts session
    /// job` from an SSH shell). Another owner, or a root whose origin is on a
    /// different host, never borrows it.
    #[test]
    fn the_root_owner_reads_the_root_app_dir_and_no_one_else_does() {
        let ws = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(ws.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/RootOwner13916/primary.git",
        ]);
        let root_dir = crate::credential_preflight::github_app_gh_config_dir(ws.path());
        std::fs::create_dir_all(&root_dir).unwrap();
        std::fs::write(root_dir.join("hosts.yml"), "github.com: {}\n").unwrap();
        let env = env_of(&[("GH_CONFIG_DIR", "/process/gh")]);
        let seen = std::cell::RefCell::new(None);
        let read = |dir: &Path| {
            *seen.borrow_mut() = Some(dir.to_path_buf());
            Some(SECRET.to_owned())
        };
        let auth = ExecAuth::resolve_with(
            ws.path(),
            "https://github.com/rootowner13916/second",
            &env,
            &read,
        );
        assert!(auth.resolved());
        assert_eq!(seen.borrow().as_deref(), Some(root_dir.as_path()));
        // Another owner never borrows the root owner's installation token.
        let never = |_: &Path| -> Option<String> { panic!("borrowed the root owner's token") };
        let other = ExecAuth::resolve_with(
            ws.path(),
            "https://github.com/someone-else-13916/repo",
            &env,
            &never,
        );
        assert!(!other.resolved());
        assert!(other.names().is_empty());
        assert!(other.child_env().is_empty());
        // Same owner name, but the root was cloned from another host.
        git(&[
            "remote",
            "set-url",
            "origin",
            "https://ghe.example/RootOwner13916/primary.git",
        ]);
        let elsewhere = ExecAuth::resolve_with(
            ws.path(),
            "https://github.com/rootowner13916/second",
            &env,
            &never,
        );
        assert!(!elsewhere.resolved());
    }

    /// #13933 / #13937: a process with no daemon registry (a `session-exec
    /// host`, a hand-run job) reads the owner dir the daemon's refresh tick
    /// keeps fresh under `LOOM_MACHINE_CHECKOUT`, never the stale copy a
    /// managed repository's own `.loom/` holds. The workspace copy is only a
    /// fallback when the primary root has none for this owner.
    #[test]
    fn the_primary_roots_refreshed_owner_dir_wins_over_a_stale_workspace_copy() {
        let owner = "acme-precedence-13937";
        let stale = published_owner_workspace(owner);
        let fresh = published_owner_workspace(owner);
        let fresh_dir =
            crate::credential_preflight::github_app_gh_config_dir_for_owner(fresh.path(), owner);
        let stale_dir =
            crate::credential_preflight::github_app_gh_config_dir_for_owner(stale.path(), owner);
        let checkout = fresh.path().to_str().unwrap().to_owned();
        let env = env_of(&[("LOOM_MACHINE_CHECKOUT", &checkout)]);
        let seen = std::cell::RefCell::new(None);
        let read = |dir: &Path| {
            *seen.borrow_mut() = Some(dir.to_path_buf());
            Some(SECRET.to_owned())
        };
        let repo = format!("https://github.com/{owner}/widget");
        let auth = ExecAuth::resolve_with(stale.path(), &repo, &env, &read);
        assert!(auth.resolved());
        assert_eq!(seen.borrow().as_deref(), Some(fresh_dir.as_path()));
        // A primary root with nothing for this owner falls back to the
        // workspace copy rather than reading nothing.
        let empty = tempfile::tempdir().unwrap();
        let elsewhere = empty.path().to_str().unwrap().to_owned();
        let env = env_of(&[("LOOM_MACHINE_CHECKOUT", &elsewhere)]);
        ExecAuth::resolve_with(stale.path(), &repo, &env, &read);
        assert_eq!(seen.borrow().as_deref(), Some(stale_dir.as_path()));
    }

    /// #13965: the dispatch pin is the only source on a pristine fleet worker.
    /// The unit carries no `LOOM_MACHINE_CHECKOUT` (`unit_env_tests::STRUCTURAL`)
    /// and a short-lived `session-exec host` has no registered root of its
    /// own, so the root the daemon pins into the dispatched child's env must
    /// select the refreshed owner dir over the stale workspace copy with the
    /// variable absent from the process env.
    #[test]
    fn a_dispatched_primary_root_wins_when_the_env_carries_no_checkout_var() {
        let owner = "acme-dispatch-13965";
        let stale = published_owner_workspace(owner);
        let fresh = published_owner_workspace(owner);
        let fresh_dir =
            crate::credential_preflight::github_app_gh_config_dir_for_owner(fresh.path(), owner);
        let seen = std::cell::RefCell::new(None);
        let read = |dir: &Path| {
            *seen.borrow_mut() = Some(dir.to_path_buf());
            Some(SECRET.to_owned())
        };
        let repo = format!("https://github.com/{owner}/widget");
        // The pristine unit's env: no `LOOM_MACHINE_CHECKOUT` anywhere.
        let env = env_of(&[]);
        let auth =
            ExecAuth::resolve_with_root(stale.path(), &repo, &env, &read, Some(fresh.path()));
        assert!(auth.resolved());
        assert_eq!(seen.borrow().as_deref(), Some(fresh_dir.as_path()));
    }

    /// The primary root holds other owners' dirs too: an owner with no dir of
    /// its own there (or in the workspace) still gets nothing, never a
    /// neighbour's installation token.
    #[test]
    fn a_primary_root_never_forwards_another_owners_token() {
        let primary = published_owner_workspace("acme-owner-13937");
        let checkout = primary.path().to_str().unwrap().to_owned();
        let env = env_of(&[("LOOM_MACHINE_CHECKOUT", &checkout)]);
        let ws = published_owner_workspace("acme-owner-13937");
        let never = |_: &Path| -> Option<String> { panic!("read another owner's token") };
        let auth = ExecAuth::resolve_with(
            ws.path(),
            "https://github.com/other-owner-13937/widget",
            &env,
            &never,
        );
        assert!(!auth.resolved());
        assert!(auth.names().is_empty());
        assert!(auth.child_env().is_empty());
    }

    #[test]
    fn remote_github_owner_requires_the_github_host() {
        for url in [
            "https://github.com/Own/repo.git",
            "git@github.com:Own/repo.git",
            "ssh://git@github.com/Own/repo",
        ] {
            assert_eq!(remote_github_owner(url, "github.com").as_deref(), Some("Own"), "{url}");
        }
        for url in [
            "https://ghe.example/Own/repo",
            "git@ghe.example:Own/repo.git",
            "ssh://git@ghe.example/Own/repo",
            "git@github.com:Own/a/b",
        ] {
            assert_eq!(remote_github_owner(url, "github.com"), None, "{url}");
        }
    }

    #[test]
    fn github_slug_accepts_only_owner_repo_on_the_github_host() {
        assert_eq!(github_slug("https://github.com/o/r.git", "github.com").as_deref(), Some("o/r"));
        assert_eq!(github_slug("https://github.com/a/b/c", "github.com"), None);
        assert_eq!(github_slug("https://forge.example/o/r", "github.com"), None);
        assert_eq!(github_slug("https://ghe.example/o/r", "ghe.example").as_deref(), Some("o/r"));
    }

    /// The real read, through the facade, against a stub `gh`: the token is
    /// read under the chosen `GH_CONFIG_DIR` and returned trimmed.
    #[test]
    fn read_token_runs_gh_auth_token_under_the_chosen_config_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("gh");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\n[ \"$1 $2\" = \"auth token\" ] || exit 2\n\
                 [ \"$GH_CONFIG_DIR\" = \"{}\" ] || exit 3\necho {SECRET}\n",
                dir.path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(read_token(dir.path(), Some(&stub)).as_deref(), Some(SECRET));
        assert_eq!(read_token(Path::new("/elsewhere"), Some(&stub)), None);
    }
}
