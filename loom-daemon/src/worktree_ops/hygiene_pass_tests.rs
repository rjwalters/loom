//! W6 PR2: the pass memo, the merged-only store and the pre-removal confirm.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serial_test::serial;

use super::*;
use crate::forge_call_stats;
use crate::forge_etag_store as store;
use crate::forge_repo_facts::test_support::Env;
use crate::types::ForgeCallCounts;

const REPO: &str = "acme/w6-app";
const OTHER: &str = "acme/other";
const MERGED_AT: &str = "2026-10-01T00:00:00Z";

fn issue_body(number: u32, state: &str) -> String {
    let closed_at = if state == "closed" {
        "\"2026-10-06T00:00:00Z\""
    } else {
        "null"
    };
    format!(
        r#"{{"number":{number},"state":"{state}","closed_at":{closed_at},"repository_url":"https://api.github.com/repos/{REPO}"}}"#
    )
}

/// A `pulls/<n>` body (also one row of a `pulls?head=` listing).
fn pull_body(number: u32, state: &str, merged_at: Option<&str>, sha: &str) -> String {
    pull_body_in(number, state, merged_at, sha, REPO, 7)
}

fn pull_body_in(
    number: u32,
    state: &str,
    merged_at: Option<&str>,
    sha: &str,
    repo: &str,
    id: u64,
) -> String {
    let merged = merged_at.map_or_else(|| "null".to_string(), |m| format!("\"{m}\""));
    let closed = if state == "closed" {
        "\"2026-10-02T00:00:00Z\""
    } else {
        "null"
    };
    format!(
        r#"{{"number":{number},"state":"{state}","merged_at":{merged},"closed_at":{closed},"head":{{"sha":"{sha}"}},"base":{{"repo":{{"id":{id},"full_name":"{repo}"}}}}}}"#
    )
}

/// A fake forge: `acme/w6-app` is repo id 7, `acme/other` repo id 8. Files
/// in its dir steer it: `issue` / `pull` (the body of an `issues/<n>` /
/// `pulls/<n>` read), `pulls` (a `pulls?head=` listing), `item_status`
/// (`404` / `500` for item reads), `notmodified` (answer `304` to an
/// `If-None-Match`), `gql_issue` (what `gh issue view` prints). Every call
/// is logged.
struct Forge {
    dir: PathBuf,
    gh: PathBuf,
}

