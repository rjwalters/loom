//! W6: the hygiene read path — explicit target, conditional fresh reads,
//! identity checks, fail-closed answers, and the wrappers every consumer
//! calls.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serial_test::serial;

use super::*;
use crate::forge_call_stats::{self, counters};
use crate::forge_repo_facts::test_support::Env;
use crate::types::ForgeCallCounts;
use crate::worktree_ops::gh::{issue_closed_at_rest, issue_state_rest};

const REPO: &str = "acme/w6-app";

fn issue_body(number: u32, state: &str, repo: &str) -> String {
    let closed_at = if state == "closed" {
        "\"2026-10-06T00:00:00Z\""
    } else {
        "null"
    };
    format!(
        r#"{{"number":{number},"state":"{state}","closed_at":{closed_at},"repository_url":"https://api.github.com/repos/{repo}"}}"#
    )
}

fn pull_body(number: u32, merged_at: Option<&str>, sha: &str, repo: &str, id: u64) -> String {
    let merged = merged_at.map_or_else(|| "null".to_string(), |m| format!("\"{m}\""));
    let state = if merged_at.is_some() {
        "closed"
    } else {
        "open"
    };
    format!(
        r#"{{"number":{number},"state":"{state}","merged_at":{merged},"closed_at":{merged},"head":{{"sha":"{sha}"}},"base":{{"repo":{{"id":{id},"full_name":"{repo}"}}}}}}"#
    )
}

/// A fake forge for `REPO` (repo id 7). Files in its dir steer it: `item`
/// (the body of any `issues/<n>` / `pulls/<n>` read), `item_status`
/// (`404` / `500`), `item_stderr` (gh's stderr on a failed item read),
/// `notmodified` (answer `304` to an `If-None-Match`),
/// `pulls` (a `pulls?head=` listing). Every call is logged with the
/// `GH_REPO` the child saw.
struct Forge {
    dir: PathBuf,
    gh: PathBuf,
}

