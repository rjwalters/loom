//! `GET /api/api-keys` — the dashboard's API-key pool account view (Issue
//! #8447, follow-up to #8401/PR #8428).
//!
//! The dashboard's account view knew only the Claude OAuth pool
//! (`/api/tokens`, see [`super::read_token_rows`]). API-key accounts — and,
//! critically, the provider namespace that identifies them — did not appear at
//! all, so an operator could not see which provider a spawn was about to bill,
//! nor why a provider had nothing selectable.
//!
//! # Shape
//!
//! One flat row per account, built from
//! [`crate::api_keys_pool::health`] — the **same secret-free library call**
//! `loom-daemon api-keys health --json` renders, so this panel can never
//! disagree with the CLI about a pool's state, and can never carry key
//! material ([`crate::api_keys_pool::ApiKeyAccount`] structurally cannot hold
//! any).
//!
//! Each row names the pool **root** it came from, because roots resolve *per
//! provider* (per-repo pool, then the shared machine-level pool — see
//! [`crate::api_keys_pool::paths::resolve_provider_root`]): two rows with the
//! same provider column can legitimately come from different directories on
//! one host.
//!
//! # A pool that cannot be read is not an empty pool
//!
//! The states this view must keep distinguishable are the whole reason it is
//! not a plain account list:
//!
//! | Row state | Meaning |
//! |---|---|
//! | `selectable` | usable for the next spawn |
//! | `disabled` | operator ran `api-keys disable` |
//! | `exhausted` | an active bad-mark (rate/allowance), until its reset horizon |
//! | `unusable` | the account file is malformed/unreadable |
//! | `withheld` | a state file (`.disabled`/`.bad_accounts.json`) could not be read, so eligibility is unknown |
//! | `unreadable` | the provider directory (or a whole pool root) exists but could not be read |
//!
//! The last two exist because the spawn path fails **closed** on them; a view
//! that collapsed either into "no accounts" would tell the operator the
//! opposite of what the spawn path is about to do.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use tokio::net::TcpStream;

use crate::api_keys_pool::{self, ApiKeyAccount, Ineligible, ProviderHealth};

/// Path the API-key pool account endpoint answers (Issue #8447).
pub const API_KEYS_PATH: &str = "/api/api-keys";

/// Row state words — see the module docs' table.
pub const STATE_SELECTABLE: &str = "selectable";
pub const STATE_DISABLED: &str = "disabled";
pub const STATE_EXHAUSTED: &str = "exhausted";
pub const STATE_UNUSABLE: &str = "unusable";
pub const STATE_WITHHELD: &str = "withheld";
pub const STATE_UNREADABLE: &str = "unreadable";

/// The provider column's value for a row describing a whole pool **root** that
/// could not be enumerated — no single provider owns that failure, and every
/// provider under that root is unknown, not absent.
pub const ALL_PROVIDERS: &str = "*";

/// One API-key pool account (or one unreadable directory) as the dashboard
/// renders it. Secret-free by construction: every field is copied from
/// [`ApiKeyAccount`]/[`ProviderHealth`], neither of which can hold key
/// material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyAccountRow {
    /// Managed workspace this pool applies to (roots resolve per workspace).
    pub workspace: String,
    /// The pool's provider namespace (`zai`, …), or [`ALL_PROVIDERS`] for an
    /// unreadable pool root.
    pub provider: String,
    /// The account's **name**. `None` for a directory-level row, where there
    /// is no account to name because nothing could be listed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Eligibility state — see the module docs' table.
    pub state: String,
    /// The provider directory this row was resolved from.
    pub dir: String,
    /// Operator-facing detail (`problem` / the pool read error). Never file
    /// contents — see [`ApiKeyAccount::problem`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// `true` when the account file is looser than `0600`.
    pub insecure_permissions: bool,
}

/// The state word for one described account.
#[must_use]
pub fn account_state(account: &ApiKeyAccount) -> &'static str {
    match account.ineligible {
        None => STATE_SELECTABLE,
        Some(Ineligible::Disabled) => STATE_DISABLED,
        Some(Ineligible::Exhausted) => STATE_EXHAUSTED,
        Some(Ineligible::Malformed) => STATE_UNUSABLE,
        Some(Ineligible::Unverifiable) => STATE_WITHHELD,
    }
}

/// Flatten one provider's health snapshot into rows.
///
/// An unreadable provider directory yields exactly one `unreadable` row rather
/// than the zero rows its (necessarily empty) account list would produce —
/// [`ProviderHealth::unreadable`]'s counts mean "unknown", not "none".
#[must_use]
fn rows_from_health(workspace: &str, health: &ProviderHealth) -> Vec<ApiKeyAccountRow> {
    let dir = health.dir.display().to_string();
    if let Some(problem) = &health.unreadable {
        return vec![ApiKeyAccountRow {
            workspace: workspace.to_string(),
            provider: health.provider.clone(),
            account: None,
            state: STATE_UNREADABLE.to_string(),
            dir,
            detail: Some(problem.clone()),
            insecure_permissions: false,
        }];
    }
    health
        .accounts
        .iter()
        .map(|account| ApiKeyAccountRow {
            workspace: workspace.to_string(),
            provider: account.provider.clone(),
            account: Some(account.name.clone()),
            state: account_state(account).to_string(),
            dir: dir.clone(),
            detail: account.problem.clone(),
            insecure_permissions: !account.permissions_ok,
        })
        .collect()
}

