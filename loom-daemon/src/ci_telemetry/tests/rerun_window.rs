//! The trailing rescan window (Issue #8898) — #8824 AC3's re-run gap.
//!
//! A re-run keeps the ORIGINAL run's `created_at`, so a `created >= watermark`
//! listing stops showing a run as soon as newer runs advance the watermark
//! past it, and a later re-attempt of it was silently never exported. These
//! tests are the only ones whose fixture **honours** `created=` — the parent
//! suite's [`super::strip_created`] deliberately ignores it, which is exactly
//! what hid this gap.
//!
//! A sibling module of `super` rather than more lines in it: `tests.rs` is near
//! the file-size ratchet, and the created-filter-honouring harness is a
//! self-contained concern (the same split #8825's [`super::job_logs`] made).

use super::*;
use crate::ci_telemetry::poll::runs_floor;

/// The recorded fixture, but runs listings honour their `created=>=` floor,
/// the way live GitHub does.
///
/// A wrapper over [`FixtureApi`] rather than a flag inside it: the created
/// filter is only ever wanted here, and every other test's fixture keys stay
/// exactly as they were.
struct CreatedFiltering {
    fixture: FixtureApi,
}

impl CreatedFiltering {
    fn new() -> Self {
        CreatedFiltering {
            fixture: FixtureApi::new(),
        }
    }

    /// The `created=>=<ts>` floor a runs-listing path carries, if any. Only a
    /// path carrying the parameter is filtered; the fixture's `next` links do
    /// not repeat it, so a paginated listing is filtered on page 1 only.
    fn floor(path: &str) -> Option<DateTime<Utc>> {
        let (_, query) = path.split_once('?')?;
        let value = query.split('&').find_map(|p| p.strip_prefix("created="))?;
        let ts = value
            .strip_prefix("%3E%3D")
            .or_else(|| value.strip_prefix(">="))?;
        ts.parse().ok()
    }

    /// Drop runs created before `floor` from a runs-listing body — what live
    /// GitHub does with `created=>=`, and what [`super::strip_created`]
    /// deliberately does not.
    fn filtered(body: &str, floor: DateTime<Utc>) -> String {
        let mut parsed: Value = serde_json::from_str(body).unwrap();
        let Some(runs) = parsed
            .get_mut("workflow_runs")
            .and_then(Value::as_array_mut)
        else {
            return body.to_string();
        };
        runs.retain(|run| {
            run["created_at"]
                .as_str()
                .and_then(|ts| ts.parse::<DateTime<Utc>>().ok())
                .is_none_or(|created| created >= floor)
        });
        parsed.to_string()
    }
}

impl GithubApi for CreatedFiltering {
    fn get(&self, path: &str, etag: Option<&str>) -> Result<ApiResponse, ApiError> {
        let mut response = self.fixture.get(path, etag)?;
        if let Some(floor) = Self::floor(path) {
            response.body = Self::filtered(&response.body, floor);
        }
        Ok(response)
    }

    fn get_document(&self, path: &str) -> Result<ApiResponse, ApiError> {
        self.fixture.get_document(path)
    }
}

/// The floor is the watermark or the trailing rescan window, whichever is
/// older — and the window is capped by the initial lookback so it can never
/// reach further back than a repo's first cycle already did.
#[test]
fn the_runs_floor_is_the_older_of_the_watermark_and_the_rescan_window() {
    let t = now();
    let day = Duration::hours(24);
    // No watermark yet: the initial lookback, rescan window irrelevant.
    assert_eq!(runs_floor(None, t, day, Duration::hours(6)), t - day);
    // A watermark newer than the window: the window widens the listing.
    let recent: DateTime<Utc> = "2026-09-20T11:00:00Z".parse().unwrap();
    assert_eq!(runs_floor(Some(recent), t, day, Duration::hours(6)), t - Duration::hours(6));
    // A watermark older than the window (a quiet repo, or one held back by an
    // unfinished run): unchanged from the pre-#8898 floor.
    let old: DateTime<Utc> = "2026-09-01T00:00:00Z".parse().unwrap();
    assert_eq!(runs_floor(Some(old), t, day, Duration::hours(6)), old);
    // A window wider than the initial lookback is capped by it.
    assert_eq!(runs_floor(Some(recent), t, day, Duration::hours(240)), t - day);
}

