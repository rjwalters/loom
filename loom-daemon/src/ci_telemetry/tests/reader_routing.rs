//! Reader routing for the poller's own `gh` calls: Link-header pages keep the
//! caller's repo, and `gh run download` goes reader → writer.
//!
//! Each test drives the production [`GhCliApi`] against a stub `gh` that logs
//! the `GH_CONFIG_DIR` it ran under, with the reader lookup and withdrawal
//! injected through [`GhCliApi::with_reader_seams`] (no daemon workspace).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::ci_telemetry::api::{ApiError, ApiResponse, GhCliApi, GithubApi};
use crate::ci_telemetry::poll::paginate;
use crate::forge_identity::{Failure, IdentityRole};
use crate::gh_invocation::TOKEN_ENV_VARS;

const REPO: &str = "acme/w";
const APP: &str = "reader-app-1";

type Withdrawals = Arc<Mutex<Vec<(String, String, Failure)>>>;

struct Stub {
    _tmp: tempfile::TempDir,
    reader: PathBuf,
    log: PathBuf,
    gh: PathBuf,
}

impl Stub {
    /// A stub `gh` that logs `GH_CONFIG_DIR` then runs `on_reader` under the
    /// reader's dir and `on_writer` under anything else. `$last` is its last
    /// argument (the API path for `gh api`).
    fn new(on_reader: &str, on_writer: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let reader = tmp.path().join("reader-dir");
        std::fs::create_dir_all(&reader).unwrap();
        let log = tmp.path().join("calls.log");
        let gh = tmp.path().join("gh-stub");
        let body = format!(
            "#!/bin/sh\nfor last; do :; done\necho \"${{GH_CONFIG_DIR:-}}\" >> '{log}'\n\
             if [ \"${{GH_CONFIG_DIR:-}}\" = '{r}' ]; then {on_reader}; else {on_writer}; fi\n",
            log = log.display(),
            r = reader.display(),
        );
        std::fs::write(&gh, body).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Stub {
            _tmp: tmp,
            reader,
            log,
            gh,
        }
    }

    /// A client whose reader for [`REPO`] (only) is this stub's reader dir.
    fn api(&self, with_reader: bool) -> (GhCliApi, Withdrawals) {
        let withdrawn: Withdrawals = Arc::default();
        let sink = Arc::clone(&withdrawn);
        let reader = self.reader.clone();
        let api = GhCliApi::with_reader_seams(
            self.gh.clone(),
            Box::new(move |repo| {
                (with_reader && repo == REPO).then(|| (reader.clone(), APP.to_string()))
            }),
            Box::new(move |app, repo, failure, _until, _why| {
                sink.lock()
                    .unwrap()
                    .push((app.to_string(), repo.to_string(), failure));
            }),
        );
        (api, withdrawn)
    }

    /// Whether each call ran under the reader dir, in order.
    fn calls(&self) -> Vec<bool> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|dir| Path::new(dir) == self.reader)
            .collect()
    }
}

/// Run `body` with THIS thread's call-stats sink at a fresh dir and return
/// the raw sink lines it wrote (`c` caller, `rp` repo, `ir` role, `op`).
fn sink_rows(body: impl FnOnce()) -> Vec<Value> {
    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    crate::forge_call_stats::set_test_sink_dir(None);
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(sink.path()).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        rows.extend(
            text.lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap()),
        );
    }
    rows
}

fn field<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

/// A `gh api --include` page whose `Link` header points at `next`, if any.
fn page(next: Option<&str>) -> String {
    let link = next.map_or(String::new(), |n| {
        format!("Link: <https://api.github.com/{n}>; rel=\\042next\\042\\r\\n")
    });
    format!(
        "printf 'HTTP/2.0 200 OK\\r\\nX-Ratelimit-Resource: core\\r\\n{link}\\r\\n{{\"items\":[1]}}'"
    )
}