impl Forge {
    fn new(parent: &Path) -> Self {
        let dir = parent.join("pass-forge");
        std::fs::create_dir_all(&dir).unwrap();
        let gh = dir.join("gh");
        let script = format!(
            r#"#!/bin/sh
D='{d}'
case "$(pwd -P)" in
  '{scope}'/*) ;;
  *) echo "fake forge: foreign cwd" >&2; exit 1 ;;
esac
echo "$*" >> "$D/log"
case "$*" in
  *"/pulls?"*)
    cat "$D/pulls" 2>/dev/null || echo '[]'
    exit 0 ;;
  *"/issues/"*|*"/pulls/"*)
    st=$(cat "$D/item_status" 2>/dev/null || echo 200)
    case "$*" in
      *If-None-Match*) if [ -f "$D/notmodified" ]; then printf 'HTTP/2.0 304 Not Modified\r\nEtag: W/"i1"\r\n\r\n'; exit 1; fi ;;
    esac
    if [ "$st" != 200 ]; then
      case "$*" in *--include*) printf 'HTTP/2.0 %s X\r\n\r\n{{"message":"x"}}' "$st" ;; esac
      echo "gh: x (HTTP $st)" >&2
      exit 1
    fi
    case "$*" in *--include*) printf 'HTTP/2.0 200 OK\r\nEtag: W/"i1"\r\n\r\n' ;; esac
    case "$*" in *"/issues/"*) cat "$D/issue" ;; *) cat "$D/pull" ;; esac
    exit 0 ;;
  "api --include repos/{other}"*)
    printf 'HTTP/2.0 200 OK\r\nEtag: "r2"\r\n\r\n{{"id":8,"name":"other","full_name":"{other}","owner":{{"login":"acme"}}}}'
    exit 0 ;;
  "api --include repos/"*)
    printf 'HTTP/2.0 200 OK\r\nEtag: "r1"\r\n\r\n{{"id":7,"name":"w6-app","full_name":"{repo}","owner":{{"login":"acme"}}}}'
    exit 0 ;;
  *"issue view"*)
    cat "$D/gql_issue" 2>/dev/null || echo OPEN
    exit 0 ;;
  *"pr list"*)
    echo '[]'
    exit 0 ;;
esac
echo "unexpected: $*" >&2
exit 1
"#,
            d = dir.display(),
            scope = parent.canonicalize().unwrap().display(),
            repo = REPO,
            other = OTHER,
        );
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, gh }
    }

    fn set(&self, file: &str, value: &str) {
        std::fs::write(self.dir.join(file), value).unwrap();
    }

    fn clear(&self, file: &str) {
        let _ = std::fs::remove_file(self.dir.join(file));
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Reads of an item or a listing (everything but the repo record).
    fn item_calls(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|l| l.contains("/issues/") || l.contains("/pulls"))
            .collect()
    }

    /// Item and listing reads made by `body`.
    fn during(&self, body: impl FnOnce()) -> Vec<String> {
        let before = self.item_calls().len();
        body();
        self.item_calls().split_off(before)
    }
}

/// Runs `body` with the fake forge as `gh` and a private ledger sink;
/// returns the ledger rows.
fn run(forge: &Forge, body: impl FnOnce()) -> Vec<ForgeCallCounts> {
    let sink = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_GH_BIN", &forge.gh);
    forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    let report = forge_call_stats::status_report(chrono::Utc::now(), None);
    forge_call_stats::set_test_sink_dir(None);
    std::env::remove_var("LOOM_GH_BIN");
    report.host_window.unwrap_or_default()
}

fn calls_of(rows: &[ForgeCallCounts], caller: &str) -> u64 {
    rows.iter()
        .filter(|r| r.caller == caller)
        .map(|r| r.ok + r.not_modified)
        .sum()
}

/// Facts on, a checkout of `REPO`, the fake forge, and the store on for
/// this test thread (off again when the guard drops).
struct Fixture {
    env: Env,
    root: PathBuf,
    forge: Forge,
    store_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let env = Env::new(&[]);
        let root = env.repo("root", &[("origin", &format!("https://github.com/{REPO}.git"))]);
        let forge = Forge::new(env.tmp.path());
        let store_dir = env.tmp.path().join("store");
        store::set_test_daemon_store_dir(Some(store_dir.clone()));
        Self {
            env,
            root,
            forge,
            store_dir,
        }
    }

    /// Terminal entries on disk.
    fn terminal_files(&self) -> usize {
        std::fs::read_dir(&self.store_dir).map_or(0, |rd| {
            rd.flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(terminal::PREFIX)
                })
                .count()
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        store::set_test_daemon_store_dir(None);
        std::env::remove_var(MEMO_ENV);
    }
}

fn merged(sha: &str) -> PrProbe {
    PrProbe {
        status: PrStatus::Merged {
            merged_at: MERGED_AT.to_string(),
        },
        head_sha: Some(sha.to_string()),
    }
}

/// A breaker that is cooling right now.
fn tripped_breaker() -> std::sync::Arc<crate::rate_limit_breaker::SharedRateLimitBreaker> {
    use crate::rate_limit_breaker::{RateLimitBreakerConfig, SharedRateLimitBreaker};
    let b = std::sync::Arc::new(SharedRateLimitBreaker::new(RateLimitBreakerConfig::default()));
    b.observe_failure("API rate limit exceeded", "test", None, chrono::Utc::now());
    assert!(b.is_suppressed(chrono::Utc::now()));
    b
}

/// A second probe of the same item within a pass makes no `gh` call: the
/// issue (state, then `closed_at` from the same body), the PR, the branch.
#[test]
#[serial(loom_config_env)]
fn a_second_probe_in_a_pass_makes_no_call() {
    let fx = Fixture::new();
    fx.forge.set("issue", &issue_body(7, "closed"));
    fx.forge.set("pull", &pull_body(9, "open", None, "cafe"));
    fx.forge
        .set("pulls", &format!("[{}]", pull_body(3, "open", None, "abc")));
    run(&fx.forge, || {
        let pass = Pass::begin(&fx.root);
        let first = fx.forge.during(|| {
            assert_eq!(pass.issue_state(7), "CLOSED");
            assert_eq!(pass.pull(9).status, PrStatus::Open);
            assert_eq!(pass.issue_pr_status(7, false), PrStatus::Open);
        });
        assert_eq!(first.len(), 3, "one read each: {first:?}");
        let again = fx.forge.during(|| {
            assert_eq!(pass.issue_state(7), "CLOSED");
            assert_eq!(pass.issue_closed_at(7).as_deref(), Some("2026-10-06T00:00:00Z"));
            assert_eq!(pass.pull(9).head_sha.as_deref(), Some("cafe"));
            assert_eq!(pass.issue_pr_status(7, false), PrStatus::Open);
        });
        assert_eq!(again, Vec::<String>::new(), "a second probe makes no call");
    });
    assert_eq!(counters::get(MEMO_HIT), 4);
}

/// The memo lives for one pass: the next pass reads everything again, and
/// sees what changed.
#[test]
#[serial(loom_config_env)]
fn nothing_is_shared_across_passes() {
    let fx = Fixture::new();
    fx.forge.set("issue", &issue_body(7, "closed"));
    fx.forge.set("pull", &pull_body(9, "open", None, "cafe"));
    run(&fx.forge, || {
        let first = Pass::begin(&fx.root);
        assert_eq!(first.issue_state(7), "CLOSED");
        assert_eq!(first.pull(9).status, PrStatus::Open);
        drop(first);
        fx.forge.set("issue", &issue_body(7, "open"));
        let calls = fx.forge.during(|| {
            let second = Pass::begin(&fx.root);
            assert_eq!(second.issue_state(7), "OPEN", "a reopened issue is seen");
            assert_eq!(second.pull(9).status, PrStatus::Open);
        });
        assert_eq!(calls.len(), 2, "OPEN and CLOSED are read again: {calls:?}");
    });
    assert_eq!(fx.terminal_files(), 0, "an open PR is never remembered");
}

/// A merged PR is read once, then answered from the store on every later
/// pass: no forge call until the pre-removal read.
#[test]
#[serial(loom_config_env)]
fn a_merged_pr_is_never_re_probed() {
    let fx = Fixture::new();
    fx.forge
        .set("pull", &pull_body(9, "closed", Some(MERGED_AT), "beef"));
    run(&fx.forge, || {
        let first = fx.forge.during(|| {
            assert_eq!(Pass::begin(&fx.root).pull(9), merged("beef"));
        });
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(fx.terminal_files(), 1);
        let later = fx.forge.during(|| {
            for _ in 0..3 {
                assert_eq!(Pass::begin(&fx.root).pull(9), merged("beef"));
            }
        });
        assert_eq!(later, Vec::<String>::new(), "merged is never re-probed");
        assert_eq!(counters::get(TERMINAL_HIT), 3);

        // ... except by the read that precedes a removal.
        let confirm = fx.forge.during(|| {
            let pass = Pass::begin(&fx.root);
            assert_eq!(pass.pull(9), merged("beef"));
            assert_eq!(pass.confirm_pull(9), Confirm::Proceed);
        });
        assert_eq!(confirm.len(), 1, "{confirm:?}");
        assert!(confirm[0].contains(&format!("repos/{REPO}/pulls/9")), "{confirm:?}");
    });
}

/// CLOSED is not terminal — a closed issue and a closed-without-merge PR can
/// both be reopened — so neither is remembered across passes.
#[test]
#[serial(loom_config_env)]
fn closed_is_not_terminal() {
    let fx = Fixture::new();
    fx.forge.set("issue", &issue_body(7, "closed"));
    fx.forge.set("pull", &pull_body(9, "closed", None, "cafe"));
    run(&fx.forge, || {
        for round in 0..2 {
            let calls = fx.forge.during(|| {
                let pass = Pass::begin(&fx.root);
                assert_eq!(pass.issue_state(7), "CLOSED");
                assert!(matches!(pass.pull(9).status, PrStatus::ClosedNoMerge { .. }));
            });
            assert_eq!(calls.len(), 2, "round {round}: both are read again: {calls:?}");
        }
        // Reopened between passes: the next pass sees it.
        fx.forge.set("pull", &pull_body(9, "open", None, "cafe"));
        assert_eq!(Pass::begin(&fx.root).pull(9).status, PrStatus::Open);
    });
    assert_eq!(fx.terminal_files(), 0, "CLOSED is never written to the store");
    assert_eq!(counters::get(TERMINAL_HIT), 0);
}

/// An error, a `404` and an unparseable answer are never held (the same
/// pass asks again) and never remembered (no store entry).
#[test]
#[serial(loom_config_env)]
fn an_unknown_is_never_cached() {
    let fx = Fixture::new();
    run(&fx.forge, || {
        let pass = Pass::begin(&fx.root);
        for status in ["500", "404"] {
            fx.forge.set("item_status", status);
            let calls = fx.forge.during(|| {
                for _ in 0..2 {
                    assert_eq!(pass.issue_state(7), "UNKNOWN", "{status}");
                    assert_eq!(pass.issue_closed_at(7), None, "{status}");
                    assert_eq!(pass.pull(9), PrProbe::unknown(), "{status}");
                }
            });
            assert_eq!(calls.len(), 6, "{status}: every question is a call: {calls:?}");
        }
        fx.forge.clear("item_status");
        fx.forge.set("issue", "not json");
        fx.forge.set("pull", "not json");
        fx.forge.set("pulls", "not json");
        let calls = fx.forge.during(|| {
            for _ in 0..2 {
                assert_eq!(pass.issue_state(7), "UNKNOWN");
                assert_eq!(pass.pull(9), PrProbe::unknown());
                assert_eq!(pass.issue_pr_status(7, false), PrStatus::Unknown);
            }
        });
        assert_eq!(calls.len(), 6, "garbage is asked again: {calls:?}");
        // The forge recovers inside the same pass: the answer is taken.
        fx.forge.set("issue", &issue_body(7, "open"));
        assert_eq!(pass.issue_state(7), "OPEN");
    });
    assert_eq!(fx.terminal_files(), 0);
    assert_eq!(counters::get(MEMO_HIT), 0);
}

/// Before a removal: exactly one read, unconditional — no `If-None-Match`
/// even though an `ETag` is stored and the forge would answer `304` — and
/// booked under its own caller.
#[test]
#[serial(loom_config_env)]
fn a_removal_makes_exactly_one_fresh_unconditional_read() {
    let fx = Fixture::new();
    fx.forge.set("issue", &issue_body(7, "closed"));
    fx.forge
        .set("pull", &pull_body(9, "closed", Some(MERGED_AT), "beef"));
    let rows = run(&fx.forge, || {
        let pass = Pass::begin(&fx.root);
        assert_eq!(pass.issue_state(7), "CLOSED");
        assert_eq!(pass.pull(9), merged("beef"));
        // From here a conditional read would be served the stored body.
        fx.forge.set("notmodified", "");
        let issue = fx
            .forge
            .during(|| assert_eq!(pass.confirm_issue(7), Confirm::Proceed));
        assert_eq!(issue.len(), 1, "{issue:?}");
        assert!(issue[0].contains(&format!("repos/{REPO}/issues/7")), "{issue:?}");
        assert!(!issue[0].contains("If-None-Match"), "unconditional: {issue:?}");
        let pull = fx
            .forge
            .during(|| assert_eq!(pass.confirm_pull(9), Confirm::Proceed));
        assert_eq!(pull.len(), 1, "{pull:?}");
        assert!(pull[0].contains(&format!("repos/{REPO}/pulls/9")), "{pull:?}");
        assert!(!pull[0].contains("If-None-Match"), "unconditional: {pull:?}");
        // The head SHA the remover uses is still the held (and confirmed) one.
        let after = fx
            .forge
            .during(|| assert_eq!(pass.pull(9).head_sha.as_deref(), Some("beef")));
        assert_eq!(after, Vec::<String>::new());
    });
    assert_eq!(calls_of(&rows, "hygiene.confirm_issue"), 1, "{rows:?}");
    assert_eq!(calls_of(&rows, "hygiene.confirm_pr"), 1, "{rows:?}");
    assert_eq!(counters::get(CONFIRM_DOWNGRADE), 0);
}

/// A decision that used no forge state for the item holds nothing: there
/// is nothing to confirm, and no read is made.
#[test]
#[serial(loom_config_env)]
fn nothing_held_means_nothing_to_confirm_for_an_issue() {
    let fx = Fixture::new();
    run(&fx.forge, || {
        let pass = Pass::begin(&fx.root);
        let calls = fx
            .forge
            .during(|| assert_eq!(pass.confirm_issue(7), Confirm::Proceed));
        assert_eq!(calls, Vec::<String>::new());
        // A `pr-<N>` removal always rests on its PR: nothing held is a keep.
        fx.forge.set("pull", &pull_body(9, "open", None, "cafe"));
        assert!(matches!(pass.confirm_pull(9), Confirm::Keep(_)));
    });
    assert_eq!(counters::get(CONFIRM_DOWNGRADE), 1);
}

/// The fresh read disagrees with what the pass held: keep, count, and drop
/// what was held so the next question reads the forge.
#[test]
#[serial(loom_config_env)]
fn a_disagreeing_confirm_keeps_and_counts() {
    let fx = Fixture::new();
    fx.forge.set("issue", &issue_body(7, "closed"));
    fx.forge
        .set("pull", &pull_body(9, "closed", Some(MERGED_AT), "beef"));
    run(&fx.forge, || {
        let pass = Pass::begin(&fx.root);
        assert_eq!(pass.issue_state(7), "CLOSED");
        assert_eq!(pass.pull(9), merged("beef"));
        assert_eq!(fx.terminal_files(), 1);

        // The issue was reopened after the pass read it.
        fx.forge.set("issue", &issue_body(7, "open"));
        let keep = pass
            .confirm_issue(7)
            .keep_reason()
            .expect("a reopened issue keeps");
        assert!(keep.contains("issue #7"), "{keep}");
        assert_eq!(counters::get(CONFIRM_DOWNGRADE), 1);
        assert_eq!(pass.issue_state(7), "OPEN", "the held answer was dropped");

        // The forge says the PR is not what the store remembered.
        fx.forge.set("pull", &pull_body(9, "open", None, "beef"));
        assert!(matches!(pass.confirm_pull(9), Confirm::Keep(_)));
        assert_eq!(counters::get(CONFIRM_DOWNGRADE), 2);
        assert_eq!(fx.terminal_files(), 0, "a contradicted entry is forgotten");
        assert_eq!(pass.pull(9).status, PrStatus::Open, "and the forge is read again");

        // A different head for the same merge is a disagreement too.
        fx.forge
            .set("pull", &pull_body(9, "closed", Some(MERGED_AT), "beef"));
        let pass = Pass::begin(&fx.root);
        assert_eq!(pass.pull(9), merged("beef"));
        fx.forge
            .set("pull", &pull_body(9, "closed", Some(MERGED_AT), "f00d"));
        assert!(matches!(pass.confirm_pull(9), Confirm::Keep(_)));
        assert_eq!(counters::get(CONFIRM_DOWNGRADE), 3);
    });
}

/// A confirm that cannot be answered (error, `404`, a transferred item) is
/// a keep, and counted — but it says nothing about the remembered merge.
#[test]
#[serial(loom_config_env)]
fn an_unknown_confirm_keeps_and_counts() {
    let fx = Fixture::new();
    fx.forge.set("issue", &issue_body(7, "closed"));
    fx.forge
        .set("pull", &pull_body(9, "closed", Some(MERGED_AT), "beef"));
    run(&fx.forge, || {
        for (status, downgrades) in [("500", 2), ("404", 4)] {
            fx.forge.clear("item_status");
            let pass = Pass::begin(&fx.root);
            assert_eq!(pass.issue_state(7), "CLOSED");
            assert_eq!(pass.pull(9), merged("beef"));
            fx.forge.set("item_status", status);
            assert!(matches!(pass.confirm_issue(7), Confirm::Keep(_)), "{status}");
            assert!(matches!(pass.confirm_pull(9), Confirm::Keep(_)), "{status}");
            assert_eq!(counters::get(CONFIRM_DOWNGRADE), downgrades, "{status}");
            assert_eq!(fx.terminal_files(), 1, "{status}: an unknown forgets nothing");
        }
        // Transferred: the forge answers for another repository.
        fx.forge.clear("item_status");
        let pass = Pass::begin(&fx.root);
        assert_eq!(pass.pull(9), merged("beef"));
        fx.forge.set(
            "pull",
            &pull_body_in(9, "closed", Some(MERGED_AT), "beef", "acme/elsewhere", 99),
        );
        assert!(matches!(pass.confirm_pull(9), Confirm::Keep(_)));
        assert_eq!(fx.terminal_files(), 1);
    });
}

/// An unmerged branch status answered from the pass's memory is not a
/// first-hand read for this removal: keep, and read it again next time. A
/// merged status cannot revert and passes.
#[test]
#[serial(loom_config_env)]
fn a_remembered_unmerged_branch_status_never_backs_a_removal() {
    let fx = Fixture::new();
    fx.forge.set("issue", &issue_body(7, "closed"));
    fx.forge.set("pulls", "[]");
    run(&fx.forge, || {
        let pass = Pass::begin(&fx.root);
        assert_eq!(pass.issue_state(7), "CLOSED");
        assert_eq!(pass.issue_pr_status(7, false), PrStatus::NoPr);
        // Read for this decision: fine.
        assert_eq!(pass.confirm_issue(7), Confirm::Proceed);
        // Answered from memory, then used for a removal: not fine.
        assert_eq!(pass.issue_pr_status(7, false), PrStatus::NoPr);
        let keep = pass.confirm_issue(7).keep_reason().expect("keep");
        assert!(keep.contains("feature/issue-7"), "{keep}");
        assert_eq!(counters::get(CONFIRM_DOWNGRADE), 1);
        let calls = fx
            .forge
            .during(|| assert_eq!(pass.issue_pr_status(7, false), PrStatus::NoPr));
        assert_eq!(calls.len(), 1, "forgotten, so read again: {calls:?}");

        fx.forge
            .set("pulls", &format!("[{}]", pull_body(3, "closed", Some(MERGED_AT), "abc")));
        let pass = Pass::begin(&fx.root);
        assert_eq!(pass.issue_state(7), "CLOSED");
        for _ in 0..2 {
            assert!(matches!(pass.issue_pr_status(7, false), PrStatus::Merged { .. }));
        }
        assert_eq!(pass.confirm_issue(7), Confirm::Proceed);
    });
}

/// While the breaker is cooling: zero forge calls — not the read, not the
/// store's repo lookup, not the confirm — and every answer is a keep.
#[test]
#[serial(loom_config_env)]
fn a_cooling_breaker_means_no_forge_call_and_keep() {
    let fx = Fixture::new();
    fx.forge.set("issue", &issue_body(7, "closed"));
    fx.forge
        .set("pull", &pull_body(9, "closed", Some(MERGED_AT), "beef"));
    run(&fx.forge, || {
        let pass = Pass::begin(&fx.root);
        assert_eq!(pass.issue_state(7), "CLOSED");
        assert_eq!(pass.pull(9), merged("beef"));
        assert_eq!(fx.terminal_files(), 1);
        let before = fx.forge.calls().len();
        crate::rate_limit_breaker::with_test_global(tripped_breaker(), || {
            // What this pass already holds still decides nothing on its own:
            // the confirm cannot be made, so everything is kept.
            assert!(matches!(pass.confirm_issue(7), Confirm::Keep(_)));
            assert!(matches!(pass.confirm_pull(9), Confirm::Keep(_)));
            // A new pass reads nothing, not even the remembered merge.
            let cold = Pass::begin(&fx.root);
            assert_eq!(cold.issue_state(8), "UNKNOWN");
            assert_eq!(cold.issue_closed_at(8), None);
            assert_eq!(cold.pull(9), PrProbe::unknown());
            assert_eq!(cold.confirm_issue(8), Confirm::Proceed, "nothing held, no read");
            assert!(matches!(cold.confirm_pull(9), Confirm::Keep(_)));
        });
        assert_eq!(fx.forge.calls().len(), before, "zero forge calls: {:?}", fx.forge.calls());
    });
    assert_eq!(counters::get(TERMINAL_HIT), 0);
    assert_eq!(counters::get(CONFIRM_DOWNGRADE), 3);
}

/// The store is scoped by repository identity: another repository's PR of
/// the same number is never answered from this one's entry.
#[test]
#[serial(loom_config_env)]
fn a_remembered_merge_never_aliases_another_repository() {
    let fx = Fixture::new();
    let other = fx
        .env
        .repo("other", &[("origin", &format!("https://github.com/{OTHER}.git"))]);
    fx.forge
        .set("pull", &pull_body(9, "closed", Some(MERGED_AT), "beef"));
    run(&fx.forge, || {
        assert_eq!(Pass::begin(&fx.root).pull(9), merged("beef"));
        fx.forge
            .set("pull", &pull_body_in(9, "open", None, "0ther", OTHER, 8));
        let calls = fx.forge.during(|| {
            assert_eq!(Pass::begin(&other).pull(9).status, PrStatus::Open);
        });
        assert_eq!(calls.len(), 1, "the other repo reads its own PR: {calls:?}");
        assert!(calls[0].contains(&format!("repos/{OTHER}/pulls/9")), "{calls:?}");
        // And this repo's entry is untouched.
        let calls = fx.forge.during(|| {
            assert_eq!(Pass::begin(&fx.root).pull(9), merged("beef"));
        });
        assert_eq!(calls, Vec::<String>::new());
    });
}

fn fact(repo_id: Option<u64>, owner: &str, name: &str) -> crate::forge_repo_facts::Fact {
    crate::forge_repo_facts::Fact {
        host: "github.com".to_string(),
        configured_nwo: format!("{owner}/{name}"),
        owner: owner.to_string(),
        name: name.to_string(),
        repo_id,
        verified_at: 0,
        fresh: false,
    }
}

/// The scope follows the repo id through a rename, and a name-keyed scope
/// never collides with an id-keyed one.
#[test]
fn the_store_scope_is_the_repo_id_where_known() {
    let by_id = terminal::repo_scope(&fact(Some(7), "acme", "w6-app"));
    assert_eq!(by_id, terminal::repo_scope(&fact(Some(7), "newco", "renamed")));
    assert_ne!(by_id, terminal::repo_scope(&fact(Some(8), "acme", "w6-app")));
    let by_name = terminal::repo_scope(&fact(None, "Acme", "W6-App"));
    assert_eq!(by_name, "github.com#name:acme/w6-app");
    assert_ne!(by_name, by_id);
}

/// Only `Merged` can be written, and an entry that is not exactly this
/// repository's PR is ignored.
#[test]
#[serial(loom_config_env)]
fn the_store_holds_merged_only_and_rejects_foreign_entries() {
    let fx = Fixture::new();
    run(&fx.forge, || {
        let not_terminal = [
            PrStatus::Open,
            PrStatus::NoPr,
            PrStatus::Unknown,
            PrStatus::ClosedNoMerge {
                closed_at: Some(MERGED_AT.to_string()),
            },
            PrStatus::Merged {
                merged_at: " ".to_string(),
            },
        ];
        for status in not_terminal {
            let facts = PullFacts {
                status: status.clone(),
                head_sha: Some("abc".to_string()),
            };
            terminal::record(&fx.root, 9, &facts);
            assert_eq!(fx.terminal_files(), 0, "{status:?} is not terminal");
            assert_eq!(terminal::lookup(&fx.root, 9), None);
        }
        let facts = PullFacts {
            status: PrStatus::Merged {
                merged_at: MERGED_AT.to_string(),
            },
            head_sha: Some("abc".to_string()),
        };
        terminal::record(&fx.root, 9, &facts);
        assert_eq!(terminal::lookup(&fx.root, 9), Some(facts));
        assert_eq!(terminal::lookup(&fx.root, 10), None);

        // Rewrite the entry in place as some other PR / repo: ignored.
        let path = std::fs::read_dir(&fx.store_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(terminal::PREFIX))
            })
            .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        for forged in [
            raw.replace("\"pr\":9", "\"pr\":10"),
            raw.replace("#id:7", "#id:8"),
            raw.replace("\"v\":1", "\"v\":2"),
            "not json".to_string(),
        ] {
            assert_ne!(forged, raw, "the fixture must actually change the entry");
            std::fs::write(&path, &forged).unwrap();
            assert_eq!(terminal::lookup(&fx.root, 9), None, "{forged}");
        }
        terminal::forget(&fx.root, 9);
        assert_eq!(fx.terminal_files(), 0);
    });
}

/// `LOOM_HYGIENE_MEMO=0`: issues are read every time, the reaper's per-pass
/// PR probe is the only thing held, nothing is remembered, no confirm is
/// made, and `clean` is back on `gh issue view` / `gh pr list`.
#[test]
#[serial(loom_config_env)]
fn the_kill_switch_restores_the_old_reads() {
    let fx = Fixture::new();
    std::env::set_var(MEMO_ENV, "0");
    fx.forge.set("issue", &issue_body(7, "closed"));
    fx.forge
        .set("pull", &pull_body(9, "closed", Some(MERGED_AT), "beef"));
    fx.forge.set("gql_issue", "CLOSED");
    run(&fx.forge, || {
        let pass = Pass::begin(&fx.root);
        let calls = fx.forge.during(|| {
            assert_eq!(pass.issue_state(7), "CLOSED");
            assert_eq!(pass.issue_state(7), "CLOSED");
            assert!(pass.issue_closed_at(7).is_some());
            assert_eq!(pass.pull(9), merged("beef"));
            assert_eq!(pass.pull(9), merged("beef"));
        });
        assert_eq!(calls.len(), 4, "3 issue reads, 1 PR read: {calls:?}");
        let confirms = fx.forge.during(|| {
            assert_eq!(pass.confirm_issue(7), Confirm::Proceed);
            assert_eq!(pass.confirm_pull(9), Confirm::Proceed);
            assert_eq!(pass.gate(7, false, WorktreeDecision::Remove), WorktreeDecision::Remove);
        });
        assert_eq!(confirms, Vec::<String>::new(), "no confirm read");
        let before = fx.forge.calls().len();
        assert_eq!(pass.clean_issue_state(7), "CLOSED");
        assert_eq!(pass.clean_pr_status(7), PrStatus::NoPr);
        assert_eq!(branch_issue_state(&fx.root, 7, false), "CLOSED");
        let graphql = fx.forge.calls().split_off(before);
        assert_eq!(graphql.len(), 3, "{graphql:?}");
        assert!(graphql[0].contains("issue view"), "{graphql:?}");
        assert!(graphql[1].contains("pr list"), "{graphql:?}");
        assert!(graphql[2].contains("issue view"), "{graphql:?}");
    });
    assert_eq!(fx.terminal_files(), 0, "nothing is remembered");
    assert_eq!(counters::get(MEMO_HIT), 0);
    assert_eq!(counters::get(CONFIRM_DOWNGRADE), 0);
}

/// `clean`'s probes are REST with the memo on, and its gate turns an
/// unconfirmed removal into a skip — but confirms nothing on a dry run or
/// for a decision that removes nothing.
#[test]
#[serial(loom_config_env)]
fn cleans_gate_confirms_only_what_removes() {
    let fx = Fixture::new();
    fx.forge.set("issue", &issue_body(7, "closed"));
    fx.forge
        .set("pulls", &format!("[{}]", pull_body(3, "closed", Some(MERGED_AT), "abc")));
    run(&fx.forge, || {
        let pass = Pass::begin(&fx.root);
        let probes = fx.forge.during(|| {
            assert_eq!(pass.clean_issue_state(7), "CLOSED");
            assert!(matches!(pass.clean_pr_status(7), PrStatus::Merged { .. }));
        });
        assert_eq!(probes.len(), 2, "{probes:?}");
        assert!(probes[0].contains(&format!("repos/{REPO}/issues/7")), "{probes:?}");
        assert!(probes[1].contains(&format!("repos/{REPO}/pulls?")), "{probes:?}");

        let none = fx.forge.during(|| {
            let kept = WorktreeDecision::SkipPrOpen;
            assert_eq!(pass.gate(7, false, kept.clone()), kept);
            assert_eq!(pass.gate(7, true, WorktreeDecision::Remove), WorktreeDecision::Remove);
        });
        assert_eq!(none, Vec::<String>::new(), "a skip and a dry run confirm nothing");

        for decision in [
            WorktreeDecision::Remove,
            WorktreeDecision::RemoveWithQuarantine,
            WorktreeDecision::ConfirmClosedIssue,
        ] {
            let one = fx
                .forge
                .during(|| assert_eq!(pass.gate(7, false, decision.clone()), decision));
            assert_eq!(one.len(), 1, "{decision:?}: {one:?}");
        }

        fx.forge.set("issue", &issue_body(7, "open"));
        let gated = pass.gate(7, false, WorktreeDecision::RemoveWithQuarantine);
        assert!(matches!(gated, WorktreeDecision::SkipNotMerged(_)), "{gated:?}");
    });
    assert_eq!(counters::get(CONFIRM_DOWNGRADE), 1);
}

/// `clean`'s stale-branch pass deletes a branch on `CLOSED`: that answer is
/// confirmed by a fresh read; `OPEN` costs one (conditional) read.
#[test]
#[serial(loom_config_env)]
fn a_stale_branch_closed_is_confirmed() {
    let fx = Fixture::new();
    run(&fx.forge, || {
        fx.forge.set("issue", &issue_body(7, "open"));
        let open = fx
            .forge
            .during(|| assert_eq!(branch_issue_state(&fx.root, 7, false), "OPEN"));
        assert_eq!(open.len(), 1, "{open:?}");

        fx.forge.set("issue", &issue_body(7, "closed"));
        let closed = fx
            .forge
            .during(|| assert_eq!(branch_issue_state(&fx.root, 7, false), "CLOSED"));
        assert_eq!(closed.len(), 2, "the read and its confirm: {closed:?}");
        assert!(!closed[1].contains("If-None-Match"), "{closed:?}");

        let dry = fx
            .forge
            .during(|| assert_eq!(branch_issue_state(&fx.root, 7, true), "CLOSED"));
        assert_eq!(dry.len(), 1, "a dry run deletes nothing: {dry:?}");

        // A `304` serves the stored CLOSED body; the fresh read says OPEN.
        fx.forge.set("notmodified", "");
        fx.forge.set("issue", &issue_body(7, "open"));
        assert_eq!(branch_issue_state(&fx.root, 7, false), UNCONFIRMED);
    });
    assert_eq!(counters::get(CONFIRM_DOWNGRADE), 1);
}