/// The fixture really does honour `created=>=` — without this, the test below
/// would pass even with the bug still in place (which is what happened to
/// `a_rerun_attempt_…` in the parent suite).
#[test]
fn the_created_filtering_fixture_omits_runs_below_the_floor() {
    let api = CreatedFiltering::new();
    let runs_key = format!("repos/{ORG}/beta/actions/runs?per_page=100");
    let unfiltered: RunsPage =
        serde_json::from_str(&api.get(&runs_key, None).unwrap().body).unwrap();
    assert!(unfiltered.workflow_runs.iter().any(|run| run.id == 2001));
    // Run 2001 was created at 09:00, so a 10:00 floor must drop it.
    let body = api
        .get(&format!("{runs_key}&created=%3E%3D2026-09-20T10:00:00Z"), None)
        .unwrap()
        .body;
    let filtered: RunsPage = serde_json::from_str(&body).unwrap();
    assert!(!filtered.workflow_runs.is_empty());
    assert!(filtered.workflow_runs.iter().all(|run| run.id != 2001));
}

/// AC3 of #8824: a re-attempt of a run the watermark has already passed is
/// still exported, exactly once — the trailing rescan window lists it again
/// and the ledger's `(repo, run_id, job_id, attempt)` dedup does the rest.
#[test]
fn a_late_rerun_of_a_run_older_than_the_watermark_is_exported_exactly_once() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &CreatedFiltering::new()).unwrap();
    assert_eq!(kind_counts(dir.path()), (6, 24, 30, 30));
    let watermark = Ledger::open_read_only(state_dir(dir.path()).join("seen.jsonl"))
        .unwrap()
        .watermark(&format!("{ORG}/beta"));
    assert_eq!(watermark, Some("2026-09-20T11:00:00Z".parse::<DateTime<Utc>>().unwrap()));

    // Beta run 2001 was created at 09:00 — two hours BEFORE that watermark —
    // and is re-run now. Only `run_attempt`/`conclusion` change; `created_at`
    // stays at the original attempt's, exactly as GitHub reports it.
    let runs_key = format!("repos/{ORG}/beta/actions/runs?per_page=100");
    let jobs_key = format!("repos/{ORG}/beta/actions/runs/2001/jobs?filter=all&per_page=100");
    let rerun = |api: &CreatedFiltering| {
        api.fixture.edit(&runs_key, |entry| {
            let runs = entry["body"]["workflow_runs"].as_array_mut().unwrap();
            let old = runs.iter_mut().find(|run| run["id"] == 2001).unwrap();
            assert_eq!(old["created_at"], Value::from("2026-09-20T09:00:00Z"));
            old["run_attempt"] = Value::from(2);
            old["conclusion"] = Value::from("success");
        });
        api.fixture.edit(&jobs_key, |entry| {
            let mut retry = entry["body"]["jobs"][0].clone();
            retry["id"] = Value::from(20015);
            retry["run_attempt"] = Value::from(2);
            retry["conclusion"] = Value::from("success");
            entry["body"]["jobs"].as_array_mut().unwrap().push(retry);
        });
    };

    // Half an hour later: the watermark is still two hours newer than run
    // 2001's `created_at`, so only the rescan window can surface it.
    let mut later = ctx(dir.path());
    later.now = now() + Duration::minutes(30);

    // Pre-#8898 behaviour, reproduced exactly by a zero-width window (the
    // floor is then the watermark alone): the re-attempt is never listed and
    // is silently never exported. This is the bug, asserted.
    let pre_fix = CycleContext {
        rescan_window: Duration::zero(),
        ..later.clone()
    };
    let pre_fix_api = CreatedFiltering::new();
    rerun(&pre_fix_api);
    let dropped = run_cycle(&pre_fix, &pre_fix_api).unwrap();
    assert_eq!(
        (dropped.summary.runs_emitted, dropped.summary.jobs_emitted),
        (0, 0),
        "a watermark-only floor must miss the re-attempt (the #8898 gap)"
    );

    let api = CreatedFiltering::new();
    rerun(&api);
    let report = run_cycle(&later, &api).unwrap();
    assert_eq!(
        (report.summary.runs_emitted, report.summary.jobs_emitted),
        (1, 1),
        "the late re-attempt of run 2001 must be exported"
    );
    assert!(
        api.fixture
            .requests()
            .iter()
            .any(|(p, _)| p.starts_with(&format!("repos/{ORG}/beta/actions/runs?"))
                && p.contains("created=%3E%3D2026-09-19T12:30:00Z")),
        "the listing floor must be the trailing rescan window: {:?}",
        api.fixture.requests()
    );
    assert_no_duplicates(dir.path());
    assert_eq!(kind_counts(dir.path()), (7, 25, 32, 32));
    // Exactly once: a further cycle re-lists the same window and emits nothing.
    let repeat = run_cycle(&later, &api).unwrap();
    assert_eq!((repeat.summary.runs_emitted, repeat.summary.jobs_emitted), (0, 0));
    assert_eq!(kind_counts(dir.path()), (7, 25, 32, 32));
    // The watermark never regressed to the re-listed run's `created_at`.
    assert_eq!(
        Ledger::open_read_only(state_dir(dir.path()).join("seen.jsonl"))
            .unwrap()
            .watermark(&format!("{ORG}/beta")),
        watermark
    );
}
