//! The daemon half of `ReadIssueState` (#8876).
//!
//! Runs inside the process that owns the forge credentials and routing, so
//! the read uses exactly the identity every other daemon `gh` call uses. All
//! environment access goes through [`Backend`] so the handler's decisions are
//! testable without a forge, a registry file or a policy on disk.

use std::path::{Path, PathBuf};

use super::{parse_state, valid_slug, IssueStateReply, ReadOutcome};
use crate::workspace_registry::{normalize_path, WorkspaceRegistry};

/// Route label for a read the daemon answered itself.
const ROUTE_PREFIX: &str = "daemon-gh";

/// The side effects a read needs.
pub trait Backend {
    /// `owner/repo` from `root`'s `origin` remote on the supported forge host.
    fn remote_slug(&self, root: &Path) -> Option<String>;
    /// `Some(codes)` when the effective forge-egress policy refuses forge work
    /// for `root`. Codes are non-secret policy finding identifiers.
    fn admission_refusal(&self, root: &Path) -> Option<String>;
    /// Run the one `gh issue view` for `slug`/`issue` from `root`.
    fn read(&self, root: &Path, slug: &str, issue: u64) -> GhRead;
}

/// What the child produced and which launcher rung ran it.
pub struct GhRead {
    /// Stdout, only when the process ran and exited zero.
    pub stdout: Option<Vec<u8>>,
    /// The launcher rung that ran it (`policy`, `path`, ...).
    pub source: &'static str,
}

/// Production [`Backend`].
pub struct LiveBackend;

impl Backend for LiveBackend {
    fn remote_slug(&self, root: &Path) -> Option<String> {
        crate::release_resolve::host::repo_slug(root)
    }

    fn admission_refusal(&self, root: &Path) -> Option<String> {
        let sources = crate::forge_egress::policy::PolicySources::from_process(Some(root));
        crate::forge_egress::gate::dispatch_refusal_with(&sources, root).map(|r| r.codes.join(","))
    }

    fn read(&self, root: &Path, slug: &str, issue: u64) -> GhRead {
        let source = crate::gh_invocation::resolver::resolve().source.as_str();
        let stdout = crate::gh_state_probe::issue_state_output_for_repo(root, slug, issue)
            .filter(|o| o.status.success())
            .map(|o| o.stdout);
        GhRead { stdout, source }
    }
}

/// Answer one `ReadIssueState` request.
#[must_use]
pub fn serve(
    registry: &WorkspaceRegistry,
    backend: &dyn Backend,
    workspace_root: &str,
    repo: &str,
    issue: u64,
) -> IssueStateReply {
    let unknown = |reason: &str| {
        IssueStateReply::new(ReadOutcome::Unknown, format!("{ROUTE_PREFIX}/none"))
            .with_reason(reason)
    };
    if issue == 0 || !valid_slug(repo) {
        return unknown("invalid-request");
    }
    let raw = PathBuf::from(workspace_root);
    if !raw.is_absolute() {
        return unknown("invalid-request");
    }
    let root = normalize_path(&raw);
    if !registry.contains(&root) {
        return IssueStateReply::new(ReadOutcome::NotManaged, format!("{ROUTE_PREFIX}/none"))
            .with_reason("unregistered-root");
    }
    // The daemon trusts its own view of the checkout's identity, never the
    // caller's: a mismatch means the caller is looking at a different repo.
    match backend.remote_slug(&root) {
        Some(own) if own.eq_ignore_ascii_case(repo) => {}
        _ => return unknown("identity-mismatch"),
    }
    if let Some(codes) = backend.admission_refusal(&root) {
        return unknown(&format!("routing-refused:{codes}"));
    }
    let read = backend.read(&root, repo, issue);
    let route = format!("{ROUTE_PREFIX}/{}", read.source);
    let outcome = read
        .stdout
        .as_deref()
        .map_or(ReadOutcome::Unknown, parse_state);
    let reply = IssueStateReply::new(outcome, route);
    if outcome == ReadOutcome::Unknown {
        reply.with_reason("read-failed")
    } else {
        reply
    }
}

/// IPC entry point: load the machine registry and delegate to [`serve`].
#[must_use]
pub fn handle(workspace_root: &str, repo: &str, issue: u64) -> IssueStateReply {
    match WorkspaceRegistry::load_default() {
        Ok(registry) => serve(&registry, &LiveBackend, workspace_root, repo, issue),
        Err(_) => IssueStateReply::new(ReadOutcome::Unknown, format!("{ROUTE_PREFIX}/none"))
            .with_reason("registry-unreadable"),
    }
}

/// [`handle`] off the async runtime: the read is a blocking `gh` child.
pub async fn handle_blocking(workspace_root: String, repo: String, issue: u64) -> IssueStateReply {
    tokio::task::spawn_blocking(move || handle(&workspace_root, &repo, issue))
        .await
        .unwrap_or_else(|_| {
            IssueStateReply::new(ReadOutcome::Unknown, format!("{ROUTE_PREFIX}/none"))
                .with_reason("read-failed")
        })
}