#[test]
fn get_in_runs_a_page_link_under_the_named_repos_reader() {
    let stub = Stub::new(&page(None), "exit 9");
    let (api, withdrawn) = stub.api(true);
    let path = "repositories/123/actions/runs?page=2";

    // The invocation: the reader's dir, every token env var stripped.
    let inv = api.api_invocation(path, &[], Some(REPO), Some(&stub.reader), IdentityRole::Reader);
    let plan = inv.env_plan(None);
    assert!(
        plan.iter().any(
            |e| e.key == "GH_CONFIG_DIR" && e.value.as_deref() == Some(stub.reader.as_os_str())
        ),
        "{plan:?}"
    );
    for key in TOKEN_ENV_VARS {
        assert!(plan.iter().any(|e| e.key == key && e.value.is_none()), "{key}: {plan:?}");
    }

    let mut result = None;
    let rows = sink_rows(|| result = Some(api.get_in(Some(REPO), path, None)));
    assert_eq!(result.unwrap().unwrap().status, 200);
    assert_eq!(stub.calls(), vec![true]);
    assert!(withdrawn.lock().unwrap().is_empty());
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(field(&rows[0], "c"), Some("ci_telemetry"));
    assert_eq!(field(&rows[0], "rp"), Some(REPO));
    assert_eq!(field(&rows[0], "ir"), Some("reader"));
    // The page link is the same route as page 1, not `unknown`.
    assert_eq!(field(&rows[0], "op"), Some("ci.workflow-runs-for-sha"), "{rows:?}");
}

#[test]
fn paginate_reads_every_page_for_page_ones_repo() {
    // Page 1 names the repo; pages 2 and 3 are GitHub's `repositories/<id>`
    // spelling, which names none.
    let pages = format!(
        "case \"$last\" in *page=3*) {p3} ;; *page=2*) {p2} ;; *) {p1} ;; esac",
        p1 = page(Some("repositories/123/actions/runs?page=2")),
        p2 = page(Some("repositories/123/actions/runs?page=3")),
        p3 = page(None),
    );
    let stub = Stub::new(&pages, "exit 9");
    let (api, _) = stub.api(true);
    let mut requests = 0;
    let mut items = None;
    let rows = sink_rows(|| {
        items = Some(paginate(
            &api,
            None,
            "repos/acme/w/actions/runs?per_page=100".to_string(),
            &mut requests,
            |body| {
                serde_json::from_str::<Value>(body)
                    .map(|v| v["items"].as_array().cloned().unwrap_or_default())
            },
        ));
    });
    assert_eq!(items.unwrap().unwrap().len(), 3);
    assert_eq!(requests, 3);
    assert_eq!(stub.calls(), vec![true, true, true]);
    assert_eq!(rows.len(), 3, "{rows:?}");
    for row in &rows {
        assert_eq!(field(row, "rp"), Some(REPO), "{rows:?}");
        assert_eq!(field(row, "ir"), Some("reader"), "{rows:?}");
    }
}

/// A fake that predates `get_in`: only `get` (and the required
/// `get_document`). `paginate` must still serve it, unchanged.
struct GetOnly(Mutex<Vec<String>>);

impl GithubApi for GetOnly {
    fn get(&self, path: &str, _etag: Option<&str>) -> Result<ApiResponse, ApiError> {
        self.0.lock().unwrap().push(path.to_string());
        let next = path
            .ends_with("per_page=100")
            .then(|| "repositories/123/actions/runs?page=2".to_string());
        Ok(ApiResponse {
            status: 200,
            next,
            body: "[1, 2]".to_string(),
            ..ApiResponse::default()
        })
    }

    fn get_document(&self, path: &str) -> Result<ApiResponse, ApiError> {
        self.get(path, None)
    }
}

#[test]
fn a_fake_with_only_get_still_serves_paginate() {
    let fake = GetOnly(Mutex::default());
    let mut requests = 0;
    let items = paginate(
        &fake,
        Some(REPO),
        "repos/acme/w/actions/runs?per_page=100".to_string(),
        &mut requests,
        |body| serde_json::from_str::<Vec<u32>>(body),
    )
    .unwrap();
    assert_eq!(items, vec![1, 2, 1, 2]);
    assert_eq!(
        *fake.0.lock().unwrap(),
        vec![
            "repos/acme/w/actions/runs?per_page=100".to_string(),
            "repositories/123/actions/runs?page=2".to_string(),
        ]
    );
}

#[test]
fn owner_listings_stay_on_the_writer() {
    // A lookup that would hand out a reader for any repo: an owner listing
    // names none, so it never asks.
    let stub = Stub::new("exit 9", &page(None));
    let (api, _) = stub.api(true);
    let mut result = None;
    let rows = sink_rows(|| result = Some(api.get("orgs/acme/repos?per_page=100", None)));
    assert_eq!(result.unwrap().unwrap().status, 200);
    assert_eq!(stub.calls(), vec![false]);
    assert_eq!(field(&rows[0], "ir"), Some("writer"), "{rows:?}");
    assert_eq!(field(&rows[0], "rp"), None, "{rows:?}");
}

