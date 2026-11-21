//! Tests for the ratification lane's marker posting and tick (#13345).

use super::*;
use crate::champion_hold::ratification::{ChangedFile, Comment};
use crate::role_runner::ratification_dispatch::InstructionSource;
use crate::role_runner::ratification_lane::{
    keys_marked, run_lane, CommentSink, Config, Key, OpenPr, PrSource,
};
use serde_json::json;
use std::cell::RefCell;

const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const EE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MKT_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn cfg() -> Config {
    Config::from_effective(&json!({"ratificationLane": {
        "enabled": true,
        "ee": {"repo": "k/i", "path": "ee.md", "commit": EE_SHA},
        "market": {"repo": "k/i", "path": "market.md", "commit": MKT_SHA}
    }}))
}

struct Prs {
    author: &'static str,
    /// Calls to `open_prs`, to prove a disabled lane never touches the forge.
    listed: RefCell<u32>,
}

impl PrSource for Prs {
    fn open_prs(&self) -> Vec<OpenPr> {
        *self.listed.borrow_mut() += 1;
        vec![OpenPr {
            number: 7,
            author: self.author.into(),
            head_sha: HEAD.into(),
        }]
    }
    fn files(
        &self,
        _pr: u64,
        _hold: &crate::champion_hold::ratification::Config,
    ) -> Vec<ChangedFile> {
        vec![ChangedFile {
            filename: "ratification/requests/a.md".into(),
            previous_filename: None,
            status: "added".into(),
            head_content: Some("Ratified only on operator merge.".into()),
        }]
    }
    fn trusted_comments(&self, _pr: u64) -> Result<Vec<Comment>, String> {
        Ok(Vec::new())
    }
}

struct Instr {
    fail_market: bool,
}

impl InstructionSource for Instr {
    fn fetch(&self, pin: &crate::role_runner::ratification_lane::Pin) -> Result<String, String> {
        if self.fail_market && pin.path == "market.md" {
            Err("HTTP 404".into())
        } else {
            Ok("INSTR".into())
        }
    }
    fn repo_is_private(&self, _repo: &str) -> Option<bool> {
        Some(true)
    }
}

#[derive(Default)]
struct Sink {
    posted: RefCell<Vec<(u64, String)>>,
}

impl CommentSink for Sink {
    fn post(&self, pr: u64, body: &str) -> Result<(), String> {
        self.posted.borrow_mut().push((pr, body.to_string()));
        Ok(())
    }
}

struct Runner {
    outcome: RoleTickOutcome,
    calls: u32,
    /// Overrides `last_output` for both keys; `None` returns a valid review
    /// for the key that ran last.
    output: Option<String>,
    last_key: std::cell::Cell<Option<Key>>,
}

impl RoleInvocationRunner for Runner {
    fn invoke(&mut self, role: &str, _prompt: &str) -> RoleTickOutcome {
        self.calls += 1;
        self.last_key.set(Some(if role == "ratification-market" {
            Key::Market
        } else {
            Key::Ee
        }));
        self.outcome.clone()
    }
    fn last_output(&self) -> Option<String> {
        Some(match &self.output {
            Some(o) => o.clone(),
            None => valid_review(self.last_key.get().unwrap_or(Key::Ee)),
        })
    }
}

fn ok_runner() -> Runner {
    Runner {
        outcome: RoleTickOutcome::Success,
        calls: 0,
        output: None,
        last_key: std::cell::Cell::new(None),
    }
}

/// A well-formed review for `key` per the pinned output contract (#13762).
fn valid_review(key: Key) -> String {
    match key {
        Key::Ee => {
            "## EE-key review: sg13g2 comparator\n\n**Overall verdict:** approve, small and sane.\n"
                .into()
        }
        Key::Market => {
            "## Market-key review: sg13g2 comparator\n\n**Overall verdict:** competitive, priced below alternates.\n"
                .into()
        }
    }
}

/// Launcher/wrapper lines that must never reach a posted comment body.
const LAUNCHER_NOISE: &str = "spawn-claude preflight: token pool ok\n\
     Permission deny rule fired (Bash)\n\
     auth mode: OAuth (len=118 tail=abcd)\n\
     reaped process group 1234\n";

/// Launcher/wrapper lines the wrapper writes *after* the model's final
/// message — the measured tail of a real role log (#13762). The review ends
/// at their first line; they must never reach a posted comment body.
const WRAPPER_TAIL: &str = "[2026-10-15T00:38:41Z] [INFO] Claude CLI completed successfully\n\
     [2026-10-15T00:38:41Z] [INFO] Claude wrapper exiting with code 0\n\
     [claude-wrapper] reaping 3 residual process(es) in process group 4124342 at exit (#6192): 77007 77203 77205\n";

fn comments(sink: &Sink) -> Vec<Comment> {
    sink.posted
        .borrow()
        .iter()
        .map(|(_, b)| Comment::new("bot[bot]", b))
        .collect()
}

#[test]
fn one_candidate_posts_one_marker_per_key_with_selected_head() {
    let (prs, sink, mut r) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
        ok_runner(),
    );
    run_lane(&cfg(), None, "o/r", &prs, &Instr { fail_market: false }, &sink, &mut r);
    let posted = sink.posted.borrow();
    assert_eq!(posted.len(), 2);
    assert!(posted[0].1.contains("reviewer=ratification-ee"));
    assert!(posted[1].1.contains("reviewer=ratification-market"));
    drop(posted);
    let keys = keys_marked(&comments(&sink), HEAD);
    assert!(keys.contains(&Key::Ee) && keys.contains(&Key::Market));
}

#[test]
fn failed_closed_key_posts_nothing_and_other_key_is_unaffected() {
    let (prs, sink, mut r) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
        ok_runner(),
    );
    run_lane(&cfg(), None, "o/r", &prs, &Instr { fail_market: true }, &sink, &mut r);
    assert_eq!(keys_marked(&comments(&sink), HEAD), vec![Key::Ee]);
    assert_eq!(sink.posted.borrow().len(), 1);
}

#[test]
fn non_success_session_fails_closed_without_marker() {
    let (prs, sink) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
    );
    let mut r = Runner {
        outcome: RoleTickOutcome::Failure("boom".into()),
        calls: 0,
        output: None,
        last_key: std::cell::Cell::new(None),
    };
    run_lane(&cfg(), None, "o/r", &prs, &Instr { fail_market: false }, &sink, &mut r);
    assert_eq!(r.calls, 2);
    assert!(sink.posted.borrow().is_empty());
}

#[test]
fn marker_carries_selected_head_so_a_push_mid_review_is_reselected() {
    // The head is captured by selection; the marker names it even if the PR
    // has since moved, so the new head has no marker and is a candidate again.
    let (prs, sink, mut r) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
        ok_runner(),
    );
    run_lane(&cfg(), None, "o/r", &prs, &Instr { fail_market: false }, &sink, &mut r);
    let new_head = "f".repeat(40);
    assert!(keys_marked(&comments(&sink), &new_head).is_empty());
    assert_eq!(keys_marked(&comments(&sink), HEAD).len(), 2);
}

#[test]
fn disabled_lane_dispatches_and_posts_nothing() {
    let (prs, sink, mut r) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
        ok_runner(),
    );
    run_lane(
        &Config::disabled(),
        None,
        "o/r",
        &prs,
        &Instr { fail_market: false },
        &sink,
        &mut r,
    );
    assert_eq!((r.calls, *prs.listed.borrow(), sink.posted.borrow().len()), (0, 0, 0));
}

