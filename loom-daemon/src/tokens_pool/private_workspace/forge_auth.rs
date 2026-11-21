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
use std::time::{Duration, SystemTime};

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
    /// The repository is on GitHub and neither the process environment nor
    /// the owner's published App directory yielded a token (#14433).
    github_unresolved: bool,
    /// A credential directory matched to this repository's owner exists but
    /// its token is provably expired, and no fresher one was found (#14435).
    expired: bool,
}

impl ExecAuth {
    /// The credential for an exec acting on `repository` from `workspace`.
    pub(super) fn resolve(workspace: &Path, repository: &str) -> Self {
        Self::resolve_with_lookup(
            workspace,
            repository,
            &|name| std::env::var_os(name),
            &|dir| read_token(dir, None),
            crate::credential_preflight::primary_workspace_root().as_deref(),
            &crate::cli::common::primary_workspace_root_from_daemon,
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
    #[cfg(test)]
    fn resolve_with_root(
        workspace: &Path,
        repository: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
        read: &dyn Fn(&Path) -> Option<String>,
        registered: Option<&Path>,
    ) -> Self {
        Self::resolve_with_lookup(workspace, repository, env, read, registered, &|| None)
    }

    /// [`Self::resolve_with_root`] with the daemon lookup injected too
    /// (#14534). `daemon_root` runs only when neither the process env nor the
    /// registered root names a primary root, i.e. in a CLI process launched
    /// outside the daemon's unit, and only for a GitHub repository that still
    /// needs a token.
    fn resolve_with_lookup(
        workspace: &Path,
        repository: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
        read: &dyn Fn(&Path) -> Option<String>,
        registered: Option<&Path>,
        daemon_root: &dyn Fn() -> Option<PathBuf>,
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
                github_unresolved: false,
                expired: false,
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
                github_unresolved: false,
                expired: false,
            };
        };
        // No directory proven to hold THIS owner's installation token: read
        // nothing. The process `GH_CONFIG_DIR` is the workspace-root owner's
        // credential on a daemon host, and handing it to another owner's
        // repository would forward the wrong installation's token.
        let primary = primary_root(env, registered, daemon_root);
        let choice =
            config_dir_for(workspace, primary.as_deref(), &slug, &gh_host, SystemTime::now());
        let secret = choice.dir.and_then(|dir| read(&dir));
        if secret.is_some() && !names.contains(&TOKEN_ENV) {
            names.push(TOKEN_ENV);
        }
        Self {
            names,
            github_unresolved: secret.is_none(),
            expired: secret.is_none() && choice.expired,
            secret,
        }
    }

