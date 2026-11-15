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
        Self::resolve_with(workspace, repository, &|name| std::env::var_os(name), &|dir| {
            read_token(dir, None)
        })
    }

    /// [`Self::resolve`] over an injected process environment and token
    /// reader (tests).
    pub(super) fn resolve_with(
        workspace: &Path,
        repository: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
        read: &dyn Fn(Option<&Path>) -> Option<String>,
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
        let dir = config_dir_for(workspace, &slug, env);
        let secret = read(dir.as_deref());
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

/// Which `GH_CONFIG_DIR` holds the installation token for `slug`'s owner: the
/// daemon's per-owner registry, else the owner's App directory under this
/// workspace's (then the daemon's primary root's) `.loom/`, else the
/// process-global `GH_CONFIG_DIR`. `None` leaves `gh` on its own default.
fn config_dir_for(
    workspace: &Path,
    slug: &str,
    env: &dyn Fn(&str) -> Option<OsString>,
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
    published(workspace)
        .or_else(|| creds::primary_workspace_root().and_then(|root| published(&root)))
        .or_else(|| env("GH_CONFIG_DIR").map(PathBuf::from))
}

/// `gh auth token` under `dir`, through the `gh` choke point. stdout is held
/// privately and returned only when it is a single printable token; it is
/// never logged, and a failure says nothing about its content.
pub(super) fn read_token(dir: Option<&Path>, program: Option<&Path>) -> Option<String> {
    use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
    use crate::proc_exec::Completion;
    let mut invocation = GhInvocation::new(
        Operation::new("auth.token"),
        AccessIntent::Read,
        GhTarget::None,
        TOKEN_READ_TIMEOUT,
    )
    .args(["auth", "token"])
    .gh_config_dir(dir)
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
        let ws = tempfile::tempdir().unwrap();
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
            let read = |_: Option<&Path>| -> Option<String> {
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
        let read = |_: Option<&Path>| -> Option<String> { panic!("not a GitHub repository") };
        let auth = ExecAuth::resolve_with(ws.path(), "https://forge.example/o/r", &env, &read);
        assert!(!auth.resolved());
        assert_eq!(auth.names(), &["GITEA_TOKEN"]);
    }

    #[test]
    fn the_owner_app_dir_under_the_workspace_wins_over_the_process_dir() {
        let ws = tempfile::tempdir().unwrap();
        let owner_dir = crate::credential_preflight::github_app_gh_config_dir_for_owner(
            ws.path(),
            "acme-unregistered-13916",
        );
        std::fs::create_dir_all(&owner_dir).unwrap();
        std::fs::write(owner_dir.join("hosts.yml"), "github.com: {}\n").unwrap();
        let env = env_of(&[("GH_CONFIG_DIR", "/process/gh")]);
        let seen = std::cell::RefCell::new(None);
        let read = |dir: Option<&Path>| {
            *seen.borrow_mut() = dir.map(Path::to_path_buf);
            Some(SECRET.to_owned())
        };
        ExecAuth::resolve_with(
            ws.path(),
            "https://github.com/acme-unregistered-13916/widget.git",
            &env,
            &read,
        );
        assert_eq!(seen.borrow().as_deref(), Some(owner_dir.as_path()));
        // No published owner dir: the process-global one.
        ExecAuth::resolve_with(
            ws.path(),
            "https://github.com/other-unregistered-13916/widget",
            &env,
            &read,
        );
        assert_eq!(seen.borrow().as_deref(), Some(Path::new("/process/gh")));
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
        assert_eq!(read_token(Some(dir.path()), Some(&stub)).as_deref(), Some(SECRET));
        assert_eq!(read_token(Some(Path::new("/elsewhere")), Some(&stub)), None);
    }
}