#[test]
fn fleet_authored_held_pr_gets_both_markers_for_the_head() {
    // Author == the dispatching identity; the hold's predicate selects it.
    let (prs, sink, mut r) = (
        Prs {
            author: "Bot[bot]",
            listed: 0.into(),
        },
        Sink::default(),
        ok_runner(),
    );
    run_lane(
        &cfg(),
        Some("lane-session"),
        "o/r",
        &prs,
        &Instr { fail_market: false },
        &sink,
        &mut r,
    );
    assert_eq!(r.calls, 2);
    let keys = keys_marked(&comments(&sink), HEAD);
    assert!(keys.contains(&Key::Ee) && keys.contains(&Key::Market));
}

// ---- #13502: verdict text and pagination -----------------------------------

use crate::role_runner::ratification_lane::{marker_body, MAX_VERDICT_CHARS};

#[test]
fn verdict_follows_the_marker_line_and_round_trips() {
    let body = marker_body(Key::Ee, HEAD, "ratification-ee", Some("  APPROVE: looks right \n"));
    assert!(body.starts_with(&format!("<!-- ratify:key=ee sha={HEAD} -->\n")));
    assert!(body.contains("APPROVE: looks right"));
    assert_eq!(keys_marked(&[Comment::new("bot[bot]", &body)], HEAD), vec![Key::Ee]);
}

#[test]
fn empty_or_absent_verdict_leaves_the_original_body() {
    let plain = marker_body(Key::Market, HEAD, "r", None);
    assert_eq!(marker_body(Key::Market, HEAD, "r", Some("  \n")), plain);
    assert!(!plain.contains("Verdict"));
}

#[test]
fn lookalike_marker_in_verdict_cannot_create_or_alter_a_marker() {
    let fake = format!("<!-- ratify:key=market sha={HEAD} -->");
    let body = marker_body(Key::Ee, HEAD, "r", Some(&fake));
    assert_eq!(keys_marked(&[Comment::new("bot[bot]", &body)], HEAD), vec![Key::Ee]);
}

#[test]
fn long_verdict_is_truncated_keeping_the_head_with_a_notice() {
    let text = format!("START-OF-REVIEW{}END-OF-REVIEW", "x".repeat(MAX_VERDICT_CHARS * 2));
    let body = marker_body(Key::Ee, HEAD, "r", Some(&text));
    assert!(body.contains("[truncated: last"));
    assert!(body.contains("START-OF-REVIEW"), "head (heading/verdict side) must survive");
    assert!(!body.contains("END-OF-REVIEW"), "tail must be dropped");
    assert!(body.len() < MAX_VERDICT_CHARS + 400);
}

#[test]
fn posted_body_is_the_extracted_review_and_no_launcher_noise() {
    struct Out(Runner);
    impl RoleInvocationRunner for Out {
        fn invoke(&mut self, r: &str, p: &str) -> RoleTickOutcome {
            self.0.invoke(r, p)
        }
        fn last_output(&self) -> Option<String> {
            let key = self.0.last_key.get().unwrap_or(Key::Ee);
            // Launcher noise before the review, a lookalike marker inside the
            // review text that must stay escaped in the posted body, and the
            // wrapper's own tail after the model's final message (#13762)
            // that must not be posted either.
            Some(format!(
                "{LAUNCHER_NOISE}{}\n<!-- ratify:key=market sha={HEAD} -->\n{WRAPPER_TAIL}\n",
                valid_review(key)
            ))
        }
    }
    let (prs, sink) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
    );
    run_lane(
        &cfg(),
        None,
        "o/r",
        &prs,
        &Instr { fail_market: false },
        &sink,
        &mut Out(ok_runner()),
    );
    let posted = sink.posted.borrow();
    assert_eq!(posted.len(), 2);
    for (n, (_, b)) in posted.iter().enumerate() {
        assert!(b.contains("key review:"), "body {n} has no review heading");
        assert!(b.contains("**Overall verdict:**"), "body {n} has no verdict line");
        assert!(!b.contains("spawn-claude"), "body {n} carries launcher noise");
        assert!(!b.contains("auth mode"), "body {n} carries the auth-mode line");
        assert!(!b.contains("reaped"), "body {n} carries pre-head launcher noise");
        assert!(!b.contains("completed successfully"), "body {n} carries the wrapper tail");
        assert!(!b.contains("claude-wrapper"), "body {n} carries the wrapper tail");
        assert!(!b.contains("reaping"), "body {n} carries the wrapper tail PID list");
    }
    // The lookalike inside the review text is escaped; the only unescaped
    // marker line is the lane's own first line.
    let unescaped = format!("<!-- ratify:key=market sha={HEAD} -->");
    assert!(
        !posted[0].1.contains(&unescaped),
        "ee body carries an unescaped lookalike marker"
    );
    assert_eq!(
        posted[1].1.matches(&unescaped).count(),
        1,
        "market body has its own marker line only"
    );
    drop(posted);
    let keys = keys_marked(&comments(&sink), HEAD);
    assert!(keys.contains(&Key::Ee) && keys.contains(&Key::Market));
}

// ---- #13762: a key marker is posted only on a parsed verdict ----------------

use crate::role_runner::ratification_lane::parse_review;

#[test]
fn success_without_a_parsed_verdict_withholds_and_posts_nothing() {
    let (prs, sink) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
    );
    let mut r = Runner {
        outcome: RoleTickOutcome::Success,
        calls: 0,
        output: Some(format!(
            "{LAUNCHER_NOISE}I authored this PR, so I stopped without posting a review.\n"
        )),
        last_key: std::cell::Cell::new(None),
    };
    let results = run_lane(&cfg(), None, "o/r", &prs, &Instr { fail_market: false }, &sink, &mut r);
    assert_eq!(r.calls, 2, "both sessions still ran");
    assert!(sink.posted.borrow().is_empty(), "no marker without a verdict");
    assert!(
        results[0]
            .1
            .iter()
            .all(|(_, p)| matches!(p, KeyPost::Withheld(_))),
        "{:?}",
        results
    );
    assert!(keys_marked(&comments(&sink), HEAD).is_empty());
}

#[test]
fn a_withheld_key_does_not_suppress_a_later_rerun_at_the_same_head() {
    let sink = Sink::default();
    let live = Live {
        inner: Prs {
            author: "alice",
            listed: 0.into(),
        },
        sink: &sink,
    };
    let mut r = Runner {
        outcome: RoleTickOutcome::Success,
        calls: 0,
        output: Some("stopped without a verdict".into()),
        last_key: std::cell::Cell::new(None),
    };
    let instr = Instr { fail_market: false };
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!((r.calls, sink.posted.borrow().len()), (2, 0));
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(r.calls, 4, "no marker exists, so the same head is selected again");
    assert!(sink.posted.borrow().is_empty());
}

#[test]
fn one_key_with_a_verdict_and_one_without_post_independently() {
    struct Mixed(Runner);
    impl RoleInvocationRunner for Mixed {
        fn invoke(&mut self, r: &str, p: &str) -> RoleTickOutcome {
            self.0.invoke(r, p)
        }
        fn last_output(&self) -> Option<String> {
            match self.0.last_key.get() {
                Some(Key::Market) => Some("declined: disclosure problem".into()),
                _ => Some(valid_review(Key::Ee)),
            }
        }
    }
    let (prs, sink) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
    );
    let results = run_lane(
        &cfg(),
        None,
        "o/r",
        &prs,
        &Instr { fail_market: false },
        &sink,
        &mut Mixed(ok_runner()),
    );
    assert_eq!(keys_marked(&comments(&sink), HEAD), vec![Key::Ee]);
    assert_eq!(sink.posted.borrow().len(), 1);
    assert_eq!(results[0].1[0].1, KeyPost::Posted);
    assert!(matches!(results[0].1[1].1, KeyPost::Withheld(_)));
}