/// Every API-key pool row visible from one managed workspace.
///
/// A pool **root** that cannot be enumerated at all (as opposed to a single
/// provider directory) is reported as one [`ALL_PROVIDERS`] row for the same
/// reason: an error here must never render as an empty pool.
#[must_use]
pub fn rows_for_workspace(workspace: &Path) -> Vec<ApiKeyAccountRow> {
    let label = workspace.display().to_string();
    match api_keys_pool::health(workspace, None) {
        Ok(snapshot) => snapshot
            .iter()
            .flat_map(|health| rows_from_health(&label, health))
            .collect(),
        Err(e) => vec![ApiKeyAccountRow {
            workspace: label,
            provider: ALL_PROVIDERS.to_string(),
            account: None,
            state: STATE_UNREADABLE.to_string(),
            dir: e.path.display().to_string(),
            detail: Some(e.to_string()),
            insecure_permissions: false,
        }],
    }
}

/// [`rows_for_workspace`] across every managed workspace the live status
/// report names, de-duplicated by root (a repo registered twice under
/// different spellings would otherwise double every row).
#[must_use]
pub fn rows_for_workspaces(roots: &[PathBuf]) -> Vec<ApiKeyAccountRow> {
    let mut seen: Vec<&PathBuf> = Vec::with_capacity(roots.len());
    let mut rows = Vec::new();
    for root in roots {
        if seen.contains(&root) {
            continue;
        }
        seen.push(root);
        rows.extend(rows_for_workspace(root));
    }
    rows
}

