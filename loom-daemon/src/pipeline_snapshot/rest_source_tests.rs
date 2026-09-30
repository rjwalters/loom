//! Tests for the REST + ETag pipeline fetch (Issue #9253).
//!
//! Every test here is `#[serial]`: it sets the process-global
//! `LOOM_LISTING_CACHE_DIR` (as `forge_listing`'s own disk-cache tests do) and
//! spawns a fake `gh`, whose spawn reads the process environment (#4547).

use super::super::{GhPipelineSource, PipelineMetrics, PipelineSource};
use serial_test::serial;
use std::path::{Path, PathBuf};

/// A fake `gh` driven by fixture files under `dir`, logging each argv line to
/// `dir/calls.log`:
///
/// - `gh api --include …issues?labels=<L>&…` answers `304` when an
///   `If-None-Match` is presented, else `200` + an ETag and the body of
///   `rest/<L>.json` (`[]` when absent) — or fails when `fail/<L>` exists.
/// - GraphQL `gh pr list … mergeable,createdAt` / `--state merged` /
///   `--search is:open label:loom:issue` answer `gql/mergeable.json`,
///   `gql/merged.json`, `gql/queued.json` (`[]` when absent).
struct FakeGh {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    gh: PathBuf,
}

impl FakeGh {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        for sub in ["rest", "gql", "fail", "cache"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::env::set_var("LOOM_LISTING_CACHE_DIR", dir.join("cache"));
        let gh = dir.join("fake-gh.sh");
        let script = format!(
            r#"#!/bin/sh
D='{d}'
echo "$*" >> "$D/calls.log"
serve() {{ if [ -f "$1" ]; then cat "$1"; else echo '[]'; fi; }}
case "$*" in
  api*)
    label=$(echo "$*" | sed -n 's/.*labels=\([^&]*\)&.*/\1/p')
    if [ -f "$D/fail/$label" ]; then echo "boom: not authenticated" 1>&2; exit 1; fi
    case "$*" in
      *If-None-Match*)
        printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
        echo 'gh: Not Modified (HTTP 304)' 1>&2
        exit 1
        ;;
    esac
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"%s"\r\n\r\n' "$label"
    serve "$D/rest/$label.json"
    ;;
  *mergeable,createdAt*) serve "$D/gql/mergeable.json" ;;
  *"--state merged"*) serve "$D/gql/merged.json" ;;
  *"--search is:open label:loom:issue"*) serve "$D/gql/queued.json" ;;
  *) echo '[]' ;;
esac
"#,
            d = dir.display()
        );
        std::fs::write(&gh, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { _tmp: tmp, dir, gh }
    }

    fn rest(&self, label: &str, rows: &[serde_json::Value]) {
        let body = serde_json::to_string(rows).unwrap();
        std::fs::write(self.dir.join("rest").join(format!("{label}.json")), body).unwrap();
    }

    fn gql(&self, name: &str, body: &str) {
        std::fs::write(self.dir.join("gql").join(format!("{name}.json")), body).unwrap();
    }

    fn fail(&self, label: &str) {
        std::fs::write(self.dir.join("fail").join(label), "").unwrap();
    }

    fn source(&self) -> GhPipelineSource {
        GhPipelineSource::new().with_gh_bin(self.gh.clone())
    }

    fn root(&self) -> &Path {
        &self.dir
    }

    /// Logged argv lines, drained so each test phase sees only its own calls.
    fn take_calls(&self) -> Vec<String> {
        let path = self.dir.join("calls.log");
        let log = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        log.lines().map(str::to_string).collect()
    }
}

impl Drop for FakeGh {
    fn drop(&mut self) {
        std::env::remove_var("LOOM_LISTING_CACHE_DIR");
    }
}

fn row(number: u32, labels: &[&str], pr: bool) -> serde_json::Value {
    let labels: Vec<_> = labels
        .iter()
        .map(|l| serde_json::json!({ "name": l }))
        .collect();
    let mut v = serde_json::json!({
        "number": number,
        "state": "open",
        "labels": labels,
        "created_at": "2026-07-01T00:00:00Z",
    });
    if pr {
        v["pull_request"] = serde_json::json!({});
    }
    v
}