#[test]
fn parse_review_takes_the_last_heading_and_ignores_noise_before_it() {
    let out = format!(
        "echo ## EE-key review: quoted\n**Overall verdict:** escalate, noise\n{}tail kept\n",
        valid_review(Key::Ee)
    );
    let r = parse_review(Key::Ee, &out).expect("last heading wins");
    assert_eq!(r.token, "approve");
    assert!(r.text.starts_with("## EE-key review: sg13g2 comparator"));
    assert!(r.text.ends_with("tail kept"));
    assert!(!r.text.contains("quoted"));
}

#[test]
fn parse_review_accepts_both_token_terminators() {
    let comma = "## EE-key review: b\n**Overall verdict:** approve, why\n";
    let space = "## EE-key review: b\n**Overall verdict:** request-changes because\n";
    assert_eq!(parse_review(Key::Ee, comma).unwrap().token, "approve");
    assert_eq!(parse_review(Key::Ee, space).unwrap().token, "request-changes");
}

#[test]
fn parse_review_ends_at_the_wrapper_tail_not_end_of_output() {
    // The real log shape (#13762): the model's review, then the wrapper's
    // own lines written after the final message.
    let out = format!("{}{}\n", valid_review(Key::Ee), WRAPPER_TAIL);
    let r = parse_review(Key::Ee, &out).expect("verdict parsed");
    assert_eq!(r.token, "approve");
    assert!(r.text.starts_with("## EE-key review:"));
    assert!(r.text.ends_with("small and sane."));
    assert!(!r.text.contains("completed successfully"), "{:?}", r.text);
    assert!(!r.text.contains("claude-wrapper"), "{:?}", r.text);
    assert!(!r.text.contains("reaping"), "{:?}", r.text);
    // A timestamped line mid-review ends the review there too.
    let out = format!(
        "{}[2026-11-17T12:05:00Z] [ERROR] probe\nmore prose after the tail line\n",
        valid_review(Key::Ee)
    );
    let r = parse_review(Key::Ee, &out).expect("verdict parsed");
    assert!(!r.text.contains("more prose after the tail line"), "{:?}", r.text);
}

#[test]
fn parse_review_rejects_missing_heading_or_verdict_line() {
    let e = parse_review(Key::Ee, "stopped without a verdict").unwrap_err();
    assert!(e.contains("## EE-key review:"), "{e}");
    // The other key's heading is not this key's.
    let e = parse_review(Key::Ee, &valid_review(Key::Market)).unwrap_err();
    assert!(e.contains("## EE-key review:"), "{e}");
    let e = parse_review(Key::Market, "## Market-key review: b\nno verdict line\n").unwrap_err();
    assert!(e.contains("**Overall verdict:**"), "{e}");
}

#[test]
fn parse_review_rejects_unknown_or_conflicting_tokens() {
    let review = |t: &str| format!("## EE-key review: b\n**Overall verdict:** {t}, why\n");
    let e = parse_review(Key::Ee, &review("thumbs-up")).unwrap_err();
    assert!(e.contains("thumbs-up"), "{e}");
    // A market token on an ee review is unknown, and vice versa.
    assert!(parse_review(Key::Ee, &review("competitive")).is_err());
    assert!(parse_review(Key::Market, &valid_review(Key::Ee)).is_err());
    let both = "## EE-key review: b\n**Overall verdict:** approve, a\n**Overall verdict:** request-changes, b\n";
    let e = parse_review(Key::Ee, both).unwrap_err();
    assert!(e.contains("conflicting"), "{e}");
}

fn page(range: std::ops::Range<u32>) -> String {
    serde_json::to_string(&range.map(|n| json!({"number": n})).collect::<Vec<_>>()).unwrap()
}

#[test]
fn paginated_listing_is_flattened_across_pages() {
    use crate::comment_trust::parse_listing;
    let two = format!("{}{}", page(0..100), page(100..101));
    assert_eq!(parse_listing(two.as_bytes()).unwrap().len(), 101);
    assert_eq!(parse_listing(page(0..100).as_bytes()).unwrap().len(), 100);
    let with_empty = format!("{}[]", page(0..100));
    assert_eq!(parse_listing(with_empty.as_bytes()).unwrap().len(), 100);
}

#[test]
fn unreadable_second_page_fails_closed() {
    use crate::comment_trust::parse_listing;
    let torn = format!("{}{{\"message\":\"boom\"}}", page(0..100));
    assert!(parse_listing(torn.as_bytes()).is_none());
    assert!(parse_listing(b"").is_none());
}

// ---- #14797: a completed key is not rerun on the same head ------------------

/// A runner whose output depends on the key that ran last.
struct KeyedRunner {
    ee: String,
    market: String,
    calls: Vec<String>,
}

impl RoleInvocationRunner for KeyedRunner {
    fn invoke(&mut self, role: &str, _prompt: &str) -> RoleTickOutcome {
        self.calls.push(role.to_string());
        RoleTickOutcome::Success
    }
    fn last_output(&self) -> Option<String> {
        Some(match self.calls.last() {
            Some(r) if r == "ratification-market" => self.market.clone(),
            _ => self.ee.clone(),
        })
    }
}

/// An instruction source that counts fetches (by pin path) and visibility
/// reads, so a partial-key retry can prove it reads only the missing key's
/// instructions and does market-only visibility work only when market runs.
#[derive(Default)]
struct CountingInstr {
    fetches: RefCell<Vec<String>>,
    visibility: RefCell<u32>,
    private: Option<bool>,
}

impl InstructionSource for CountingInstr {
    fn fetch(&self, pin: &crate::role_runner::ratification_lane::Pin) -> Result<String, String> {
        self.fetches.borrow_mut().push(pin.path.clone());
        Ok("INSTR".into())
    }
    fn repo_is_private(&self, _repo: &str) -> Option<bool> {
        *self.visibility.borrow_mut() += 1;
        self.private
    }
}