/// Serve `GET /api/api-keys`: the API-key pool account rows for every managed
/// repo the live status report names.
///
/// Deliberately local filesystem reads only (the same choice
/// [`super::read_token_rows`] makes): no provider network probe, nothing that
/// could stall a page polling on a dashboard cadence. An unreachable daemon
/// degrades to 503, matching the sibling endpoints; zero managed repos yields
/// `[]`.
pub(super) async fn handle(stream: &mut TcpStream, socket_path: &Path) -> Result<()> {
    let report = match super::fetch_report(socket_path).await {
        Ok(r) => r,
        Err(e) => {
            let body = serde_json::json!({ "error": format!("daemon unreachable: {e}") });
            return super::write_json_response(
                stream,
                "503 Service Unavailable",
                &body.to_string(),
            )
            .await;
        }
    };
    let roots: Vec<PathBuf> = report.per_repo.iter().map(|r| r.root.clone()).collect();
    let rows = rows_for_workspaces(&roots);
    let body = serde_json::to_string(&rows)?;
    super::write_json_response(stream, "200 OK", &body).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::api_keys_pool::{bad_marks, paths, registry};
    use std::net::Ipv4Addr;
    use tokio::net::TcpListener;

    /// A workspace whose per-repo pool holds one `loomtest/alpha` account.
    /// `shared_api_keys_dir()` refuses the machine-level fallback under
    /// `cfg(test)`, so a fixture can never see the operator's real pool.
    fn workspace_with_account(name: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = paths::per_repo_api_keys_dir(tmp.path());
        registry::add(&root, "loomtest", name, "LOOM_TEST_KEY_8447", "fake-secret", false).unwrap();
        tmp
    }

    fn state_of(rows: &[ApiKeyAccountRow], account: &str) -> String {
        rows.iter()
            .find(|r| r.account.as_deref() == Some(account))
            .unwrap_or_else(|| panic!("no row for {account}: {rows:?}"))
            .state
            .clone()
    }

    #[test]
    fn a_registered_account_renders_with_its_provider_and_selectable_state() {
        let tmp = workspace_with_account("alpha");
        let rows = rows_for_workspace(tmp.path());
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].provider, "loomtest");
        assert_eq!(rows[0].account.as_deref(), Some("alpha"));
        assert_eq!(rows[0].state, STATE_SELECTABLE);
        assert!(rows[0].dir.ends_with("loomtest"), "{rows:?}");
        assert!(!rows[0].insecure_permissions);
    }

    #[test]
    fn no_key_material_reaches_the_rendered_rows() {
        let tmp = workspace_with_account("alpha");
        let rendered = serde_json::to_string(&rows_for_workspace(tmp.path())).unwrap();
        assert!(!rendered.contains("fake-secret"), "{rendered}");
    }

    #[test]
    fn a_disabled_account_renders_disabled_rather_than_vanishing() {
        let tmp = workspace_with_account("alpha");
        let root = paths::per_repo_api_keys_dir(tmp.path());
        registry::set_enabled(&root, "loomtest", "alpha", false).unwrap();
        let rows = rows_for_workspace(tmp.path());
        assert_eq!(state_of(&rows, "alpha"), STATE_DISABLED);
    }

    #[test]
    fn a_bad_marked_account_renders_exhausted_with_its_reason() {
        let tmp = workspace_with_account("alpha");
        let root = paths::per_repo_api_keys_dir(tmp.path());
        bad_marks::mark_bad(&root, "loomtest", "alpha", "429 from provider", None).unwrap();
        let rows = rows_for_workspace(tmp.path());
        assert_eq!(state_of(&rows, "alpha"), STATE_EXHAUSTED);
        let detail = rows[0]
            .detail
            .clone()
            .expect("exhausted rows carry a reason");
        assert!(detail.contains("429 from provider"), "{detail}");
    }

    #[test]
    fn a_malformed_account_file_renders_unusable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = paths::provider_dir(&paths::per_repo_api_keys_dir(tmp.path()), "loomtest");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken.env"), "not an assignment\n").unwrap();
        let rows = rows_for_workspace(tmp.path());
        assert_eq!(state_of(&rows, "broken"), STATE_UNUSABLE);
    }

    /// An unreadable `.disabled` withholds the account rather than freeing it
    /// — the view has to show that, not "selectable" and not "no accounts".
    #[cfg(unix)]
    #[test]
    fn an_unverifiable_account_renders_withheld() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = workspace_with_account("alpha");
        let dir = paths::provider_dir(&paths::per_repo_api_keys_dir(tmp.path()), "loomtest");
        let disabled = dir.join(registry::DISABLED_FILE);
        std::fs::write(&disabled, "someone-else\n").unwrap();
        std::fs::set_permissions(&disabled, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable_as_root = std::fs::read_to_string(&disabled).is_ok();
        let rows = rows_for_workspace(tmp.path());
        std::fs::set_permissions(&disabled, std::fs::Permissions::from_mode(0o600)).unwrap();
        if readable_as_root {
            return; // permission bits do not apply to this uid
        }
        assert_eq!(state_of(&rows, "alpha"), STATE_WITHHELD);
    }

    /// AC3: an unreadable provider directory is shown as unreadable, NOT as an
    /// empty pool.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_provider_directory_renders_unreadable_not_an_empty_pool() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = workspace_with_account("alpha");
        let dir = paths::provider_dir(&paths::per_repo_api_keys_dir(tmp.path()), "loomtest");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable_as_root = std::fs::read_dir(&dir).is_ok();
        let rows = rows_for_workspace(tmp.path());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if readable_as_root {
            return; // permission bits do not apply to this uid
        }
        assert_eq!(rows.len(), 1, "an unreadable provider must still render a row: {rows:?}");
        assert_eq!(rows[0].provider, "loomtest");
        assert_eq!(rows[0].state, STATE_UNREADABLE);
        assert_eq!(rows[0].account, None);
        assert!(rows[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("cannot be read"));
    }

    #[test]
    fn a_workspace_with_no_pool_contributes_no_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(rows_for_workspace(tmp.path()).is_empty());
    }

    #[test]
    fn duplicate_workspace_roots_are_not_double_counted() {
        let tmp = workspace_with_account("alpha");
        let roots = vec![tmp.path().to_path_buf(), tmp.path().to_path_buf()];
        assert_eq!(rows_for_workspaces(&roots).len(), 1);
    }

    #[test]
    fn the_route_is_registered_for_dispatch() {
        assert!(
            super::super::KNOWN_PATHS.contains(&API_KEYS_PATH),
            "the endpoint must be dispatchable, not a 404"
        );
    }

    #[tokio::test]
    async fn http_get_api_keys_lists_the_managed_repos_pool_accounts() {
        let tmp = workspace_with_account("alpha");
        let report = super::super::tests::report_with_repo_root(tmp.path());
        let socket_path = super::super::tests::spawn_fake_daemon_socket(report).await;
        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind tcp");
        let addr = tcp_listener.local_addr().expect("local addr");
        let server_task = tokio::spawn(super::super::run(tcp_listener, socket_path));

        let (status, body) = super::super::tests::http_get(addr, API_KEYS_PATH).await;
        assert_eq!(status, 200);
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(value[0]["provider"], serde_json::json!("loomtest"));
        assert_eq!(value[0]["account"], serde_json::json!("alpha"));
        assert_eq!(value[0]["state"], serde_json::json!(STATE_SELECTABLE));
        assert!(!body.contains("fake-secret"), "{body}");

        server_task.abort();
    }

    #[tokio::test]
    async fn http_get_api_keys_with_no_managed_repos_is_an_empty_array() {
        let socket_path =
            super::super::tests::spawn_fake_daemon_socket(super::super::tests::empty_report())
                .await;
        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind tcp");
        let addr = tcp_listener.local_addr().expect("local addr");
        let server_task = tokio::spawn(super::super::run(tcp_listener, socket_path));

        let (status, body) = super::super::tests::http_get(addr, API_KEYS_PATH).await;
        assert_eq!(status, 200);
        assert_eq!(body, "[]");

        server_task.abort();
    }
}