/// The fixture the pre-#9253 `gh_pipeline_source_counts_each_metric_independently`
/// asserted against, re-expressed as REST listings (plus PR/issue and
/// park/claim rows the REST path must filter client-side).
fn seed_full_fixture(fake: &FakeGh) {
    fake.rest(
        "loom:issue",
        &[
            row(1, &["loom:issue"], false),
            row(2, &["loom:issue"], false),
            row(3, &["loom:issue"], false),
            row(20, &["loom:issue", "loom:blocked"], false),
            row(21, &["loom:issue", "loom:operator-only"], false),
        ],
    );
    fake.rest(
        "loom:building",
        &[
            row(4, &["loom:building"], false),
            row(40, &["loom:building"], true),
        ],
    );
    fake.rest(
        "loom:review-requested",
        &[
            row(5, &["loom:review-requested"], true),
            row(6, &["loom:review-requested"], true),
        ],
    );
    fake.rest(
        "loom:changes-requested",
        &[
            row(12, &["loom:changes-requested"], true),
            row(16, &["loom:changes-requested", "loom:treating"], true),
            row(17, &["loom:changes-requested", "loom:blocked"], true),
        ],
    );
    fake.rest("loom:pr", &[row(7, &["loom:pr"], true)]);
    fake.rest(
        "loom:operator",
        &[
            row(13, &["loom:operator"], true),
            row(14, &["loom:operator"], true),
        ],
    );
    fake.rest("loom:operator-only", &[row(15, &["loom:operator-only"], false)]);
    fake.gql("merged", r#"[{"number":8},{"number":9},{"number":10},{"number":11}]"#);
    fake.gql(
        "mergeable",
        r#"[{"number":13,"mergeable":"CONFLICTING","createdAt":"2026-07-01T00:00:00Z"},{"number":14,"mergeable":"MERGEABLE","createdAt":"2026-08-01T00:00:00Z"}]"#,
    );
}

/// AC1: counts match the old path's for the same fixture; a second fetch of
/// an unchanged root sends only conditional (`If-None-Match`) listings, all
/// answered `304`, and returns identical counts. The only non-REST calls are
/// the merged search and — because a held PR exists — the `mergeable` query.
#[test]
#[serial]
fn rest_counts_match_and_an_unchanged_root_is_served_by_304s() {
    let fake = FakeGh::new();
    seed_full_fixture(&fake);
    let source = fake.source();

    let first = source.fetch(fake.root());
    assert_eq!(first.queued, Some(3), "park-labeled rows excluded (#4825)");
    assert_eq!(first.building, Some(1), "PR rows filtered from an issue metric");
    assert_eq!(first.review_requested, Some(2));
    assert_eq!(first.changes_requested, Some(3));
    assert_eq!(first.changes_requested_unclaimed, Some(1), "#5272");
    assert_eq!(first.approved, Some(1));
    assert_eq!(first.merged_24h, Some(4));
    assert_eq!(first.operator_held, Some(2));
    assert_eq!(first.operator_held_conflicting, Some(1));
    assert!(first.operator_held_oldest_days.is_some_and(|d| d > 0));
    assert_eq!(first.operator_only_issues, Some(1));
    assert!(first.is_complete(), "{:?}", first.error);

    let calls = fake.take_calls();
    let rest: Vec<_> = calls.iter().filter(|c| c.starts_with("api ")).collect();
    assert_eq!(rest.len(), 7, "one listing per label: {calls:#?}");
    assert_eq!(calls.len(), 9, "7 listings + merged + mergeable: {calls:#?}");

    let second = source.fetch(fake.root());
    assert_eq!(second, first, "a 304 must reproduce the identical snapshot");
    let calls = fake.take_calls();
    let rest: Vec<_> = calls.iter().filter(|c| c.starts_with("api ")).collect();
    assert_eq!(rest.len(), 7, "{calls:#?}");
    assert!(
        rest.iter().all(|c| c.contains("If-None-Match")),
        "every listing on an unchanged root must be conditional: {calls:#?}"
    );
    let billable = calls.len() - rest.len();
    assert!(billable <= 2, "≤ ~2 billable calls per unchanged root: {calls:#?}");
}

/// AC1: with no `loom:operator` rows, the `mergeable` GraphQL query is never
/// made — the only non-`304` call on an unchanged root is the merged search.
#[test]
#[serial]
fn no_mergeable_query_when_nothing_is_operator_held() {
    let fake = FakeGh::new();
    fake.rest("loom:issue", &[row(1, &["loom:issue"], false)]);
    let source = fake.source();

    let snap = source.fetch(fake.root());
    assert_eq!(snap.queued, Some(1));
    assert_eq!(snap.operator_held, Some(0));
    assert_eq!(snap.operator_held_conflicting, Some(0));
    assert_eq!(snap.operator_held_oldest_days, None, "no held PRs is not a failure");
    assert!(snap.is_complete());
    let _ = fake.take_calls();

    let _ = source.fetch(fake.root());
    let calls = fake.take_calls();
    assert!(!calls.iter().any(|c| c.contains("mergeable")), "{calls:#?}");
    assert!(!calls.iter().any(|c| c.starts_with("issue list")), "{calls:#?}");
    let gql: Vec<_> = calls.iter().filter(|c| c.starts_with("pr list")).collect();
    assert_eq!(gql.len(), 1, "only the merged search: {calls:#?}");
    assert!(gql[0].contains("--state merged"));
}

/// AC3: a full (possibly truncated) REST page must not be reported as a
/// capped 100 — that metric alone falls back to its GraphQL count.
#[test]
#[serial]
fn a_full_page_falls_back_to_graphql_for_that_metric_only() {
    let fake = FakeGh::new();
    let full: Vec<_> = (1..=100).map(|n| row(n, &["loom:issue"], false)).collect();
    fake.rest("loom:issue", &full);
    fake.rest("loom:pr", &[row(7, &["loom:pr"], true)]);
    let many: Vec<String> = (1..=150).map(|n| format!(r#"{{"number":{n}}}"#)).collect();
    fake.gql("queued", &format!("[{}]", many.join(",")));

    let snap = fake.source().fetch(fake.root());
    assert_eq!(snap.queued, Some(150), "the GraphQL count, not the capped page");
    assert_eq!(snap.approved, Some(1), "other metrics stay on REST");
    let calls = fake.take_calls();
    let search: Vec<_> = calls
        .iter()
        .filter(|c| c.contains("--search is:open"))
        .collect();
    assert_eq!(search.len(), 1, "{calls:#?}");
    assert!(
        search[0].contains("-label:loom:blocked"),
        "the fallback keeps the park exclusion"
    );
}

/// The partial-failure contract survives the REST path: one failed listing
/// leaves its metric `None` and records the error, the rest still resolve.
#[test]
#[serial]
fn a_failed_listing_keeps_the_other_metrics() {
    let fake = FakeGh::new();
    fake.fail("loom:issue");
    fake.fail("loom:operator");
    fake.rest("loom:building", &[row(1, &["loom:building"], false)]);

    let snap = fake.source().fetch(fake.root());
    assert_eq!(snap.queued, None);
    assert_eq!(snap.building, Some(1));
    assert_eq!(snap.merged_24h, Some(0));
    assert_eq!(snap.operator_held, None, "#8091: must not render 0 held on a failed read");
    assert_eq!(snap.operator_held_conflicting, None);
    assert!(!snap.is_complete());
    assert!(snap.error.as_deref().unwrap().contains("not authenticated"));
}

/// The #4761 mask still holds: HEALTH never lists `loom:building`, and every
/// axis the health sections read is fetched.
#[test]
#[serial]
fn health_mask_fetches_every_read_axis_but_not_building() {
    let fake = FakeGh::new();
    seed_full_fixture(&fake);
    let snap = fake
        .source()
        .with_metrics(PipelineMetrics::HEALTH)
        .fetch(fake.root());

    assert_eq!(snap.building, None, "no health section reads `building`");
    assert_eq!(snap.queued, Some(3));
    assert_eq!(snap.review_requested, Some(2));
    assert_eq!(snap.changes_requested_unclaimed, Some(1));
    assert_eq!(snap.approved, Some(1));
    assert_eq!(snap.merged_24h, Some(4));
    assert_eq!(snap.operator_held_conflicting, Some(1));
    assert_eq!(snap.operator_only_issues, Some(1));
    let calls = fake.take_calls();
    assert!(!calls.iter().any(|c| c.contains("loom:building")), "{calls:#?}");
    assert!(
        calls.iter().any(|c| c.contains("labels=loom:operator&")),
        "the held query lists loom:operator, not loom:operator-only: {calls:#?}"
    );
}