#[test]
fn partial_key_retry_runs_and_posts_only_the_missing_key() {
    let sink = Sink::default();
    let live = Live {
        inner: Prs {
            author: "alice",
            listed: 0.into(),
        },
        sink: &sink,
    };
    let instr = Instr { fail_market: false };
    // Tick 1: both keys missing; market ends in a valid verdict, ee declines.
    let mut r = KeyedRunner {
        ee: "declined without a verdict".into(),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(r.calls, vec!["ratification-ee", "ratification-market"]);
    assert_eq!(sink.posted.borrow().len(), 1);
    assert!(sink.posted.borrow()[0].1.contains("ratify:key=market"));

    // Tick 2: the trusted market marker is visible on the same head, so only
    // the missing ee key may run again — not the completed market key.
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(r.calls.len(), 3, "only the missing key reruns; calls so far: {:?}", r.calls);
    assert_eq!(r.calls[2], "ratification-ee");
    assert_eq!(sink.posted.borrow().len(), 1, "no duplicate market marker");
}

#[test]
fn unreadable_forge_comment_rows_stop_dispatch() {
    use crate::comment_trust::TrustPolicy;
    use crate::forge_identity::{FleetLogins, Placement, RouteDecision};
    use crate::gh_invocation::cwd_route::{CwdAnswer, DeriveEnv};
    use crate::gh_invocation::{test_routing, test_support};
    use crate::role_runner::ratification_lane::{marker_body, run_lane_stats, GhPrSource};

    struct CommentReader<'a> {
        prs: Prs,
        forge: GhPrSource<'a>,
    }

    impl PrSource for CommentReader<'_> {
        fn open_prs(&self) -> Vec<OpenPr> {
            self.prs.open_prs()
        }
        fn files(
            &self,
            pr: u64,
            hold: &crate::champion_hold::ratification::Config,
        ) -> Vec<ChangedFile> {
            self.prs.files(pr, hold)
        }
        fn trusted_comments(&self, pr: u64) -> Result<Vec<Comment>, String> {
            self.forge.trusted_comments(pr)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let reader = dir.path().join("reader");
    std::fs::create_dir(&reader).unwrap();
    let program = test_support::stub(
        dir.path(),
        "gh-comments",
        r#"test "$1" = api && test "$2" = 'repos/o/r/issues/7/comments?per_page=100' && test "$3" = --paginate || exit 64
cat "${0%/*}/reply"
test ! -f "${0%/*}/failed""#,
    );
    let _routing = test_routing::install_with_program(
        test_routing::TestRouting {
            decision: RouteDecision::Reader {
                dir: reader,
                app_id: "fixture-reader".into(),
                placement: Placement::Home,
            },
            env: DeriveEnv::default(),
            checkout: CwdAnswer::Unresolved,
        },
        program,
    );
    let cfg = cfg();
    let policy = TrustPolicy::new(FleetLogins::single("fleet-bot"), None, Vec::new());
    let source = CommentReader {
        prs: Prs {
            author: "alice",
            listed: 0.into(),
        },
        forge: GhPrSource::new(dir.path(), "o/r", &policy),
    };
    let marker =
        marker_body(Key::Market, HEAD, "ratification-market", Some(&valid_review(Key::Market)));
    let marked = json!([{
        "user":{"login":"fixture-owner","type":"User"}, "author_association":"OWNER", "body":marker
    }]);
    let ordinary = json!([
        {"user":{"login":"fixture-owner","type":"User"}, "author_association":"OWNER", "body":"ordinary discussion"},
        {"user":{"login":"unknown[bot]","type":"Bot"}, "author_association":"NONE", "body":marker},
        {"user":null, "author_association":"NONE", "body":marker}
    ]);
    let missing_body = json!([{
        "user":{"login":"fixture-owner","type":"User"}, "author_association":"OWNER", "body":null
    }]);
    let bare_row = json!([{"message":"incomplete comment record"}]);
    let run =
        |label, listing: String, failed: bool, expected_pins: &[&str], expected_unreadable| {
            std::fs::write(dir.path().join("reply"), listing).unwrap();
            let failure = dir.path().join("failed");
            if failed {
                std::fs::write(&failure, "").unwrap();
            }
            let instructions = CountingInstr {
                private: Some(true),
                ..Default::default()
            };
            let sink = Sink::default();
            let mut runner = ok_runner();
            let (_, _, unreadable) =
                run_lane_stats(&cfg, None, "o/r", &source, &instructions, &sink, &mut runner);
            if failed {
                std::fs::remove_file(failure).unwrap();
            }
            assert_eq!(unreadable, expected_unreadable, "{label}: unreadable PRs");
            assert_eq!(*instructions.fetches.borrow(), expected_pins, "{label}: instructions");
            assert_eq!(
                *instructions.visibility.borrow(),
                u32::from(expected_pins.contains(&"market.md")),
                "{label}: visibility reads"
            );
            assert_eq!(runner.calls as usize, expected_pins.len(), "{label}: sessions");
            assert_eq!(sink.posted.borrow().len(), expected_pins.len(), "{label}: marker posts");
        };
    let both = ["ee.md", "market.md"];
    run("empty listing", "[]".into(), false, &both, 0);
    run("readable ordinary and untrusted rows", ordinary.to_string(), false, &both, 0);
    run("trusted marker", marked.to_string(), false, &["ee.md"], 0);
    run("failed read", marked.to_string(), true, &[], 1);
    run("invalid later page", format!("{marked}\n[{{"), false, &[], 1);
    run("malformed row", bare_row.to_string(), false, &[], 1);
    run("trusted row without a readable body", missing_body.to_string(), false, &[], 1);
    run(
        "valid marker before malformed row",
        format!("{marked}\n{missing_body}"),
        false,
        &[],
        1,
    );
}

#[test]
fn partial_key_retry_reads_only_the_missing_keys_instructions() {
    let sink = Sink::default();
    let live = Live {
        inner: Prs {
            author: "alice",
            listed: 0.into(),
        },
        sink: &sink,
    };
    let instr = CountingInstr {
        private: Some(false),
        ..Default::default()
    };
    let mut r = KeyedRunner {
        ee: "declined without a verdict".into(),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    let run = |r: &mut KeyedRunner| {
        run_lane(&cfg(), None, "o/r", &live, &instr, &sink, r);
    };
    // Tick 1: both keys missing — both instructions read, EE first.
    run(&mut r);
    assert_eq!(*instr.fetches.borrow(), vec!["ee.md", "market.md"]);
    assert_eq!(*instr.visibility.borrow(), 1, "one market visibility read");
    assert_eq!(sink.posted.borrow().len(), 1);

    // Tick 2: market complete, ee still declining. One missing-key
    // instruction fetch, one role call, no new marker, no visibility read.
    run(&mut r);
    assert_eq!(*instr.fetches.borrow(), vec!["ee.md", "market.md", "ee.md"]);
    assert_eq!(r.calls.last().map(String::as_str), Some("ratification-ee"));
    assert_eq!(sink.posted.borrow().len(), 1);
    assert_eq!(*instr.visibility.borrow(), 1);

    // Tick 3: ee finally reviews — only its marker, at the captured head.
    r.ee = valid_review(Key::Ee);
    run(&mut r);
    assert_eq!(sink.posted.borrow().len(), 2);
    assert!(sink.posted.borrow()[1].1.contains("ratify:key=ee"));
    assert!(sink.posted.borrow()[1].1.contains(HEAD));

    // Tick 4: both keys complete — zero instruction reads, role calls, posts.
    let fetches_before = instr.fetches.borrow().len();
    let calls_before = r.calls.len();
    run(&mut r);
    assert_eq!(instr.fetches.borrow().len(), fetches_before);
    assert_eq!(r.calls.len(), calls_before);
    assert_eq!(sink.posted.borrow().len(), 2);
}

#[test]
fn partial_key_retry_works_in_both_directions() {
    // The other direction: ee completes first, market withholds and is then
    // retried alone; its session keeps the public-source constraint.
    let sink = Sink::default();
    let live = Live {
        inner: Prs {
            author: "alice",
            listed: 0.into(),
        },
        sink: &sink,
    };
    let instr = CountingInstr {
        private: Some(false),
        ..Default::default()
    };
    let mut r = KeyedRunner {
        ee: valid_review(Key::Ee),
        market: "declined without a verdict".into(),
        calls: Vec::new(),
    };
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(sink.posted.borrow().len(), 1);
    assert!(sink.posted.borrow()[0].1.contains("ratify:key=ee"));

    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(r.calls.len(), 3, "only market reruns: {:?}", r.calls);
    assert_eq!(r.calls[2], "ratification-market");
    assert_eq!(*instr.fetches.borrow(), vec!["ee.md", "market.md", "market.md"]);
    assert_eq!(*instr.visibility.borrow(), 2, "each market session reads visibility");
    assert_eq!(sink.posted.borrow().len(), 1, "no duplicate ee marker");
}

/// A forge whose comment read can fail, and whose trusted comments can be
/// seeded with raw records filtered through the real trust policy.
struct Feed {
    author: &'static str,
    head: std::cell::Cell<String>,
    /// Raw REST comment records, filtered through `TrustPolicy` on read.
    records: RefCell<Vec<serde_json::Value>>,
    fail_comments: Option<String>,
}

impl PrSource for Feed {
    fn open_prs(&self) -> Vec<OpenPr> {
        vec![OpenPr {
            number: 7,
            author: self.author.into(),
            head_sha: self.head.replace(String::new()),
        }]
    }
    fn files(
        &self,
        _pr: u64,
        _hold: &crate::champion_hold::ratification::Config,
    ) -> Vec<ChangedFile> {
        vec![ChangedFile {
            filename: "ratification/requests/a.md".into(),
            previous_filename: None,
            status: "added".into(),
            head_content: Some("Ratified only on operator merge.".into()),
        }]
    }
    fn trusted_comments(&self, _pr: u64) -> Result<Vec<Comment>, String> {
        if let Some(why) = &self.fail_comments {
            return Err(why.clone());
        }
        let policy = crate::comment_trust::TrustPolicy::new(
            crate::forge_identity::FleetLogins::single("fleet-bot"),
            None,
            Vec::new(),
        );
        Ok(self
            .records
            .borrow()
            .iter()
            .filter(|c| policy.trusts_json(c))
            .filter_map(Comment::from_record)
            .collect())
    }
}

impl Feed {
    fn new() -> Self {
        Feed {
            author: "alice",
            head: std::cell::Cell::new(HEAD.into()),
            records: RefCell::new(Vec::new()),
            fail_comments: None,
        }
    }

    /// A raw comment record as the REST listing reports it.
    fn record(login: &str, association: &str, body: &str) -> serde_json::Value {
        serde_json::json!({
            "user": {"login": login, "type": "Bot"},
            "author_association": association,
            "body": body,
        })
    }

    /// A hand-lane marker in the accepted wire format (#14797), optionally
    /// carrying a recorded review.
    fn seed_marker(
        &self,
        login: &str,
        association: &str,
        key: &str,
        sha: &str,
        verdict: Option<&str>,
    ) {
        let body = match verdict {
            Some(v) => format!(
                "<!-- ratify:key={key} sha={sha} -->\nreviewer=hand-run-lane\n\nVerdict:\n\n{v}\n"
            ),
            None => format!("<!-- ratify:key={key} sha={sha} -->\nreviewer=hand-run-lane\n"),
        };
        self.records
            .borrow_mut()
            .push(Self::record(login, association, &body));
    }
}

#[test]
fn a_trusted_hand_lane_marker_completes_only_its_key() {
    let feed = Feed::new();
    // A trusted fleet App's hand-run marker completes market — even though
    // its recorded review was negative (`escalate`).
    feed.seed_marker(
        "fleet-bot[bot]",
        "NONE",
        "market",
        HEAD,
        Some("## Market-key review: x\n\n**Overall verdict:** escalate, niche.\n"),
    );
    // An otherwise identical lookalike from an untrusted author is prose and
    // completes nothing: it goes through the actual trust filter above.
    feed.seed_marker("stranger[bot]", "NONE", "ee", HEAD, None);

    let sink = Sink::default();
    let instr = CountingInstr::default();
    let mut r = KeyedRunner {
        ee: valid_review(Key::Ee),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    let results = run_lane(&cfg(), None, "o/r", &feed, &instr, &sink, &mut r);
    assert_eq!(r.calls, vec!["ratification-ee"], "only ee was missing");
    assert_eq!(sink.posted.borrow().len(), 1);
    assert!(sink.posted.borrow()[0].1.contains("ratify:key=ee"));
    // The completed key contributes neither a post nor a withheld count.
    assert_eq!(results[0].1.len(), 1);
    assert_eq!(results[0].1[0], (Key::Ee, KeyPost::Posted));
}

#[test]
fn unreadable_comment_list_runs_posts_and_counts_nothing() {
    let feed = Feed {
        fail_comments: Some("exit 1: HTTP 502".into()),
        ..Feed::new()
    };
    let sink = Sink::default();
    let instr = CountingInstr::default();
    let mut r = ok_runner();
    // A failed or malformed paginated read: zero instruction fetches, role
    // calls and posts, and the PR counts as unreadable (degraded), never as
    // "no markers" authorizing a duplicate run.
    let (results, skipped, unreadable) =
        run_lane_stats(&cfg(), None, "o/r", &feed, &instr, &sink, &mut r);
    assert_eq!((results.len(), skipped, unreadable), (0, 0, 1));
    assert_eq!(r.calls, 0);
    assert!(sink.posted.borrow().is_empty());
    assert!(instr.fetches.borrow().is_empty());
    let o = TickOutcome::from_run_stats(&results, skipped, unreadable);
    assert_eq!(o.reason, "degraded");
}

#[test]
fn a_verified_empty_comment_list_still_selects_both_keys() {
    // `Ok(vec![])` is a successfully read empty list, not an unreadable one.
    let feed = Feed::new();
    let sink = Sink::default();
    let instr = CountingInstr::default();
    let mut r = KeyedRunner {
        ee: valid_review(Key::Ee),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    run_lane(&cfg(), None, "o/r", &feed, &instr, &sink, &mut r);
    assert_eq!(r.calls, vec!["ratification-ee", "ratification-market"]);
    assert_eq!(sink.posted.borrow().len(), 2);
}

#[test]
fn a_moved_head_loses_both_keys_even_with_old_markers() {
    let feed = Feed::new();
    feed.seed_marker("fleet-bot[bot]", "NONE", "ee", HEAD, None);
    feed.seed_marker("fleet-bot[bot]", "NONE", "market", HEAD, None);
    let new_head = "f".repeat(40);
    feed.head.set(new_head.clone());

    let sink = Sink::default();
    let instr = CountingInstr::default();
    let mut r = KeyedRunner {
        ee: valid_review(Key::Ee),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    run_lane(&cfg(), None, "o/r", &feed, &instr, &sink, &mut r);
    assert_eq!(r.calls.len(), 2, "both keys are missing at the new head");
    assert_eq!(sink.posted.borrow().len(), 2);
    for (_, body) in sink.posted.borrow().iter() {
        assert!(body.contains(&new_head), "markers name the new head: {body}");
    }
}

#[test]
fn a_failed_post_for_the_missing_key_leaves_the_completed_key_alone() {
    /// A sink that refuses every post.
    struct Refusing;
    impl CommentSink for Refusing {
        fn post(&self, _pr: u64, _body: &str) -> Result<(), String> {
            Err("HTTP 403".into())
        }
    }

    let feed = Feed::new();
    feed.seed_marker("fleet-bot[bot]", "NONE", "market", HEAD, None);
    let sink = Refusing;
    let instr = CountingInstr::default();
    let mut r = KeyedRunner {
        ee: valid_review(Key::Ee),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    let results = run_lane(&cfg(), None, "o/r", &feed, &instr, &sink, &mut r);
    assert_eq!(r.calls, vec!["ratification-ee"]);
    match results[0].1.as_slice() {
        [(Key::Ee, KeyPost::Withheld(reason))] => {
            assert!(reason.contains("post failed"), "{reason}")
        }
        other => panic!("expected the failed ee post, got {other:?}"),
    }
    // Withheld stays scoped to the attempted key: the completed market key
    // contributes nothing, and a later tick retries ee alone.
    let o = TickOutcome::from_run_stats(&results, 0, 0);
    assert_eq!((o.reason, o.selected, o.ran, o.posted, o.withheld), ("ran", 1, 1, 0, 1));
}

// ---- tick outcome + shared entry points (#13707) ----

use crate::role_runner::ratification_lane::{
    run_lane_counted, run_lane_stats, tick_span, KeyPost, TickOutcome, STEP_TICK_INTERVAL,
};

/// A forge whose trusted comments are whatever the sink has posted so far.
struct Live<'a> {
    inner: Prs,
    sink: &'a Sink,
}

impl PrSource for Live<'_> {
    fn open_prs(&self) -> Vec<OpenPr> {
        self.inner.open_prs()
    }
    fn files(
        &self,
        pr: u64,
        hold: &crate::champion_hold::ratification::Config,
    ) -> Vec<ChangedFile> {
        self.inner.files(pr, hold)
    }
    fn trusted_comments(&self, _pr: u64) -> Result<Vec<Comment>, String> {
        Ok(comments(self.sink))
    }
}

#[test]
fn outcome_counts_selected_ran_posted() {
    let (prs, sink, mut r) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
        ok_runner(),
    );
    let (results, skipped) =
        run_lane_counted(&cfg(), None, "o/r", &prs, &Instr { fail_market: false }, &sink, &mut r);
    let o = TickOutcome::from_run(&results, skipped);
    assert_eq!((o.reason, o.selected, o.skipped, o.ran, o.posted), ("ran", 1, 0, 1, 2));
    assert!(results[0].1.iter().all(|(_, p)| *p == KeyPost::Posted));
}

#[test]
fn nothing_to_do_is_a_distinct_outcome() {
    let o = TickOutcome::from_run(&[], 0);
    assert_eq!((o.reason, o.selected, o.ran, o.posted), ("nothing_to_do", 0, 0, 0));
}

#[test]
fn second_pass_over_a_marked_head_posts_nothing() {
    let sink = Sink::default();
    let live = Live {
        inner: Prs {
            author: "alice",
            listed: 0.into(),
        },
        sink: &sink,
    };
    let mut r = ok_runner();
    let instr = Instr { fail_market: false };
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(sink.posted.borrow().len(), 2);
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(sink.posted.borrow().len(), 2, "no double-posting of keys");
    assert_eq!(r.calls, 2, "no second reviewer dispatch");
}

#[test]
fn tick_span_carries_reason_and_counts() {
    let o = TickOutcome::from_run(&[], 3);
    let span = tick_span(std::path::Path::new("/r"), "step_dispatch", &o, chrono::Utc::now());
    assert_eq!(span.name.as_str(), "ratification_lane.tick");
    // Assert through `.bounded()`: the export allowlist drops unregistered keys.
    let span = span.bounded();
    for (k, v) in [
        ("loom.ratification.reason", "nothing_to_do"),
        ("loom.ratification.source", "step_dispatch"),
        ("loom.ratification.selected", "0"),
        ("loom.ratification.skipped", "3"),
        ("loom.ratification.ran", "0"),
        ("loom.ratification.posted", "0"),
        ("loom.ratification.unreadable", "0"),
        ("loom.ratification.withheld", "0"),
    ] {
        assert_eq!(span.attributes.get(k).map(String::as_str), Some(v), "{k}");
    }
}

#[test]
fn disabled_tick_is_recorded_with_its_reason() {
    // The temp root has no config: the lane is disabled and says so.
    let dir = std::env::temp_dir().join(format!("ratif-tick-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut r = ok_runner();
    let o = crate::role_runner::ratification_lane::tick_for_root(&dir, &mut r);
    assert_eq!((o.reason, r.calls), ("disabled", 0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn step_cadence_ticks_once_per_interval_per_root() {
    use std::time::{Duration, Instant};
    let mut last = std::collections::HashMap::new();
    let (a, b) = (std::path::Path::new("/a"), std::path::Path::new("/b"));
    let t0 = Instant::now();
    assert!(crate::role_runner::ratification_lane::step_tick_due_for_test(&mut last, a, t0));
    assert!(!crate::role_runner::ratification_lane::step_tick_due_for_test(
        &mut last,
        a,
        t0 + Duration::from_secs(1)
    ));
    assert!(crate::role_runner::ratification_lane::step_tick_due_for_test(&mut last, b, t0));
    assert!(crate::role_runner::ratification_lane::step_tick_due_for_test(
        &mut last,
        a,
        t0 + STEP_TICK_INTERVAL
    ));
}

#[test]
fn tick_with_unreadable_prs_is_degraded_not_nothing_to_do() {
    let o = TickOutcome::from_run_stats(&[], 0, 2);
    assert_eq!((o.reason, o.selected, o.unreadable), ("degraded", 0, 2));
    let span =
        tick_span(std::path::Path::new("/r"), "step_dispatch", &o, chrono::Utc::now()).bounded();
    assert_eq!(
        span.attributes
            .get("loom.ratification.unreadable")
            .map(String::as_str),
        Some("2")
    );
    assert_eq!(
        span.attributes
            .get("loom.ratification.reason")
            .map(String::as_str),
        Some("degraded")
    );
}

#[test]
fn withheld_keys_are_counted() {
    let results = vec![(
        5,
        vec![
            (Key::Ee, KeyPost::Withheld("could not resolve".into())),
            (Key::Market, KeyPost::Posted),
        ],
    )];
    let o = TickOutcome::from_run_stats(&results, 0, 0);
    assert_eq!((o.reason, o.posted, o.withheld), ("ran", 1, 1));
}

#[cfg(unix)]
mod gh_failure {
    use crate::cmd_out::{CmdOutcome, Unavailable};
    use crate::role_runner::ratification_dispatch::gh_failure_detail;
    use std::os::unix::process::ExitStatusExt;
    use std::time::Duration;

    fn ran(code: i32, stderr: &str) -> CmdOutcome {
        CmdOutcome::Ran(std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    #[test]
    fn nonzero_exit_carries_status_and_stderr() {
        let d = gh_failure_detail(&ran(1, "gh: Not Found (HTTP 404)\n"));
        assert_eq!(d, "exit 1: gh: Not Found (HTTP 404)");
        assert_eq!(gh_failure_detail(&ran(4, "")), "exit 4, no stderr");
    }

    #[test]
    fn stderr_is_bounded_and_token_shaped_words_redacted() {
        let d = gh_failure_detail(&ran(1, &format!("bad ghp_SECRET123 {}", "x".repeat(500))));
        assert!(!d.contains("SECRET123"));
        assert!(d.contains("[redacted]"));
        assert!(d.ends_with("..."));
        assert!(d.chars().count() < 230);
    }

    #[test]
    fn spawn_failure_and_timeout_are_distinct_from_a_nonzero_exit() {
        let spawn = gh_failure_detail(&CmdOutcome::Unavailable(Unavailable::Spawn("no gh".into())));
        let timeout = gh_failure_detail(&CmdOutcome::Unavailable(Unavailable::TimedOut {
            after: Duration::from_secs(120),
            partial_stdout: Vec::new(),
        }));
        assert!(spawn.starts_with("could not start"));
        assert!(timeout.starts_with("timed out"));
        assert_ne!(spawn, timeout);
        assert!(!spawn.starts_with("exit"));
    }
}

// ---- #14834: the read-only status report -------------------------------------

use crate::role_runner::ratification_lane::{
    resolve_status, scan_markers, GhPrSource, MarkerComment, MarkerFormat,
};

/// The head both real PR #98 fixture markers name.
const DROP_HEAD: &str = "6a3cd2c43c31821fe93bbc18828c0eb6e06aa063";

/// The lane's own wire marker, as the daemon posts it (PR #98 shape).
fn lane_marker(key: &str) -> String {
    format!("<!-- ratify:key={key} sha={DROP_HEAD} -->\nreviewer=ratification-{key}\n")
}

/// The hand-applied marker, as observed on PR #98.
fn hand_marker(key: &str, verdict: &str) -> String {
    format!(
        "<!-- RATIFY-KEY: {key} verdict={verdict} block=o/r#98 \
sha={DROP_HEAD} reviewer=01a1fa5f-0873-7071-a338-184a562cfa55 date=2026-11-20T19:14:24Z -->"
    )
}

fn mc(id: u64, at: &str, body: &str) -> MarkerComment {
    MarkerComment {
        id,
        created_at: chrono::DateTime::parse_from_rfc3339(at)
            .unwrap()
            .with_timezone(&chrono::Utc),
        url: format!("https://example/pull/98#issuecomment-{id}"),
        body: body.into(),
    }
}

#[test]
fn status_picks_the_newest_marker_per_key_and_links_the_rest_once_as_superseded() {
    let comments = vec![
        mc(1, "2026-11-20T19:00:37Z", &lane_marker("market")),
        mc(2, "2026-11-20T19:05:00Z", &lane_marker("market")),
        mc(3, "2026-11-20T19:14:25Z", &hand_marker("market", "adequate-for-catalog")),
        mc(4, "2026-11-20T18:00:00Z", &lane_marker("ee")),
    ];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    let market = &s.market;
    assert_eq!(market.effective.as_ref().unwrap().comment_id, 3);
    assert_eq!(market.effective.as_ref().unwrap().format, MarkerFormat::Hand);
    assert_eq!(
        market
            .superseded
            .iter()
            .map(|l| l.comment_id)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "superseded links are exact and oldest first"
    );
    assert_eq!(s.ee.effective.as_ref().unwrap().comment_id, 4);
    assert!(s.ee.superseded.is_empty());

    // Reversing the fixture moves nothing: ordering is the forge's creation
    // time, never the listing's order.
    let mut reversed = comments.clone();
    reversed.reverse();
    assert_eq!(resolve_status(DROP_HEAD, "o/r", 98, &reversed), s);
}

#[test]
fn equal_creation_times_are_broken_by_comment_id_across_zone_formats() {
    // 20:00:00+01:00 is 19:00:00Z: same instant, different notation — the
    // numeric id decides.
    let comments = vec![
        mc(11, "2026-11-20T19:00:00Z", &lane_marker("ee")),
        mc(12, "2026-11-20T20:00:00+01:00", &lane_marker("ee")),
    ];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    assert_eq!(s.ee.effective.as_ref().unwrap().comment_id, 12);
    assert_eq!(
        s.ee.superseded
            .iter()
            .map(|l| l.comment_id)
            .collect::<Vec<_>>(),
        vec![11]
    );
}

#[test]
fn a_more_favorable_verdict_gives_a_comment_no_priority() {
    let comments = vec![
        mc(1, "2026-11-20T19:00:00Z", &hand_marker("ee", "approve")),
        mc(2, "2026-11-20T19:30:00Z", &hand_marker("ee", "request-changes")),
    ];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    assert_eq!(s.ee.effective.as_ref().unwrap().comment_id, 2);
}

#[test]
fn both_marker_formats_compete_on_the_same_terms_in_either_direction() {
    let nwo = "o/r";
    // The real #98 pair: the lane marker came first, the hand marker later.
    let a = vec![
        mc(7048105117, "2026-11-20T19:00:37Z", &lane_marker("market")),
        mc(
            7048390167,
            "2026-11-20T19:14:25Z",
            &hand_marker("market", "adequate-for-catalog"),
        ),
    ];
    let s = resolve_status(DROP_HEAD, nwo, 98, &a);
    assert_eq!(s.market.effective.as_ref().unwrap().comment_id, 7048390167);
    assert_eq!(
        s.market
            .superseded
            .iter()
            .map(|l| l.comment_id)
            .collect::<Vec<_>>(),
        vec![7048105117]
    );
    // The mirror: a lane marker newer than a hand marker wins the same way.
    let b = vec![
        mc(1, "2026-11-20T19:00:37Z", &hand_marker("market", "competitive")),
        mc(2, "2026-11-20T19:14:25Z", &lane_marker("market")),
    ];
    let s = resolve_status(DROP_HEAD, nwo, 98, &b);
    assert_eq!(s.market.effective.as_ref().unwrap().format, MarkerFormat::Lane);
}

#[test]
fn stale_head_and_foreign_pr_markers_are_neither_effective_nor_superseded() {
    let stale = format!("<!-- ratify:key=ee sha={} -->", "f".repeat(40));
    let foreign_repo =
        format!("<!-- RATIFY-KEY: ee verdict=approve block=other/repo#98 sha={DROP_HEAD} -->");
    let foreign_pr =
        format!("<!-- RATIFY-KEY: ee verdict=approve block=o/r#99 sha={DROP_HEAD} -->");
    let own = format!("<!-- RATIFY-KEY: ee verdict=approve block=o/r#98 sha={DROP_HEAD} -->");
    let comments = vec![
        mc(1, "2026-11-20T19:00:00Z", &stale),
        mc(2, "2026-11-20T19:01:00Z", &foreign_repo),
        mc(3, "2026-11-20T19:02:00Z", &foreign_pr),
        mc(4, "2026-11-20T19:03:00Z", &own),
    ];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    assert_eq!(s.ee.effective.as_ref().unwrap().comment_id, 4);
    assert!(
        s.ee.superseded.is_empty(),
        "markers for another head or PR are not this head's markers"
    );
}

#[test]
fn quoted_and_fenced_lookalikes_never_establish_a_head_or_key() {
    let quoted = format!("> quoted example:\n> <!-- ratify:key=ee sha={DROP_HEAD} -->");
    let fenced = format!("```\n<!-- RATIFY-KEY: ee verdict=approve sha={DROP_HEAD} -->\n```");
    let lane_body = lane_marker("ee");
    let lane = lane_body.lines().next().unwrap();
    let hand = hand_marker("ee", "approve");
    let examples = [
        quoted,
        fenced,
        format!("Example: `{lane}`"),
        format!("`{hand}`"),
        format!("{lane} trailing prose"),
        format!("{hand} {lane}"),
    ];
    for example in &examples {
        assert!(scan_markers(example).is_empty(), "{example}");
    }
    let comments = vec![
        mc(1, "2026-11-20T19:00:00Z", &lane_body),
        mc(2, "2026-11-20T19:01:00Z", &examples.join("\n")),
    ];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    assert_eq!(s.ee.effective.unwrap().comment_id, 1);
    assert!(s.ee.superseded.is_empty());
    assert_eq!(scan_markers(&hand).len(), 1);
}

#[test]
fn indented_markdown_code_cannot_select_a_marker() {
    let lane_body = lane_marker("ee");
    let lane = lane_body.lines().next().unwrap();
    let hand = hand_marker("market", "approve");
    for marker in [lane, hand.as_str()] {
        assert_eq!(scan_markers(&format!("   {marker}")).len(), 1);
        for indent in ["    ", "     ", "\t", " \t", "   \t"] {
            assert!(scan_markers(&format!("{indent}{marker}")).is_empty());
        }
        assert_eq!(scan_markers(&format!("    ```\n\n{marker}")).len(), 1);
        assert!(scan_markers(&format!("```\n    ```\n{marker}\n```")).is_empty());
    }

    let comments = vec![
        mc(1, "2026-11-20T19:00:00Z", &format!("{lane}\n{hand}")),
        mc(2, "2026-11-20T19:01:00Z", &format!("Example:\n\n    {lane}\n\n\t{hand}")),
    ];
    let status = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    for key in [status.ee, status.market] {
        assert_eq!(key.effective.unwrap().comment_id, 1);
        assert!(key.superseded.is_empty());
    }
}

#[test]
fn a_hand_marker_without_a_full_matching_sha_is_not_a_marker() {
    let short = format!("<!-- RATIFY-KEY: ee verdict=approve sha={} -->", "abc".repeat(7));
    let no_sha = "<!-- RATIFY-KEY: ee verdict=approve -->\n";
    assert!(scan_markers(&short).is_empty());
    assert!(scan_markers(no_sha).is_empty());
}

#[test]
fn a_missing_key_is_explicit_and_a_verified_empty_history_is_no_marker() {
    let empty = resolve_status(DROP_HEAD, "o/r", 98, &[]);
    assert!(empty.ee.effective.is_none() && empty.market.effective.is_none());
    let comments = vec![mc(1, "2026-11-20T19:00:00Z", &lane_marker("market"))];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    assert!(s.ee.effective.is_none(), "ee is missing at this head");
    assert!(s.market.effective.is_some());
}

#[test]
fn a_comment_carrying_several_markers_for_one_key_links_once() {
    let body = format!("{}\n{}", lane_marker("ee"), hand_marker("ee", "approve"));
    let comments = vec![mc(1, "2026-11-20T19:00:00Z", &body)];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    assert_eq!(s.ee.effective.as_ref().unwrap().comment_id, 1);
    assert!(s.ee.superseded.is_empty());
}

#[test]
fn status_reads_head_and_history_through_the_forge_and_performs_only_reads() {
    use crate::cli::ratification_lane_cmd::{handle, Action};
    use crate::comment_trust::TrustPolicy;
    use crate::forge_identity::{FleetLogins, Placement, RouteDecision};
    use crate::gh_invocation::cwd_route::{CwdAnswer, DeriveEnv};
    use crate::gh_invocation::{test_routing, test_support};

    let dir = tempfile::tempdir().unwrap();
    let reader = dir.path().join("reader");
    std::fs::create_dir(&reader).unwrap();
    let calls = dir.path().join("calls");
    let program = test_support::stub(
        dir.path(),
        "gh-status",
        &format!(
            r#"printf '%s\n' "$*" >> '{calls}'
test "$1" = api || exit 64
case "$2" in
  repos/o/r/pulls/98) test ! -f "${{0%/*}}/prfail" || exit 1; cat "${{0%/*}}/pr" ;;
  'repos/o/r/issues/98/comments?per_page=100') test "$3" = --paginate || exit 64; cat "${{0%/*}}/reply"; test ! -f "${{0%/*}}/failed" ;;
  *) exit 64 ;;
esac"#,
            calls = calls.display(),
        ),
    );
    let _routing = test_routing::install_with_program(
        test_routing::TestRouting {
            decision: RouteDecision::Reader {
                dir: reader,
                app_id: "fixture-reader".into(),
                placement: Placement::Home,
            },
            env: DeriveEnv::default(),
            checkout: CwdAnswer::Unresolved,
        },
        program,
    );
    let policy = TrustPolicy::new(FleetLogins::single("fleet-bot"), None, Vec::new());
    let source = GhPrSource::new(dir.path(), "o/r", &policy);

    let record = |id: u64, at: &str, login: &str, assoc: &str, r#type: &str, body: &str| {
        json!({
            "id": id,
            "created_at": at,
            "html_url": format!("https://example/o/r/pull/98#issuecomment-{id}"),
            "user": {"login": login, "type": r#type},
            "author_association": assoc,
            "body": body,
        })
    };
    let pr = json!({"number": 98, "head": {"sha": DROP_HEAD}, "base": {"sha": "0".repeat(40)}});

    // Two pages carrying several market AND ee markers. The newest market
    // marker (the hand marker) is on page one, the newest ee marker on page
    // two: page and listing position never decide, creation time does.
    let page_one = json!([
        record(3, "2026-11-20T19:14:25Z", "fixture-owner", "OWNER", "User", &hand_marker("market", "adequate-for-catalog")),
        { "id": 4, "created_at": "2026-11-20T19:20:00Z", "html_url": "u4",
          "user": {"login": "unknown[bot]", "type": "Bot"}, "author_association": "NONE",
          "body": lane_marker("market") },
        { "id": 5, "created_at": "2026-11-20T19:21:00Z", "html_url": "u5",
          "user": null, "author_association": "NONE", "body": lane_marker("ee") }
    ]);
    let page_two = json!([
        record(
            7048105117,
            "2026-11-20T19:00:37Z",
            "fixture-owner",
            "OWNER",
            "User",
            &lane_marker("market")
        ),
        record(
            7048105999,
            "2026-11-20T18:30:00Z",
            "fixture-owner",
            "OWNER",
            "User",
            &hand_marker("ee", "approve")
        ),
        record(
            7048106111,
            "2026-11-20T18:40:00Z",
            "fixture-owner",
            "OWNER",
            "User",
            &lane_marker("ee")
        )
    ]);
    std::fs::write(dir.path().join("pr"), pr.to_string()).unwrap();
    std::fs::write(dir.path().join("reply"), format!("{page_one}\n{page_two}")).unwrap();

    let head = source.pr_head(98).expect("head");
    assert_eq!(head, DROP_HEAD);
    let comments = source.marker_comments(98).expect("comments");
    // Untrusted and deleted-author rows are gone; trusted identity survives.
    assert_eq!(
        comments.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![3, 7048105117, 7048105999, 7048106111]
    );
    assert_eq!(comments[0].url, "https://example/o/r/pull/98#issuecomment-3");
    let s = resolve_status(&head, "o/r", 98, &comments);
    // Market: the hand marker on page one is newer than the lane marker on
    // page two — creation time, not page or listing position, decides.
    assert_eq!(s.market.effective.as_ref().unwrap().comment_id, 3);
    assert_eq!(
        s.market
            .superseded
            .iter()
            .map(|l| l.comment_id)
            .collect::<Vec<_>>(),
        vec![7048105117]
    );
    // EE: the newest ee marker is on page two and wins; the deleted-author
    // ee marker (id 5) never counted.
    assert_eq!(s.ee.effective.as_ref().unwrap().comment_id, 7048106111);
    assert_eq!(
        s.ee.superseded
            .iter()
            .map(|l| l.comment_id)
            .collect::<Vec<_>>(),
        vec![7048105999]
    );

    // A torn second page fails the whole read closed.
    std::fs::write(dir.path().join("reply"), format!("{page_one}\n[{{")).unwrap();
    assert!(source.marker_comments(98).is_err());

    // A verified empty listing reads as empty — both keys genuinely missing,
    // not unreadable.
    std::fs::write(dir.path().join("reply"), "[]").unwrap();
    let empty = source
        .marker_comments(98)
        .expect("empty history is readable");
    assert!(empty.is_empty());
    let s = resolve_status(&head, "o/r", 98, &empty);
    assert!(s.ee.effective.is_none() && s.market.effective.is_none());

    // A malformed trusted record (no id to link) fails the read closed.
    let malformed = json!([
        { "created_at": "2026-11-20T19:00:00Z", "html_url": "u9",
          "user": {"login": "fixture-owner", "type": "User"},
          "author_association": "OWNER", "body": lane_marker("market") }
    ]);
    std::fs::write(dir.path().join("reply"), malformed.to_string()).unwrap();
    let err = source.marker_comments(98).unwrap_err();
    assert!(err.contains("malformed"), "{err}");

    // A failed gh read of either endpoint is an error, never empty.
    std::fs::write(dir.path().join("reply"), format!("{page_one}\n{page_two}")).unwrap();
    std::fs::write(dir.path().join("failed"), "").unwrap();
    assert!(source.marker_comments(98).is_err());
    std::fs::remove_file(dir.path().join("failed")).unwrap();
    std::fs::write(dir.path().join("prfail"), "").unwrap();
    assert!(source.pr_head(98).is_err());
    std::fs::remove_file(dir.path().join("prfail")).unwrap();

    // A malformed captured head stops the actual status command before it
    // reads comment history, even when that history is readable.
    std::fs::write(dir.path().join("reply"), "[]").unwrap();
    for invalid in [
        "".to_string(),
        "a".repeat(39),
        "a".repeat(41),
        "g".repeat(40),
    ] {
        let pr = json!({"head": {"sha": invalid}});
        std::fs::write(dir.path().join("pr"), pr.to_string()).unwrap();
        let before = std::fs::read_to_string(&calls).unwrap();
        let error = handle(Action::Status {
            pr: 98,
            repo: Some("o/r".to_string()),
            root: Some(dir.path().to_path_buf()),
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("head could not be read") && error.contains("40-hex"), "{error}");
        let after = std::fs::read_to_string(&calls).unwrap();
        assert_eq!(after.strip_prefix(&before), Some("api repos/o/r/pulls/98\n"));
    }

    // Every forge call was a read: `gh api <endpoint>` with no method verb.
    let seen = std::fs::read_to_string(&calls).unwrap();
    for line in seen.lines() {
        assert!(line.starts_with("api "), "non-api call: {line}");
        assert!(!line.contains("-X") && !line.contains("--method"), "write call: {line}");
    }
}