    /// `true` when this owner's only published credential is provably
    /// expired: the exec would carry a dead token (or none), so the caller
    /// should refuse before it claims anything (#14435).
    pub(super) fn expired(&self) -> bool {
        self.expired
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

    /// `true` for a GitHub repository with no write credential at all: no
    /// `GH_TOKEN`/`GITHUB_TOKEN` in the process and no installation token read
    /// from the owner's published App directory. A non-GitHub forge is never
    /// unresolved here (it keeps its own `GITEA_TOKEN` path), and neither is a
    /// process that carries a token (#14433).
    pub(super) fn github_token_missing(&self) -> bool {
        self.github_unresolved
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
/// (#13937), as does any operator export. A CLI process launched from an
/// operator shell has none of these, so as a last resort it asks the running
/// daemon for the root it registered (#14534); an explicit root always wins.
fn primary_root(
    env: &dyn Fn(&str) -> Option<OsString>,
    registered: Option<&Path>,
    daemon_root: &dyn Fn() -> Option<PathBuf>,
) -> Option<PathBuf> {
    env("LOOM_MACHINE_CHECKOUT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or(registered.map(Path::to_path_buf))
        .or_else(daemon_root)
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
///
/// A candidate whose `identity.json` proves its token expired is skipped, not
/// returned (#14435): on a daemon whose workspace root is itself the repository
/// owner, `gh-config-by-owner/<owner>/hosts.yml` is a leftover that nothing
/// refreshes (the reader pool's `<app-id>/` subdirectories keep the directory
/// itself looking live), while the live writer token sits in `.loom/gh-config`.
/// Returning the leftover first handed every exec for that owner a dead token.
fn config_dir_for(
    workspace: &Path,
    primary: Option<&Path>,
    slug: &str,
    gh_host: &str,
    now: SystemTime,
) -> DirChoice {
    use crate::credential_preflight as creds;
    let owner = creds::owner_of_nwo(slug);
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
    let candidates: [&dyn Fn() -> Option<PathBuf>; 5] = [
        &|| creds::gh_config_dir_for_owner_slug(slug).filter(|dir| dir.join("hosts.yml").is_file()),
        &|| primary.and_then(published),
        &|| published(workspace),
        &|| primary.and_then(root_owned),
        &|| root_owned(workspace),
    ];
    // The first candidate whose token is not provably dead wins. A directory
    // that is merely present is NOT enough (#14435): `gh-config-by-owner/
    // <owner>/hosts.yml` outlives its token whenever nothing refreshes it
    // (the workspace-root owner's live token is in `.loom/gh-config`), and
    // `gh auth token` hands back an expired value without complaint.
    let mut expired = false;
    for candidate in candidates {
        let Some(dir) = candidate() else { continue };
        if token_expired(&dir, now) {
            expired = true;
            continue;
        }
        return DirChoice {
            dir: Some(dir),
            expired: false,
        };
    }
    DirChoice { dir: None, expired }
}

/// Which credential directory an exec reads, and whether a matched one was
/// passed over because its token is dead.
struct DirChoice {
    dir: Option<PathBuf>,
    expired: bool,
}

/// `true` only when `dir`'s `identity.json` sidecar PROVES the token expired
/// (or inside [`READER_MIN_REMAINING`] of it). A directory with no readable
/// sidecar is unknown, not expired: it keeps the pre-sidecar behaviour.
fn token_expired(dir: &Path, now: SystemTime) -> bool {
    let Some(side) = crate::forge_identity::read_sidecar(dir) else {
        return false;
    };
    let Ok(exp) = chrono::DateTime::parse_from_rfc3339(&side.expires_at) else {
        return false;
    };
    let exp: SystemTime = exp.with_timezone(&chrono::Utc).into();
    exp.duration_since(now)
        .map_or(true, |left| left < crate::forge_identity::READER_MIN_REMAINING)
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

/// A mirror fetch's proved reader (#14716): the exact canonical route and
/// the exact credential one [`read_authority::ReadProof`] verified.
///
/// There is no ambient fallback BY CONSTRUCTION — a `ProvedReader` exists
/// only when a proof supplied the credential — and the fetch environment
/// disables every ambient source: credential helpers are cleared, URL
/// rewriting and system/global config are not read, and HTTP redirects are
/// refused, so the fetch cannot drift to another origin or credential. The
/// credential travels as a git config value in the CHILD'S ENVIRONMENT
/// (`GIT_CONFIG_*`, git ≥ 2.31), never in argv, never in a log.
///
/// Legacy private clones and seat writer selection keep
/// [`ExecAuth::resolve`] untouched: an unproved route never constructs one
/// of these and stays on the legacy path.
pub(super) struct ProvedReader {
    url: String,
    /// The full `Authorization` header value. Private on purpose: never
    /// argv, never logged, never serialized.
    header: String,
}

impl ProvedReader {
    /// The reader for `proof` fetching from the registered canonical `url`.
    /// `None` unless `url` names the exact repository the proof verified on
    /// the same forge base — a proof for another origin or another
    /// repository cannot authorize this fetch, and the caller stays on the
    /// legacy path.
    pub(super) fn new(proof: &super::read_authority::ReadProof, url: &str) -> Option<Self> {
        let base = super::read_authority::ForgeBase::of_repository_url(url)?;
        (proof.grant().identity.base == base && super::same_repository(proof.origin(), url)).then(
            || Self {
                url: url.to_owned(),
                header: auth_header(proof),
            },
        )
    }

    /// The credential-free canonical URL the fetch must use.
    pub(super) fn url(&self) -> &str {
        &self.url
    }

    /// Environment for the `git fetch` child: the proved credential rides
    /// as an `http.extraHeader` config value in the environment, and every
    /// ambient credential source is disabled (#14716).
    pub(super) fn fetch_env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
            ("GIT_CONFIG_COUNT".to_owned(), "3".to_owned()),
            ("GIT_CONFIG_KEY_0".to_owned(), "credential.helper".to_owned()),
            ("GIT_CONFIG_VALUE_0".to_owned(), String::new()),
            ("GIT_CONFIG_KEY_1".to_owned(), "http.followRedirects".to_owned()),
            ("GIT_CONFIG_VALUE_1".to_owned(), "false".to_owned()),
            ("GIT_CONFIG_KEY_2".to_owned(), "http.extraHeader".to_owned()),
        ];
        env.push(("GIT_CONFIG_VALUE_2".to_owned(), self.header.clone()));
        env
    }
}

/// The Authorization header a proved fetch carries: HTTP Basic for git's
/// smart protocol, the username carrying the credential per provider
/// convention (GitHub: `x-access-token:<token>`; Gitea: `<token>:`).
fn auth_header(proof: &super::read_authority::ReadProof) -> String {
    use base64::Engine as _;
    let pair = match proof.grant().identity.provider {
        super::read_authority::Provider::GitHub => {
            format!("x-access-token:{}", proof.credential().value())
        }
        super::read_authority::Provider::Gitea => format!("{}:", proof.credential().value()),
    };
    format!(
        "Authorization: Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
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

    /// #14433: a GitHub repository is "missing" its write token only when the
    /// process has none AND no owner App directory yields one; a published
    /// directory (the operator-shell root fallback), a process token and a
    /// non-GitHub forge are all not missing.
    #[test]
    fn github_token_missing_only_when_no_route_yields_a_credential() {
        let repo = "https://github.com/acme/widget";
        let empty = tempfile::tempdir().unwrap();
        let none = env_of(&[]);
        let unread = |_: &Path| -> Option<String> { None };
        let auth = ExecAuth::resolve_with_root(empty.path(), repo, &none, &unread, None);
        assert!(auth.github_token_missing());
        assert!(auth.names().is_empty() && auth.child_env().is_empty());

        // The owner's App dir is published under the daemon root, and the
        // process (an operator shell) has no env token and no registry.
        let root = published_owner_workspace("acme");
        let read = |_: &Path| Some(SECRET.to_owned());
        let auth = ExecAuth::resolve_with_root(empty.path(), repo, &none, &read, Some(root.path()));
        assert!(!auth.github_token_missing());
        assert!(auth.resolved());

        // Published, but the token read yields nothing: still missing.
        let auth =
            ExecAuth::resolve_with_root(empty.path(), repo, &none, &unread, Some(root.path()));
        assert!(auth.github_token_missing());

        let with_env = env_of(&[("GITHUB_TOKEN", "from-process")]);
        let auth = ExecAuth::resolve_with_root(empty.path(), repo, &with_env, &unread, None);
        assert!(!auth.github_token_missing());

        let gitea = ExecAuth::resolve_with_root(
            empty.path(),
            "https://forge.example/acme/widget",
            &none,
            &unread,
            None,
        );
        assert!(!gitea.github_token_missing());
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
    fn a_reaped_registration_falls_back_only_to_a_matching_root() {
        use crate::credential_preflight as creds;
        use crate::test_git_helpers::run;

        let owner = "registered-owner-14523";
        let repo = format!("https://github.com/{owner}/widget");
        let ws = tempfile::tempdir().unwrap();
        run(ws.path(), &["init", "-q"]);
        run(ws.path(), &["remote", "add", "origin", &repo]);
        let published = published_owner_workspace(owner);
        let registered = creds::github_app_gh_config_dir_for_owner(published.path(), owner);
        creds::register_owner_gh_config_dir(owner, &registered);
        let root = creds::github_app_gh_config_dir(ws.path());
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("hosts.yml"), "github.com: {}\n").unwrap();

        let env = env_of(&[("GH_CONFIG_DIR", root.to_str().unwrap())]);
        let expect_read = |expected: &Path| {
            let auth = ExecAuth::resolve_with_root(
                ws.path(),
                &repo,
                &env,
                &|dir| {
                    assert_eq!(dir, expected);
                    Some(SECRET.into())
                },
                None,
            );
            assert!(auth.resolved());
            assert!(!auth.expired());
        };
        expect_read(&registered);
        std::fs::remove_file(registered.join("hosts.yml")).unwrap();
        expect_read(&root);
        std::fs::remove_dir(&registered).unwrap();
        expect_read(&root);

        for origin in [
            "https://github.com/other-owner-14523/widget",
            "https://ghe.example/registered-owner-14523/widget",
        ] {
            run(ws.path(), &["remote", "set-url", "origin", origin]);
            let auth = ExecAuth::resolve_with_root(
                ws.path(),
                &repo,
                &env,
                &|_| panic!("read a credential without a matching owner and host"),
                None,
            );
            assert!(auth.child_env().is_empty());
            assert!(!auth.expired());
        }
    }

    /// Publish an `identity.json` sidecar beside `dir`'s `hosts.yml`.
    fn write_sidecar(dir: &Path, expires_at: &str) {
        std::fs::write(
            dir.join("identity.json"),
            format!(
                r#"{{"appId":"1","slug":null,"installationId":"2","owner":"acme","role":"writer","expiresAt":"{expires_at}"}}"#
            ),
        )
        .unwrap();
    }

    /// #14435: on a daemon whose root IS the repository owner, a leftover
    /// `gh-config-by-owner/<owner>` with an expired token must not shadow the
    /// root's live `.loom/gh-config` (the layout measured on a fleet worker:
    /// stale per-owner writer file, fresh root writer file). Judge and Builder
    /// execs alike would otherwise carry a dead token (`401` / git `128`).
    #[test]
    fn an_expired_owner_dir_never_shadows_the_live_root_owner_dir() {
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
            "https://github.com/RootOwner14435/primary.git",
        ]);
        let stale = crate::credential_preflight::github_app_gh_config_dir_for_owner(
            ws.path(),
            "rootowner14435",
        );
        let live = crate::credential_preflight::github_app_gh_config_dir(ws.path());
        for dir in [&stale, &live] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("hosts.yml"), "github.com: {}\n").unwrap();
        }
        write_sidecar(&stale, "2020-01-01T00:00:00Z");
        write_sidecar(&live, "2999-01-01T00:00:00Z");
        let seen = std::cell::RefCell::new(None);
        let read = |dir: &Path| {
            *seen.borrow_mut() = Some(dir.to_path_buf());
            Some(SECRET.to_owned())
        };
        let auth = ExecAuth::resolve_with_root(
            ws.path(),
            "https://github.com/rootowner14435/second",
            &env_of(&[]),
            &read,
            Some(ws.path()),
        );
        assert!(auth.resolved());
        assert!(!auth.expired());
        assert_eq!(seen.borrow().as_deref(), Some(live.as_path()));
    }

    /// #14435: with nothing fresher, an expired credential is neither read nor
    /// forwarded, and the exec says so, so admission can refuse up front.
    #[test]
    fn an_owner_whose_only_credential_is_expired_reads_nothing_and_reports_it() {
        let ws = published_owner_workspace("acme-expired-14435");
        let dir = crate::credential_preflight::github_app_gh_config_dir_for_owner(
            ws.path(),
            "acme-expired-14435",
        );
        write_sidecar(&dir, "2020-01-01T00:00:00Z");
        let never = |_: &Path| -> Option<String> { panic!("read an expired token") };
        let auth = ExecAuth::resolve_with_root(
            ws.path(),
            "https://github.com/acme-expired-14435/widget",
            &env_of(&[]),
            &never,
            None,
        );
        assert!(!auth.resolved());
        assert!(auth.expired());
        assert!(auth.names().is_empty());
        assert!(auth.child_env().is_empty());
        // A sidecar-less directory is unknown, not expired: still read.
        std::fs::remove_file(dir.join("identity.json")).unwrap();
        let auth = ExecAuth::resolve_with_root(
            ws.path(),
            "https://github.com/acme-expired-14435/widget",
            &env_of(&[]),
            &|_| Some(SECRET.into()),
            None,
        );
        assert!(auth.resolved());
        assert!(!auth.expired());
        // An owner with no directory at all is unmatched, not expired
        // (a public repository needs no credential).
        let none = ExecAuth::resolve_with_root(
            ws.path(),
            "https://github.com/nobody-14435/widget",
            &env_of(&[]),
            &never,
            None,
        );
        assert!(!none.expired());
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

    /// #14534: a CLI process outside the daemon's unit has neither the env var
    /// nor an in-process registration, so the daemon's own answer is the only
    /// source of the primary root. The owner's credential published ONLY under
    /// that root (the seat's workspace holds nothing) is then found and read.
    #[test]
    fn the_daemons_answer_supplies_the_primary_root_when_no_other_source_exists() {
        let daemon_root = published_owner_workspace("acme");
        let seat = tempfile::tempdir().unwrap();
        let repo = "https://github.com/acme/widget";
        let none = env_of(&[]);
        let read = |dir: &Path| {
            dir.starts_with(daemon_root.path())
                .then(|| SECRET.to_owned())
        };
        let asked = std::cell::Cell::new(0);
        let lookup = || {
            asked.set(asked.get() + 1);
            Some(daemon_root.path().to_path_buf())
        };
        let auth = ExecAuth::resolve_with_lookup(seat.path(), repo, &none, &read, None, &lookup);
        assert!(auth.resolved() && !auth.github_token_missing());
        assert_eq!(auth.child_env(), [(TOKEN_ENV.to_owned(), SECRET.to_owned())]);
        assert_eq!(auth.names(), [TOKEN_ENV]);
        assert_eq!(asked.get(), 1);
    }

    /// The daemon lookup is the LAST resort: an explicit export or the
    /// in-process registration wins, and neither pays for a socket round trip.
    /// A token already in the environment never asks either.
    #[test]
    fn the_daemon_lookup_is_consulted_only_when_nothing_else_names_a_root() {
        let daemon_root = published_owner_workspace("acme");
        let other = published_owner_workspace("acme");
        let seat = tempfile::tempdir().unwrap();
        let repo = "https://github.com/acme/widget";
        let read = |dir: &Path| {
            dir.starts_with(other.path())
                .then(|| "from-other".to_owned())
        };
        let asked = std::cell::Cell::new(0);
        let lookup = || {
            asked.set(asked.get() + 1);
            Some(daemon_root.path().to_path_buf())
        };
        let checkout = other.path().display().to_string();
        let exported = env_of(&[("LOOM_MACHINE_CHECKOUT", &checkout)]);
        let auth =
            ExecAuth::resolve_with_lookup(seat.path(), repo, &exported, &read, None, &lookup);
        assert_eq!(auth.child_env(), [(TOKEN_ENV.to_owned(), "from-other".to_owned())]);
        let none = env_of(&[]);
        let auth = ExecAuth::resolve_with_lookup(
            seat.path(),
            repo,
            &none,
            &read,
            Some(other.path()),
            &lookup,
        );
        assert!(auth.resolved());
        let with_token = env_of(&[("GH_TOKEN", "x")]);
        let _ = ExecAuth::resolve_with_lookup(seat.path(), repo, &with_token, &read, None, &lookup);
        let gitea = ExecAuth::resolve_with_lookup(
            seat.path(),
            "https://git.example.com/acme/widget",
            &none,
            &read,
            None,
            &lookup,
        );
        assert!(!gitea.github_token_missing());
        assert_eq!(asked.get(), 0, "no daemon round trip when it cannot matter");
    }

    /// An unavailable daemon changes nothing: the workspace copy still serves,
    /// and with no credential anywhere the result is the same refusal state.
    #[test]
    fn an_unavailable_daemon_leaves_existing_routes_and_the_refusal_intact() {
        let ws = published_owner_workspace("acme");
        let repo = "https://github.com/acme/widget";
        let none = env_of(&[]);
        let read = |_: &Path| Some(SECRET.to_owned());
        let down = || None;
        let auth = ExecAuth::resolve_with_lookup(ws.path(), repo, &none, &read, None, &down);
        assert!(auth.resolved());
        let bare = tempfile::tempdir().unwrap();
        let auth = ExecAuth::resolve_with_lookup(bare.path(), repo, &none, &read, None, &down);
        assert!(auth.github_token_missing() && !auth.expired() && auth.child_env().is_empty());
    }

    /// The daemon's root holds only ANOTHER owner's credential: it is never
    /// forwarded for this repository, and nothing resolves.
    #[test]
    fn the_daemons_root_never_forwards_another_owners_token() {
        let daemon_root = published_owner_workspace("someone-else");
        let seat = tempfile::tempdir().unwrap();
        let none = env_of(&[]);
        let read = |_: &Path| Some(SECRET.to_owned());
        let lookup = || Some(daemon_root.path().to_path_buf());
        let auth = ExecAuth::resolve_with_lookup(
            seat.path(),
            "https://github.com/acme/widget",
            &none,
            &read,
            None,
            &lookup,
        );
        assert!(auth.github_token_missing() && auth.child_env().is_empty());
    }

    /// #14534 across the real boundaries: a separate "operator shell" state
    /// (no token, no config dir, no checkout export, no in-process root) with
    /// a seat workspace distinct from the daemon's root. The daemon answers
    /// over a real Unix socket (found through `LOOM_SOCKET_PATH`), the owner's
    /// credential exists only under the daemon's root, and the real
    /// `gh auth token` read runs against a stub that answers only for that
    /// directory. The token reaches only the child env, never argv names.
    #[test]
    #[serial_test::serial]
    fn a_bare_shell_resolves_the_owner_credential_through_the_daemons_socket() {
        use std::io::{BufRead, Write};
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;
        let daemon_root = published_owner_workspace("acme");
        let seat = tempfile::tempdir().unwrap();
        let sock_dir = tempfile::tempdir().unwrap();
        let socket = sock_dir.path().join("loom-daemon.sock");
        let stub = sock_dir.path().join("gh");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\n[ \"$1 $2\" = \"auth token\" ] || exit 2\n\
                 case \"$GH_CONFIG_DIR\" in {}/*) echo {SECRET} ;; *) exit 3 ;; esac\n",
                daemon_root.path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let reply = serde_json::to_string(&crate::types::Response::PrimaryWorkspaceRoot {
            path: daemon_root.path().display().to_string(),
        })
        .unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert!(line.contains("GetPrimaryWorkspaceRoot"), "{line}");
            let mut stream = stream;
            writeln!(stream, "{reply}").unwrap();
        });
        let _env = crate::env_test_support::EnvGuard::set("LOOM_SOCKET_PATH", &socket);
        let none = env_of(&[]);
        let read = |dir: &Path| read_token(dir, Some(&stub));
        let auth = ExecAuth::resolve_with_lookup(
            seat.path(),
            "https://github.com/acme/widget",
            &none,
            &read,
            None,
            &crate::cli::common::primary_workspace_root_from_daemon,
        );
        server.join().unwrap();
        assert!(auth.resolved() && !auth.github_token_missing());
        assert_eq!(auth.child_env(), [(TOKEN_ENV.to_owned(), SECRET.to_owned())]);
        assert_eq!(auth.names(), [TOKEN_ENV], "the value is never an argv name");
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
