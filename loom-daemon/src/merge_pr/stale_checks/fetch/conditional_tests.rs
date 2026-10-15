//! The surviving reads are conditional, paced and held on a
//! secondary limit (#11418), against a stub `gh` that speaks `--include`,
//! honours `If-None-Match` like the forge, and logs every request it gets as
//! `<status> <inm|-> <url>`.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use super::{branch_protection_contexts, budget, fetch_check_runs, fetch_required};
use crate::forge_wait_checks::reads::GhReads;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const SECONDARY: &str = r#"{"message":"You have exceeded a secondary rate limit. Please wait a few minutes before you try again."}"#;

struct Forge {
    dir: tempfile::TempDir,
    store: tempfile::TempDir,
    gh: PathBuf,
}

fn key(url: &str) -> String {
    url.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

impl Forge {
    /// A forge, with this thread's ETag store pointed at a fresh directory
    /// and the guard's pacing and hold forgotten.
    fn new() -> Self {
        budget::reset_for_test();
        let dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        crate::forge_etag_store::set_test_daemon_store_dir(Some(store.path().to_path_buf()));
        let gh = dir.path().join("gh");
        let d = dir.path().display();
        let script = format!(
            r#"#!/bin/sh
url=""; inm=""; prev=""; inc=""
for a in "$@"; do
  [ "$prev" = "-H" ] && inm="${{a#If-None-Match: }}"
  [ "$a" = "--include" ] && inc=1
  case "$a" in repos/*|graphql) [ -z "$url" ] && url="$a" ;; esac
  prev="$a"
done
k=$(printf '%s' "$url" | tr -c 'A-Za-z0-9' '_')
if [ -f '{d}'/"$k.raw" ]; then
  echo "RAW - $url" >> '{d}/calls.log'
  [ -n "$inc" ] && cat '{d}'/"$k.raw"
  cat '{d}'/"$k.err" 1>&2
  exit 1
fi
if [ -f '{d}'/"$k.out" ]; then
  echo "OUT - $url" >> '{d}/calls.log'
  cat '{d}'/"$k.out"
  exit 0
fi
if [ ! -f '{d}'/"$k.body" ]; then
  echo "404 - $url" >> '{d}/calls.log'
  printf 'HTTP/2.0 404 Not Found\r\n\r\n{{"message":"Not Found"}}'
  exit 1
fi
etag=$(cat '{d}'/"$k.etag")
if [ -n "$inm" ] && [ "$inm" = "$etag" ]; then
  echo "304 inm $url" >> '{d}/calls.log'
  printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
  exit 1
fi
if [ -n "$inm" ]; then t=inm; else t=-; fi
echo "200 $t $url" >> '{d}/calls.log'
printf 'HTTP/2.0 200 OK\r\nEtag: %s\r\n\r\n' "$etag"
cat '{d}'/"$k.body"
"#
        );
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, store, gh }
    }

    fn gh(&self) -> &str {
        self.gh.to_str().unwrap()
    }

    fn file(&self, url: &str, ext: &str) -> PathBuf {
        self.dir.path().join(format!("{}.{ext}", key(url)))
    }

    /// Serve `body` for a conditional read of `url`, with an ETag.
    fn serve(&self, url: &str, body: &str) {
        std::fs::write(self.file(url, "body"), body).unwrap();
        std::fs::write(self.file(url, "etag"), format!("W/\"{}\"", body.len())).unwrap();
    }

    /// Answer a plain (`--jq` / raw) read of `url` with `out`.
    fn out(&self, url: &str, out: &str) {
        std::fs::write(self.file(url, "out"), out).unwrap();
    }

    /// Refuse `url` with `status`, `headers` and a JSON `body`.
    fn refuse(&self, url: &str, status: u16, headers: &str, body: &str) {
        let raw = format!("HTTP/2.0 {status} Forbidden\r\n{headers}\r\n{body}");
        let msg = crate::forge_denial::body_message(body).unwrap_or_default();
        std::fs::write(self.file(url, "raw"), raw).unwrap();
        std::fs::write(self.file(url, "err"), format!("gh: {msg} (HTTP {status})\n")).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn store(&self) -> &Path {
        self.store.path()
    }
}

fn runs_url(sha: &str) -> String {
    format!("repos/o/r/commits/{sha}/check-runs?per_page=100")
}

/// `n` green Actions rows named `check-<offset..>`, reporting `total`.
fn page(total: usize, n: usize, offset: usize) -> String {
    let rows: Vec<String> = (offset..offset + n)
        .map(|i| {
            format!(
                r#"{{"name":"check-{i}","status":"completed","conclusion":"success","started_at":"2026-09-01T00:00:0{}Z","app":{{"slug":"github-actions"}},"details_url":"https://github.com/o/r/actions/runs/5/job/{i}","id":{i}}}"#,
                i % 10
            )
        })
        .collect();
    format!(r#"{{"total_count":{total},"check_runs":[{}]}}"#, rows.join(","))
}

// ---- check-runs: folding and the fail-closed short read (#8987) -----------

#[test]
fn every_page_of_a_multi_page_check_runs_read_is_folded_in() {
    let f = Forge::new();
    f.serve(&runs_url(SHA), &page(250, 100, 0));
    f.serve(&format!("{}&page=2", runs_url(SHA)), &page(250, 100, 100));
    f.serve(&format!("{}&page=3", runs_url(SHA)), &page(250, 50, 200));
    let runs = fetch_check_runs(f.gh(), "o/r", SHA).unwrap();
    assert_eq!(runs.len(), 250);
    let unique: std::collections::BTreeSet<&str> = runs.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(unique.len(), 250, "no page dropped or double-counted");
    assert_eq!(runs[7].actions_job_id, Some(7), "projection still parsed");
}

#[test]
fn a_short_check_runs_read_fails_closed_instead_of_returning_a_subset() {
    let f = Forge::new();
    f.serve(&runs_url(SHA), &page(39, 30, 0));
    let error = fetch_check_runs(f.gh(), "o/r", SHA).unwrap_err();
    assert!(error.contains("30 of total_count 39"), "{error}");
}

#[test]
fn an_empty_or_shapeless_check_runs_answer_is_not_zero_checks() {
    let f = Forge::new();
    f.serve(&runs_url(SHA), "");
    assert!(fetch_check_runs(f.gh(), "o/r", SHA).is_err());
    let f = Forge::new();
    f.serve(&runs_url(SHA), r#"{"total_count":0}"#);
    assert!(fetch_check_runs(f.gh(), "o/r", SHA).is_err());
    let f = Forge::new();
    f.serve(&runs_url(SHA), &page(0, 0, 0));
    assert!(fetch_check_runs(f.gh(), "o/r", SHA).unwrap().is_empty());
}

// ---- conditional reads ----------------------------------------------------

#[test]
fn a_repeated_check_runs_read_is_a_304_served_from_the_entry() {
    let f = Forge::new();
    f.serve(&runs_url(SHA), &page(2, 2, 0));
    let first = fetch_check_runs(f.gh(), "o/r", SHA).unwrap();
    let second = fetch_check_runs(f.gh(), "o/r", SHA).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        f.calls(),
        vec![
            format!("200 - {}", runs_url(SHA)),
            format!("304 inm {}", runs_url(SHA))
        ]
    );
}

#[test]
fn the_guard_revalidates_the_entry_forge_wait_checks_wrote() {
    let f = Forge::new();
    f.serve(&runs_url(SHA), &page(1, 1, 0));
    let mut wait = GhReads::new(f.gh.clone(), None, f.store().to_path_buf(), Some("o/r")).unwrap();
    wait.check_runs(SHA).unwrap();
    assert_eq!(fetch_check_runs(f.gh(), "o/r", SHA).unwrap().len(), 1);
    assert_eq!(f.calls()[1], format!("304 inm {}", runs_url(SHA)), "{:#?}", f.calls());
}

#[test]
fn the_rulesets_read_is_conditional_and_graphql_is_not() {
    let f = Forge::new();
    f.serve(
        "repos/o/r/rules/branches/main",
        r#"[{"type":"deletion"},{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"Backend"},{"context":"Docs"}]}}]"#,
    );
    f.out("graphql", "Docs\nLegacy\n");
    for _ in 0..2 {
        let (required, notices) = fetch_required(f.gh(), "o/r", "main", Some(f.store())).unwrap();
        assert_eq!(required, ["Backend", "Docs", "Legacy"]);
        assert!(notices.is_empty());
    }
    let calls = f.calls();
    assert_eq!(calls[2], "304 inm repos/o/r/rules/branches/main", "{calls:#?}");
    assert_eq!(calls.iter().filter(|c| c.ends_with(" graphql")).count(), 2);
}

#[test]
fn a_plan_gated_rulesets_403_still_reads_as_no_rules() {
    let f = Forge::new();
    f.refuse(
        "repos/o/r/rules/branches/main",
        403,
        "",
        r#"{"message":"Upgrade to GitHub Pro or make this repository public to enable this feature."}"#,
    );
    f.out("graphql", "");
    let (required, notices) = fetch_required(f.gh(), "o/r", "main", Some(f.store())).unwrap();
    assert!(required.is_empty());
    assert!(notices[0].contains("#8844"), "{notices:?}");
}

const NOT_ACCESSIBLE: &str = r#"{"message":"Resource not accessible by integration"}"#;

/// #11855: a read-only App token is refused the GraphQL `branchProtectionRule`
/// field (it needs `Administration: read`); the REST branch summary answers
/// the same question, so the lookup succeeds instead of failing closed on
/// every Judge pass.
#[test]
fn a_refused_graphql_classic_read_falls_back_to_the_rest_branch_summary() {
    let f = Forge::new();
    f.serve("repos/o/r/rules/branches/main", "[]");
    f.refuse("graphql", 403, "", NOT_ACCESSIBLE);
    f.serve(
        "repos/o/r/branches/main",
        r#"{"name":"main","protected":true,"protection":{"enabled":true,"required_status_checks":{"enforcement_level":"non_admins","contexts":["Rust (ubuntu-latest)","Docs"],"checks":[{"context":"Docs","app_id":1},{"context":"Package","app_id":1}]}}}"#,
    );
    let (required, notices) = fetch_required(f.gh(), "o/r", "main", Some(f.store())).unwrap();
    assert_eq!(required, ["Rust (ubuntu-latest)", "Docs", "Package"]);
    assert!(notices.is_empty(), "{notices:?}");
}

#[test]
fn a_refused_graphql_read_still_fails_closed_when_rest_cannot_answer() {
    let f = Forge::new();
    f.serve("repos/o/r/rules/branches/main", "[]");
    f.refuse("graphql", 403, "", NOT_ACCESSIBLE);
    // Protected, but the summary is withheld: unknown, never "nothing".
    f.serve("repos/o/r/branches/main", r#"{"name":"main","protected":true}"#);
    let err = fetch_required(f.gh(), "o/r", "main", Some(f.store())).unwrap_err();
    assert!(err.contains("classic branch-protection lookup failed"), "{err}");
    assert!(err.contains("withheld"), "{err}");
    // And a REST read that fails outright is no answer either.
    std::fs::remove_file(f.file("repos/o/r/branches/main", "body")).unwrap();
    let err = fetch_required(f.gh(), "o/r", "main", Some(f.store())).unwrap_err();
    assert!(err.contains("REST branch fallback"), "{err}");
}

#[test]
fn the_branch_summary_parser_reads_ruleset_only_and_unprotected_branches_as_none() {
    // A ruleset-only branch: protected, classic protection disabled.
    let ruleset_only = r#"{"protected":true,"protection":{"enabled":false,"required_status_checks":{"enforcement_level":"off","contexts":[],"checks":[]}}}"#;
    assert!(branch_protection_contexts(ruleset_only).unwrap().is_empty());
    assert!(branch_protection_contexts(r#"{"protected":false}"#)
        .unwrap()
        .is_empty());
    assert!(branch_protection_contexts(r#"{"name":"main"}"#).is_err());
    // A protection summary without `required_status_checks` is withheld,
    // not "no checks": GitHub always sends that key.
    let no_rsc = r#"{"protected":true,"protection":{"enabled":true}}"#;
    assert!(branch_protection_contexts(no_rsc).is_err());
    assert!(branch_protection_contexts("not json").is_err());
}

// ---- secondary limits and pacing -----------------------------------------

#[test]
fn a_secondary_limit_holds_every_later_request_until_retry_after() {
    let f = Forge::new();
    f.refuse(&runs_url(SHA), 403, "Retry-After: 120\r\n", SECONDARY);
    let error = fetch_check_runs(f.gh(), "o/r", SHA).unwrap_err();
    assert!(error.contains("secondary-rate-limit"), "{error}");
    let now = chrono::Utc::now().timestamp();
    let until = budget::hold_until(&budget::scope("o/r"), now).expect("a hold is in force");
    assert!((now + 115..=now + 121).contains(&until), "{until} vs {now}");

    // The next read is refused without spawning gh at all.
    let sent = f.calls().len();
    let error = fetch_required(f.gh(), "o/r", "main", Some(f.store())).unwrap_err();
    assert!(error.contains("#11418"), "{error}");
    assert_eq!(f.calls().len(), sent, "nothing was sent");

    // The hold is on disk, so another process on this host honours it too.
    budget::reset_for_test();
    assert_eq!(budget::hold_until(&budget::scope("o/r"), now), Some(until));
}

#[test]
fn a_secondary_limit_on_a_plain_read_holds_too() {
    let f = Forge::new();
    f.serve("repos/o/r/rules/branches/main", "[]");
    // The classic-protection read is a plain (GraphQL) `gh api` call.
    f.refuse("graphql", 403, "", SECONDARY);
    assert!(fetch_required(f.gh(), "o/r", "main", Some(f.store())).is_err());
    let now = chrono::Utc::now().timestamp();
    assert!(budget::hold_until(&budget::scope("o/r"), now).is_some_and(|t| t > now + 50));
    let sent = f.calls().len();
    assert!(fetch_required(f.gh(), "o/r", "main", Some(f.store())).is_err());
    assert_eq!(f.calls().len(), sent, "nothing more was asked for");
}

#[test]
fn a_primary_rate_limit_sets_no_secondary_hold() {
    let f = Forge::new();
    f.refuse(
        &runs_url(SHA),
        403,
        "X-Ratelimit-Remaining: 0\r\n",
        r#"{"message":"API rate limit exceeded for installation."}"#,
    );
    assert!(fetch_check_runs(f.gh(), "o/r", SHA).is_err());
    assert_eq!(budget::hold_until(&budget::scope("o/r"), chrono::Utc::now().timestamp()), None);
}

#[test]
fn back_to_back_requests_are_spaced() {
    let _f = Forge::new();
    budget::before_request("s").unwrap();
    budget::before_request("s").unwrap();
    let slept = budget::slept_for_test();
    assert_eq!(slept.len(), 1, "{slept:?}");
    assert!(
        slept[0] > budget::DEFAULT_GAP / 2 && slept[0] <= budget::DEFAULT_GAP,
        "{slept:?}"
    );
}

#[test]
fn a_hold_on_one_credential_or_owner_does_not_block_another() {
    let f = Forge::new();
    f.refuse(&runs_url(SHA), 403, "Retry-After: 120\r\n", SECONDARY);
    assert!(fetch_check_runs(f.gh(), "o/r", SHA).is_err());
    let now = chrono::Utc::now().timestamp();
    assert!(budget::hold_until(&budget::scope("o/r"), now).is_some());

    // Another owner (its own reader App installation) is not held.
    assert_eq!(budget::hold_until(&budget::scope("p/r"), now), None);
    let sent = f.calls().len();
    let _ = fetch_required(f.gh(), "p/r", "main", Some(f.store()));
    assert!(f.calls().len() > sent, "the other owner's read was sent");

    // Another token on the same owner is not held either.
    let token_a = budget::scope_with("github.com", "default;aaaa", "o/r");
    let token_b = budget::scope_with("github.com", "default;bbbb", "o/r");
    budget::note_secondary(&token_a, Some(now + 120));
    assert!(budget::hold_until(&token_a, now).is_some());
    assert_eq!(budget::hold_until(&token_b, now), None);
    assert!(budget::before_request(&token_b).is_ok());
}

#[test]
fn a_secondary_limit_without_retry_after_holds_a_minute_not_until_the_primary_reset() {
    let f = Forge::new();
    let now = chrono::Utc::now().timestamp();
    f.refuse(
        &runs_url(SHA),
        403,
        &format!("X-Ratelimit-Remaining: 2000\r\nX-Ratelimit-Reset: {}\r\n", now + 3600),
        SECONDARY,
    );
    assert!(fetch_check_runs(f.gh(), "o/r", SHA).is_err());
    let until = budget::hold_until(&budget::scope("o/r"), now).expect("held");
    assert!((now + 55..=now + 62).contains(&until), "{until} vs {now}");
}

#[test]
fn a_stored_hold_past_the_cap_is_read_as_the_cap() {
    let _f = Forge::new();
    let scope = budget::scope("o/r");
    let now = chrono::Utc::now().timestamp();
    budget::plant_hold_for_test(&scope, now + 86_400);
    assert!(budget::hold_file_for_test(&scope).is_some_and(|p| p.exists()));
    assert_eq!(budget::hold_until(&scope, now), Some(now + 3600));
    // Read back by a fresh process (memory cleared), the disk value is capped too.
    budget::reset_for_test();
    assert_eq!(budget::hold_until(&scope, now), Some(now + 3600));
}

#[test]
fn later_check_runs_pages_are_spaced() {
    let f = Forge::new();
    f.serve(&runs_url(SHA), &page(150, 100, 0));
    f.serve(&format!("{}&page=2", runs_url(SHA)), &page(150, 50, 100));
    fetch_check_runs(f.gh(), "o/r", SHA).unwrap();
    assert_eq!(budget::slept_for_test().len(), 1, "page 2 waited out the spacing");
}

#[test]
fn writing_a_hold_prunes_expired_holds_and_keeps_live_ones() {
    let f = Forge::new();
    let now = chrono::Utc::now().timestamp();
    let expired = budget::scope_with("github.com", "default;aaaa", "o/r");
    let live = budget::scope_with("github.com", "default;bbbb", "o/r");
    budget::plant_hold_for_test(&expired, now - 10);
    let expired_file = budget::hold_file_for_test(&expired).unwrap();
    assert!(expired_file.exists());
    let other = f.store().join("checks-unrelated.json");
    std::fs::write(&other, "{}").unwrap();

    budget::note_secondary(&live, Some(now + 120));

    assert!(!expired_file.exists(), "the expired hold was pruned");
    assert!(budget::hold_file_for_test(&live).unwrap().exists(), "the live hold stays");
    assert!(other.exists(), "only hold files are pruned");
    assert!(budget::hold_until(&live, now).is_some());
}

#[test]
fn a_hold_file_older_than_the_horizon_is_pruned_whatever_it_says() {
    let f = Forge::new();
    let now = chrono::Utc::now().timestamp();
    let stale = budget::scope_with("github.com", "default;cccc", "o/r");
    budget::plant_hold_for_test(&stale, now + 86_400);
    let file = budget::hold_file_for_test(&stale).unwrap();
    let three_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 3600);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(three_hours_ago)
        .unwrap();
    budget::prune(f.store(), now);
    assert!(!file.exists());
}
