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
    fn trusted_comments(&self, _pr: u64) -> Vec<Comment> {
        Vec::new()
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
}

impl RoleInvocationRunner for Runner {
    fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
        self.calls += 1;
        self.outcome.clone()
    }
}

fn ok_runner() -> Runner {
    Runner {
        outcome: RoleTickOutcome::Success,
        calls: 0,
    }
}

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
fn long_verdict_is_truncated_keeping_the_end_with_a_notice() {
    let text = format!("{}END", "x".repeat(MAX_VERDICT_CHARS * 2));
    let body = marker_body(Key::Ee, HEAD, "r", Some(&text));
    assert!(body.contains("[truncated: first"));
    assert!(body.contains("xEND"));
    assert!(body.len() < MAX_VERDICT_CHARS + 400);
}

#[test]
fn posted_marker_carries_the_session_output() {
    struct Out(Runner);
    impl RoleInvocationRunner for Out {
        fn invoke(&mut self, r: &str, p: &str) -> RoleTickOutcome {
            self.0.invoke(r, p)
        }
        fn last_output(&self) -> Option<String> {
            Some("VERDICT-TEXT".into())
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
    assert!(posted.iter().all(|(_, b)| b.contains("VERDICT-TEXT")));
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