impl Forge {
    fn new(parent: &Path) -> Self {
        let dir = parent.join("w6-forge");
        std::fs::create_dir_all(&dir).unwrap();
        let gh = dir.join("gh");
        let script = format!(
            r#"#!/bin/sh
D='{d}'
case "$(pwd -P)" in
  '{scope}'/*) ;;
  *) echo "fake forge: foreign cwd" >&2; exit 1 ;;
esac
echo "$* | GH_REPO=${{GH_REPO-unset}}" >> "$D/log"
case "$*" in
  *"/issues/"*|*"/pulls/"*)
    st=$(cat "$D/item_status" 2>/dev/null || echo 200)
    case "$*" in
      *If-None-Match*) if [ -f "$D/notmodified" ]; then printf 'HTTP/2.0 304 Not Modified\r\nEtag: W/"i1"\r\n\r\n'; exit 1; fi ;;
    esac
    if [ "$st" != 200 ]; then
      case "$*" in *--include*) printf 'HTTP/2.0 %s X\r\n\r\n{{"message":"x"}}' "$st" ;; esac
      cat "$D/item_stderr" >&2 2>/dev/null || echo "gh: x (HTTP $st)" >&2
      exit 1
    fi
    case "$*" in *--include*) printf 'HTTP/2.0 200 OK\r\nEtag: W/"i1"\r\n\r\n' ;; esac
    cat "$D/item"
    exit 0 ;;
  *"/pulls?"*)
    cat "$D/pulls" 2>/dev/null || echo '[]'
    exit 0 ;;
  "api --include repos/"*)
    printf 'HTTP/2.0 200 OK\r\nEtag: "r1"\r\n\r\n{{"id":7,"name":"w6-app","full_name":"{repo}","owner":{{"login":"acme"}}}}'
    exit 0 ;;
esac
echo "unexpected: $*" >&2
exit 1
"#,
            d = dir.display(),
            scope = parent.canonicalize().unwrap().display(),
            repo = REPO,
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

    /// Every call so far, item reads and repo reads alike.
    fn all_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Repo reads (`repos/<o>/<r>` — the facts record's resolve / confirm).
    fn repo_reads(&self) -> usize {
        self.all_calls()
            .iter()
            .filter(|l| !l.contains("/issues/") && !l.contains("/pulls"))
            .count()
    }

    /// Item reads (`issues/<n>`, `pulls/<n>`) so far.
    fn item_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("/issues/") || l.contains("/pulls"))
            .map(str::to_string)
            .collect()
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

fn rows_of(rows: &[ForgeCallCounts], caller: &str) -> (u64, u64) {
    let of = |f: fn(&ForgeCallCounts) -> u64| -> u64 {
        rows.iter().filter(|r| r.caller == caller).map(f).sum()
    };
    (of(|r| r.ok), of(|r| r.not_modified))
}

/// Facts on (with `vars` as the resolver's environment), a checkout of
/// `REPO`, and the fake forge.
fn fixture(vars: &[(&str, &str)]) -> (Env, PathBuf, Forge) {
    let env = Env::new(vars);
    let root = env.repo("root", &[("origin", &format!("https://github.com/{REPO}.git"))]);
    let forge = Forge::new(env.tmp.path());
    (env, root, forge)
}

/// One read answers state AND `closed_at`; a re-read sends the stored ETag
/// and a `304` serves the stored body. Every read reaches the forge (no
/// memo), and the ledger books 200 vs 304 under the caller.
#[test]
#[serial(loom_config_env)]
fn issue_reads_are_conditional_and_fresh() {
    let (env, root, forge) = fixture(&[]);
    let store_dir = env.tmp.path().join("store");
    store::set_test_daemon_store_dir(Some(store_dir));
    forge.set("item", &issue_body(7, "closed", REPO));
    let mut got = Vec::new();
    let rows = run(&forge, || {
        got.push(issue_facts(&root, 7, "worktree.issue_state_rest"));
        forge.set("notmodified", "");
        got.push(issue_facts(&root, 7, "worktree.issue_state_rest"));
    });
    store::set_test_daemon_store_dir(None);
    let want = Read::Ok(IssueFacts {
        state: IssueState::Closed,
        closed_at: Some("2026-10-06T00:00:00Z".to_string()),
    });
    assert_eq!(got, [want.clone(), want]);
    let calls = forge.item_calls();
    assert_eq!(calls.len(), 2, "every read reaches the forge: {calls:?}");
    assert!(calls[0].contains(&format!("repos/{REPO}/issues/7")), "{calls:?}");
    assert!(!calls[0].contains("If-None-Match"), "{calls:?}");
    assert!(calls[1].contains(r#"If-None-Match: W/"i1""#), "{calls:?}");
    assert_eq!(rows_of(&rows, "worktree.issue_state_rest"), (1, 1), "{rows:?}");
}

/// `LOOM_REPO` naming another repo never redirects a hygiene read: the URL
/// names the checkout's own repo, for issues, PRs and listings alike.
#[test]
#[serial(loom_config_env)]
fn loom_repo_never_redirects_a_hygiene_read() {
    let (_env, root, forge) = fixture(&[("LOOM_REPO", "acme/other"), ("GH_REPO", "acme/other")]);
    std::env::set_var("LOOM_REPO", "acme/other");
    forge.set("item", &issue_body(12, "open", REPO));
    let mut state = String::new();
    run(&forge, || state = issue_state_rest(&root, 12));
    std::env::remove_var("LOOM_REPO");
    assert_eq!(state, "OPEN");
    let calls = forge.item_calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].contains(&format!("repos/{REPO}/issues/12")), "{calls:?}");
    assert!(!calls[0].contains("acme/other/"), "{calls:?}");
}

/// The owner and the listing for `head=` come from the checkout's repo too.
#[test]
#[serial(loom_config_env)]
fn the_pr_listing_names_the_checkouts_repo() {
    let (_env, root, forge) = fixture(&[("LOOM_REPO", "acme/other")]);
    forge.set(
        "pulls",
        &format!("[{}]", pull_body(3, Some("2026-10-01T00:00:00Z"), "abc", REPO, 7)),
    );
    let mut status = PrStatus::Unknown;
    run(&forge, || {
        let owner = super::super::clean_owner::repo_owner(&root).unwrap();
        status = super::super::clean_owner::pr_status_confirmed(&root, &owner, "feature/issue-3");
    });
    assert!(matches!(status, PrStatus::Merged { .. }), "{status:?}");
    let listing: Vec<_> = forge
        .item_calls()
        .into_iter()
        .filter(|c| c.contains("/pulls?"))
        .collect();
    assert_eq!(listing.len(), 1, "{listing:?}");
    assert!(
        listing[0].starts_with(&format!(
            "api repos/{REPO}/pulls?state=all&head=acme:feature/issue-3&per_page=30"
        )),
        "{listing:?}"
    );
    assert!(listing[0].ends_with("GH_REPO=unset"), "GH_REPO is stripped: {listing:?}");
}

/// A body for another repo (a transferred item, a followed redirect) or
/// another number is Unknown, counted, and casts doubt on the record.
#[test]
#[serial(loom_config_env)]
fn an_identity_mismatch_is_unknown() {
    let (_env, root, forge) = fixture(&[]);
    let mut got = Vec::new();
    run(&forge, || {
        forge.set("item", &issue_body(7, "closed", "acme/elsewhere"));
        got.push(issue_state_rest(&root, 7));
        forge.set("item", &issue_body(8, "closed", REPO));
        got.push(issue_state_rest(&root, 7));
        forge.set("item", &pull_body(9, Some("2026-10-01T00:00:00Z"), "abc", REPO, 99));
        got.push(format!("{:?}", super::clean::check_pr_by_number_rest(&root, 9).status));
    });
    assert_eq!(got, ["UNKNOWN", "UNKNOWN", "Unknown"]);
    assert_eq!(counters::get(IDENTITY_MISMATCH), 3);
}

/// The repo id decides when both sides carry one: a renamed repo's PR (same
/// id, new name) is still this repo's PR, and the record is re-resolved.
#[test]
#[serial(loom_config_env)]
fn a_matching_repo_id_survives_a_rename() {
    let (_env, root, forge) = fixture(&[]);
    forge.set("item", &pull_body(9, Some("2026-10-01T00:00:00Z"), "beef", "acme/renamed", 7));
    let mut got = None;
    run(&forge, || got = Some(pull_facts(&root, 9, "clean.pr_by_number_rest")));
    let facts = got.unwrap().ok().expect("same repo id: an answer");
    assert!(matches!(facts.status, PrStatus::Merged { .. }));
    assert_eq!(facts.head_sha.as_deref(), Some("beef"));
    assert_eq!(counters::get(IDENTITY_MISMATCH), 0);
}

/// `404` is Gone, a server error or garbage is Unknown — and nothing is
/// remembered: a second read is a second call.
#[test]
#[serial(loom_config_env)]
fn failures_are_never_remembered() {
    let (_env, root, forge) = fixture(&[]);
    let mut got = Vec::new();
    run(&forge, || {
        forge.set("item_status", "404");
        got.push(format!("{:?}", issue_facts(&root, 5, "worktree.issue_state_rest")));
        got.push(format!("{:?}", issue_closed_at_rest(&root, 5)));
        forge.set("item_status", "500");
        got.push(issue_state_rest(&root, 5));
        got.push(issue_state_rest(&root, 5));
        forge.clear("item_status");
        forge.set("item", "not json");
        got.push(issue_state_rest(&root, 5));
    });
    assert_eq!(got, ["Gone", "None", "UNKNOWN", "UNKNOWN", "UNKNOWN"]);
    assert_eq!(forge.item_calls().len(), 5, "{:?}", forge.item_calls());
    assert_eq!(counters::get(ITEM_GONE), 2);
}

/// A root the facts cannot model keeps gh's placeholder, unconditional, with
/// `GH_REPO` stripped even when `LOOM_REPO` names another repo.
#[test]
#[serial(loom_config_env)]
fn the_placeholder_fallback_never_sees_loom_repo() {
    let (_env, root, forge) = fixture(&[("LOOM_REPO_FACTS", "0")]);
    std::env::set_var("LOOM_REPO", "acme/other");
    forge.set("item", &issue_body(4, "open", REPO));
    let mut state = String::new();
    run(&forge, || {
        state = issue_state_rest(&root, 4);
        state.push_str(&issue_state_rest(&root, 4));
    });
    std::env::remove_var("LOOM_REPO");
    assert_eq!(state, "OPENOPEN");
    let calls = forge.item_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    for c in &calls {
        assert!(c.starts_with("api repos/{owner}/{repo}/issues/4 "), "{c}");
        assert!(c.ends_with("GH_REPO=unset"), "{c}");
        assert!(!c.contains("If-None-Match"), "{c}");
    }
}

/// `LOOM_HYGIENE_CONDITIONAL=0`: nothing is sent and nothing stored.
#[test]
#[serial(loom_config_env)]
fn the_kill_switch_makes_reads_unconditional() {
    let (env, root, forge) = fixture(&[]);
    store::set_test_daemon_store_dir(Some(env.tmp.path().join("store")));
    std::env::set_var(CONDITIONAL_ENV, "0");
    forge.set("item", &issue_body(6, "open", REPO));
    run(&forge, || {
        let _ = issue_state_rest(&root, 6);
        let _ = issue_state_rest(&root, 6);
    });
    std::env::remove_var(CONDITIONAL_ENV);
    store::set_test_daemon_store_dir(None);
    let calls = forge.item_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls.iter().all(|c| !c.contains("If-None-Match")), "{calls:?}");
}

/// The entry is the agents' `view-` entry: `gh-cached --invalidate N` drops
/// it, so the next read after a write is unconditional.
#[test]
#[serial(loom_config_env)]
fn reads_share_the_view_entry_that_invalidate_drops() {
    let (env, root, forge) = fixture(&[]);
    let store_dir = env.tmp.path().join("store");
    store::set_test_daemon_store_dir(Some(store_dir.clone()));
    forge.set("item", &issue_body(10, "open", REPO));
    run(&forge, || {
        let _ = issue_state_rest(&root, 10);
    });
    let target = store::Target {
        repo: Some(REPO.to_string()),
        host: Some("github.com".to_string()),
    };
    let url = format!("repos/{REPO}/issues/10");
    let entry = crate::forge_cached_view::entry_path(
        &store_dir,
        "issue",
        10,
        &store::cache_key(Some(&root), &target, &url),
    );
    assert!(entry.exists(), "the read wrote the shared view entry");
    std::fs::remove_file(&entry).unwrap(); // what `--invalidate 10` does
    forge.set("notmodified", "");
    run(&forge, || {
        let _ = issue_state_rest(&root, 10);
    });
    store::set_test_daemon_store_dir(None);
    let calls = forge.item_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(!calls[1].contains("If-None-Match"), "after invalidation: {calls:?}");
}

/// A `pr-<N>` probe takes its head SHA from the same fresh read.
#[test]
#[serial(loom_config_env)]
fn a_pull_read_carries_status_and_head() {
    let (_env, root, forge) = fixture(&[]);
    forge.set("item", &pull_body(21, None, "cafe", REPO, 7));
    let mut probe = None;
    run(&forge, || probe = Some(super::clean::check_pr_by_number_rest(&root, 21)));
    let probe = probe.unwrap();
    assert_eq!(probe.status, PrStatus::Open);
    assert_eq!(probe.head_sha.as_deref(), Some("cafe"));
    let calls = forge.item_calls();
    assert!(calls[0].contains(&format!("repos/{REPO}/pulls/21")), "{calls:?}");
}

/// A breaker that is cooling right now.
fn tripped_breaker() -> std::sync::Arc<crate::rate_limit_breaker::SharedRateLimitBreaker> {
    use crate::rate_limit_breaker::{RateLimitBreakerConfig, SharedRateLimitBreaker};
    let b = std::sync::Arc::new(SharedRateLimitBreaker::new(RateLimitBreakerConfig::default()));
    b.observe_failure("API rate limit exceeded", "test", None, chrono::Utc::now());
    assert!(b.is_suppressed(chrono::Utc::now()));
    b
}

fn fresh_breaker() -> std::sync::Arc<crate::rate_limit_breaker::SharedRateLimitBreaker> {
    use crate::rate_limit_breaker::{RateLimitBreakerConfig, SharedRateLimitBreaker};
    std::sync::Arc::new(SharedRateLimitBreaker::new(RateLimitBreakerConfig::default()))
}

/// While the global breaker is cooling, an item read makes NO forge call —
/// not the item read, not the repo resolve — and is Unknown (KEEP), on the
/// fact path and the placeholder path alike.
#[test]
#[serial(loom_config_env)]
fn a_cooling_breaker_suppresses_every_item_read() {
    for vars in [&[][..], &[("LOOM_REPO_FACTS", "0")][..]] {
        let (_env, root, forge) = fixture(vars);
        forge.set("item", &issue_body(7, "closed", REPO));
        let mut got = Vec::new();
        crate::rate_limit_breaker::with_test_global(tripped_breaker(), || {
            run(&forge, || {
                got.push(format!("{:?}", issue_facts(&root, 7, "worktree.issue_state_rest")));
                got.push(issue_state_rest(&root, 7));
                got.push(format!("{:?}", issue_closed_at_rest(&root, 7)));
                got.push(format!("{:?}", pull_facts(&root, 7, "clean.pr_by_number_rest")));
            });
        });
        assert_eq!(got, ["Unknown", "UNKNOWN", "None", "Unknown"], "{vars:?}");
        assert_eq!(forge.all_calls(), Vec::<String>::new(), "{vars:?}: zero forge calls");
    }
}

/// A rate-limit refusal on an item read is reported to the global breaker,
/// which trips — on the fact path and the placeholder path alike.
#[test]
#[serial(loom_config_env)]
fn a_rate_limited_item_read_trips_the_breaker() {
    for vars in [&[][..], &[("LOOM_REPO_FACTS", "0")][..]] {
        let (_env, root, forge) = fixture(vars);
        forge.set("item_status", "403");
        forge.set("item_stderr", "gh: API rate limit exceeded for installation (HTTP 403)");
        let breaker = fresh_breaker();
        let mut state = String::new();
        crate::rate_limit_breaker::with_test_global(breaker.clone(), || {
            run(&forge, || state = issue_state_rest(&root, 7));
        });
        assert_eq!(state, "UNKNOWN", "{vars:?}");
        assert!(breaker.is_suppressed(chrono::Utc::now()), "{vars:?}: the refusal tripped it");
    }
}

/// A plain server error is not a rate limit: the breaker stays closed.
#[test]
#[serial(loom_config_env)]
fn an_ordinary_failure_does_not_trip_the_breaker() {
    let (_env, root, forge) = fixture(&[]);
    forge.set("item_status", "500");
    let breaker = fresh_breaker();
    crate::rate_limit_breaker::with_test_global(breaker.clone(), || {
        run(&forge, || assert_eq!(issue_state_rest(&root, 7), "UNKNOWN"));
    });
    assert!(!breaker.is_suppressed(chrono::Utc::now()));
}

fn fact_of(repo_id: Option<u64>) -> Fact {
    Fact {
        host: "github.com".to_string(),
        configured_nwo: REPO.to_string(),
        owner: "acme".to_string(),
        name: "w6-app".to_string(),
        repo_id,
        verified_at: 0,
        fresh: false,
    }
}

/// Only a body for the asked-for number, naming another repo under the same
/// id (or with no id to compare), may cast doubt on the record.
#[test]
fn only_a_same_item_rename_is_observed() {
    let seen = |number: u64, repo: &'static str, id: Option<u64>| Seen {
        number: Some(number),
        repo: Some(repo),
        id,
    };
    let with_id = fact_of(Some(7));
    let no_id = fact_of(None);
    // A rename: same number, same id (or none), new name.
    assert!(should_observe(&with_id, 9, &seen(9, "acme/renamed", Some(7))));
    assert!(should_observe(&no_id, 9, &seen(9, "acme/renamed", None)));
    assert!(should_observe(&with_id, 9, &seen(9, "acme/renamed", None)));
    // A transferred issue answers with its new repo's number.
    assert!(!should_observe(&no_id, 9, &seen(41, "acme/elsewhere", None)));
    // Another repo's id is another repo, not this one renamed.
    assert!(!should_observe(&with_id, 9, &seen(9, "acme/elsewhere", Some(99))));
    // The same name says nothing new.
    assert!(!should_observe(&with_id, 9, &seen(9, REPO, Some(7))));
}

/// A transferred issue (another number, another repo) is Unknown on every
/// pass, but never marks the record suspect: no confirm read follows.
#[test]
#[serial(loom_config_env)]
fn a_transferred_issue_never_re_resolves_the_repo() {
    let (_env, root, forge) = fixture(&[]);
    forge.set("item", &issue_body(41, "open", "acme/elsewhere"));
    let mut got = Vec::new();
    run(&forge, || {
        for _ in 0..3 {
            got.push(issue_state_rest(&root, 9));
        }
    });
    assert_eq!(got, ["UNKNOWN", "UNKNOWN", "UNKNOWN"]);
    let first = forge.repo_reads();
    assert!(first >= 1, "the record was resolved once: {:?}", forge.all_calls());
    run(&forge, || got.push(issue_state_rest(&root, 9)));
    assert_eq!(forge.repo_reads(), first, "no confirm read: {:?}", forge.all_calls());
}

/// A first-hand body naming the repo under a new name DOES mark it suspect:
/// the next use confirms the record with a repo read.
#[test]
#[serial(loom_config_env)]
fn a_first_hand_rename_re_resolves_the_repo() {
    let (_env, root, forge) = fixture(&[]);
    forge.set("item", &issue_body(9, "open", REPO));
    run(&forge, || assert_eq!(issue_state_rest(&root, 9), "OPEN"));
    let before = forge.repo_reads();
    run(&forge, || assert_eq!(issue_state_rest(&root, 9), "OPEN"));
    assert_eq!(forge.repo_reads(), before, "a settled record is not re-read");
    forge.set("item", &issue_body(9, "open", "acme/renamed"));
    run(&forge, || {
        let _ = issue_state_rest(&root, 9);
        let _ = issue_state_rest(&root, 9);
    });
    assert!(forge.repo_reads() > before, "suspect → confirm: {:?}", forge.all_calls());
}

/// A `304` serves a stored body, not this request's answer: even one naming
/// another repo never casts doubt on the record.
#[test]
#[serial(loom_config_env)]
fn a_not_modified_body_is_never_observed() {
    let (env, root, forge) = fixture(&[]);
    let store_dir = env.tmp.path().join("store");
    store::set_test_daemon_store_dir(Some(store_dir.clone()));
    forge.set("item", &issue_body(9, "open", REPO));
    run(&forge, || assert_eq!(issue_state_rest(&root, 9), "OPEN"));
    let before = forge.repo_reads();
    let target = store::Target {
        repo: Some(REPO.to_string()),
        host: Some("github.com".to_string()),
    };
    let url = format!("repos/{REPO}/issues/9");
    let entry = crate::forge_cached_view::entry_path(
        &store_dir,
        "issue",
        9,
        &store::cache_key(Some(&root), &target, &url),
    );
    store::write_disk_entry(
        &entry,
        &store::DiskEntry {
            etag: r#"W/"i1""#.to_string(),
            body: issue_body(9, "open", "acme/renamed"),
        },
    );
    forge.set("notmodified", "");
    run(&forge, || {
        let _ = issue_state_rest(&root, 9);
        let _ = issue_state_rest(&root, 9);
    });
    store::set_test_daemon_store_dir(None);
    let calls = forge.item_calls();
    assert!(calls[1..].iter().all(|c| c.contains("If-None-Match")), "{calls:?}");
    assert_eq!(forge.repo_reads(), before, "no confirm read: {:?}", forge.all_calls());
}