const OK: &str = "exit 0";
const NOT_COVERED: &str = "echo 'HTTP 403: Resource not accessible by integration' >&2; exit 1";
const RATE_LIMITED: &str = "echo 'HTTP 403: API rate limit exceeded for installation' >&2; exit 1";
const BAD_GATEWAY: &str = "echo 'HTTP 502: Bad Gateway' >&2; exit 1";

/// One download through `stub`, returning (result, download sink rows).
fn download(stub: &Stub, with_reader: bool) -> (Result<(), ApiError>, Vec<Value>, Withdrawals) {
    let (api, withdrawn) = stub.api(with_reader);
    let dest = stub.reader.with_file_name("dest");
    let mut result = None;
    let rows = sink_rows(|| result = Some(api.download_artifact(REPO, 7, "timings", &dest)));
    let rows = rows
        .into_iter()
        .filter(|r| field(r, "c") == Some("ci_telemetry.download"))
        .collect();
    (result.unwrap(), rows, withdrawn)
}

fn roles(rows: &[Value]) -> Vec<&str> {
    rows.iter().filter_map(|r| field(r, "ir")).collect()
}

#[test]
fn a_download_runs_under_the_reader() {
    let stub = Stub::new(OK, "exit 9");
    let (result, rows, withdrawn) = download(&stub, true);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stub.calls(), vec![true]);
    assert_eq!(roles(&rows), vec!["reader"]);
    assert_eq!(field(&rows[0], "rp"), Some(REPO));
    assert!(withdrawn.lock().unwrap().is_empty());
}

#[test]
fn a_download_outside_the_readers_installation_falls_back_and_withdraws_for_the_repo() {
    let stub = Stub::new(NOT_COVERED, OK);
    let (result, rows, withdrawn) = download(&stub, true);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stub.calls(), vec![true, false]);
    assert_eq!(roles(&rows), vec!["reader", "writer-fallback"]);
    assert_eq!(
        *withdrawn.lock().unwrap(),
        vec![(APP.to_string(), REPO.to_string(), Failure::Coverage)]
    );
}

#[test]
fn a_rate_limited_reader_download_falls_back_and_withdraws_the_app() {
    let stub = Stub::new(RATE_LIMITED, OK);
    let (result, rows, withdrawn) = download(&stub, true);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stub.calls(), vec![true, false]);
    assert_eq!(roles(&rows), vec!["reader", "writer-fallback"]);
    assert_eq!(
        *withdrawn.lock().unwrap(),
        vec![(APP.to_string(), REPO.to_string(), Failure::App)]
    );
}

#[test]
fn a_server_error_on_the_reader_download_is_not_retried() {
    let stub = Stub::new(BAD_GATEWAY, OK);
    let (result, rows, withdrawn) = download(&stub, true);
    assert!(matches!(result, Err(ApiError::Transport(_))), "{result:?}");
    assert_eq!(stub.calls(), vec![true]);
    assert_eq!(roles(&rows), vec!["reader"]);
    assert!(withdrawn.lock().unwrap().is_empty());
}

#[test]
fn a_download_without_a_reader_is_the_pre_reader_invocation() {
    let stub = Stub::new("exit 9", OK);
    let (api, _) = stub.api(false);
    let dest = stub.reader.with_file_name("dest");
    // The invocation exactly as it was built before reader routing.
    let before = crate::gh_invocation::GhInvocation::new(
        crate::gh_invocation::Operation::new("ci_telemetry.download"),
        crate::gh_invocation::AccessIntent::Read,
        crate::gh_invocation::GhTarget::None,
        std::time::Duration::from_secs(300),
    )
    .forge_op(crate::forge_call_stats::ops::CI_RUN_LOGS_AND_ARTIFACTS)
    .identity_scope(None, Some(REPO))
    .program(&stub.gh)
    .args(["run", "download"])
    .arg("7")
    .arg("--repo")
    .arg(REPO)
    .arg("--name")
    .arg("timings")
    .arg("--dir")
    .arg(&dest);
    let now = api.download_invocation(REPO, 7, "timings", &dest, None, None);
    assert_eq!(format!("{now:?}"), format!("{before:?}"));
    assert_eq!(now.env_plan(None), before.env_plan(None));

    let (result, rows, _) = download(&stub, false);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(stub.calls(), vec![false]);
    assert_eq!(roles(&rows), vec!["writer"]);
}
