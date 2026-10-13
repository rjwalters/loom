//! #11766: the auto-filed red-main fix issue — decision, parsing, and the
//! reconcile against a fake forge.

use super::*;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const NEWER: &str = "fedcba9876543210fedcba9876543210fedcba98";

fn red(sha: &str) -> Verdict {
    Verdict::Red(RedEvidence {
        source: Source::Ci,
        sha: Some(sha.to_string()),
        failing: vec!["CI".to_string(), "Lint".to_string()],
        run_url: Some("https://github.com/o/r/actions/runs/1".to_string()),
        gate_tail: None,
    })
}

fn green() -> Verdict {
    Verdict::Green {
        source: Source::Ci,
        sha: Some(NEWER.to_string()),
    }
}

fn auto_body(sha: &str) -> String {
    body_for(
        &RedEvidence {
            source: Source::Ci,
            sha: Some(sha.to_string()),
            failing: Vec::new(),
            run_url: None,
            gate_tail: None,
        },
        "main",
    )
}

fn open(number: u32, sha: &str) -> OpenAuto {
    OpenAuto {
        number,
        sha: Some(sha.to_string()),
        has_open_pr: false,
    }
}

#[test]
fn red_with_no_issue_creates_one_with_both_markers_and_labels() {
    let actions = decide(&red(SHA), &[], "main");
    let [Action::Create {
        title,
        body,
        labels,
    }] = actions.as_slice()
    else {
        panic!("expected one create: {actions:?}");
    };
    assert_eq!(title, "Fix red main CI (0123456)");
    assert_eq!(labels, &["loom:issue", "loom:very-important"]);
    assert!(is_auto_body(body), "{body}");
    assert_eq!(body_sha(body).as_deref(), Some(SHA));
    assert!(body.contains("CI, Lint"), "{body}");
    assert!(body.contains("actions/runs/1"), "{body}");
    // The lane reads its marker line-anchored: the created issue is a fix.
    let item = crate::work_finder::WorkItem::new(1, labels.clone()).with_body(Some(body.clone()));
    assert!(item.is_main_red_fix());
}

#[test]
fn red_with_an_existing_issue_reuses_it() {
    let actions = decide(&red(SHA), &[open(7, SHA)], "main");
    assert_eq!(
        actions,
        vec![Action::Reuse {
            number: 7,
            newer_sha: None
        }]
    );
    let actions = decide(&red(NEWER), &[open(7, SHA)], "main");
    assert_eq!(
        actions,
        vec![Action::Reuse {
            number: 7,
            newer_sha: Some(NEWER.to_string())
        }]
    );
}

#[test]
fn green_closes_our_issue_unless_a_pr_is_open() {
    let mut with_pr = open(9, SHA);
    with_pr.has_open_pr = true;
    let actions = decide(&green(), &[open(7, SHA), with_pr], "main");
    let [Action::Close { number, comment }] = actions.as_slice() else {
        panic!("expected one close: {actions:?}");
    };
    assert_eq!(*number, 7);
    assert!(comment.contains(NEWER), "{comment}");
    assert!(decide(&green(), &[], "main").is_empty());
}

#[test]
fn duplicates_keep_the_oldest() {
    let actions = decide(&red(SHA), &[open(12, SHA), open(10, SHA), open(11, SHA)], "main");
    assert_eq!(
        actions,
        vec![
            Action::Reuse {
                number: 10,
                newer_sha: None
            },
            Action::CloseDuplicate { number: 11, of: 10 },
            Action::CloseDuplicate { number: 12, of: 10 },
        ]
    );
}

#[test]
fn unknown_does_nothing() {
    assert!(decide(&Verdict::Unknown, &[open(7, SHA)], "main").is_empty());
    assert!(decide(&Verdict::Unknown, &[], "main").is_empty());
}

#[test]
fn the_listing_keeps_only_issues_with_both_markers() {
    let fix_only = format!("{MAIN_RED_FIX_MARKER}\nfiled by a person");
    let quoted = format!("prose mentioning {MAIN_RED_FIX_MARKER} {MAIN_RED_AUTO_MARKER}");
    let json = serde_json::json!([
        {"number": 1, "body": auto_body(SHA)},
        {"number": 2, "body": fix_only},
        {"number": 3, "body": quoted},
        {"number": 4, "body": auto_body(SHA), "pull_request": {}},
        {"number": 5, "body": null},
    ])
    .to_string();
    let listed = parse_listing(&json).unwrap();
    assert_eq!(listed.iter().map(|i| i.number).collect::<Vec<_>>(), vec![1]);
    assert!(parse_listing("{}").is_none());
}

#[test]
fn ci_verdict_reads_the_newest_commit() {
    let run = |sha: &str, status: &str, conclusion: &str, wf: &str| {
        serde_json::json!({"headSha": sha, "status": status, "conclusion": conclusion,
            "workflowName": wf, "url": format!("https://x/{wf}")})
    };
    let failing = serde_json::json!([
        run(SHA, "completed", "failure", "CI"),
        run(SHA, "completed", "success", "Docs"),
        run(NEWER, "completed", "success", "CI"),
    ]);
    let Verdict::Red(e) = ci_verdict(&failing.to_string()) else {
        panic!("red expected");
    };
    assert_eq!(e.sha.as_deref(), Some(SHA));
    assert_eq!(e.failing, vec!["CI".to_string()]);
    assert_eq!(e.run_url.as_deref(), Some("https://x/CI"));

    let passing = serde_json::json!([
        run(NEWER, "completed", "success", "CI"),
        run(NEWER, "completed", "cancelled", "Docs"),
        run(SHA, "completed", "failure", "CI"),
    ]);
    assert_eq!(ci_verdict(&passing.to_string()), green());

    let running = serde_json::json!([
        run(NEWER, "in_progress", "", "CI"),
        run(NEWER, "completed", "success", "Docs"),
    ]);
    assert_eq!(ci_verdict(&running.to_string()), Verdict::Unknown);
    assert_eq!(ci_verdict("[]"), Verdict::Unknown);
    assert_eq!(ci_verdict("not json"), Verdict::Unknown);
}

#[test]
fn the_gate_decides_when_it_is_on() {
    let on_red = RedMainLane {
        verified_red: true,
        ..RedMainLane::default()
    };
    let note = Some(GateNote::Red {
        tail: "test failed".to_string(),
    });
    let v = verdict_for(on_red, note, || panic!("CI not read"), || Some(SHA.to_string()));
    let Verdict::Red(e) = v else {
        panic!("red expected")
    };
    assert_eq!(e.source, Source::Gate);
    assert_eq!(e.gate_tail.as_deref(), Some("test failed"));
    assert!(body_for(&e, "main").contains("Gate output (tail)"));

    let on = RedMainLane::default();
    assert_eq!(
        verdict_for(on, None, || panic!("CI not read"), || None),
        Verdict::Unknown,
        "no gate verdict yet is unknown, never green"
    );
    assert!(matches!(
        verdict_for(on, Some(GateNote::Green), || panic!("CI not read"), || None),
        Verdict::Green {
            source: Source::Gate,
            ..
        }
    ));
    let off = RedMainLane {
        gate_disabled: true,
        ..RedMainLane::default()
    };
    assert_eq!(verdict_for(off, Some(GateNote::Green), || red(SHA), || None), red(SHA));
}

#[test]
fn tail_of_keeps_the_end() {
    assert_eq!(tail_of("abc", 10), "abc");
    assert_eq!(tail_of("abcdef", 3), "…def");
}

/// A forge that records every write.
#[derive(Default)]
struct FakeForge {
    open: Vec<ListedIssue>,
    with_pr: HashSet<u32>,
    comments: HashMap<u32, Vec<String>>,
    next: u32,
    created: Vec<(String, Vec<String>)>,
    closed: Vec<(u32, bool)>,
    list_fails: bool,
}

impl AutofileForge for FakeForge {
    fn list_open_auto(&mut self) -> Result<Vec<ListedIssue>, String> {
        if self.list_fails {
            return Err("boom".to_string());
        }
        Ok(self.open.clone())
    }
    fn open_prs(&mut self, issues: &[u32]) -> HashSet<u32> {
        issues
            .iter()
            .copied()
            .filter(|n| self.with_pr.contains(n))
            .collect()
    }
    fn create(&mut self, title: &str, body: &str, labels: &[String]) -> Result<u32, String> {
        self.next += 1;
        self.created.push((title.to_string(), labels.to_vec()));
        self.open.push(ListedIssue {
            number: self.next,
            body: body.to_string(),
        });
        Ok(self.next)
    }
    fn comments(&mut self, issue: u32) -> Result<Vec<String>, String> {
        Ok(self.comments.get(&issue).cloned().unwrap_or_default())
    }
    fn comment(&mut self, issue: u32, body: &str) -> Result<(), String> {
        self.comments
            .entry(issue)
            .or_default()
            .push(body.to_string());
        Ok(())
    }
    fn close(&mut self, issue: u32, not_planned: bool) -> Result<(), String> {
        self.closed.push((issue, not_planned));
        self.open.retain(|i| i.number != issue);
        Ok(())
    }
}

#[test]
fn reconcile_creates_once_then_reuses_then_closes_on_green() {
    let mut forge = FakeForge {
        next: 100,
        ..FakeForge::default()
    };
    let mut memory = Memory::default();
    let (applied, _) = reconcile(&mut forge, &red(SHA), "main", &mut memory).unwrap();
    assert_eq!(
        applied,
        vec![Applied {
            action: "created",
            issue: 101
        }]
    );
    // The next tick finds it and creates nothing.
    let (applied, open) = reconcile(&mut forge, &red(SHA), "main", &mut memory).unwrap();
    assert!(applied.is_empty(), "{applied:?}");
    assert_eq!((open, forge.created.len()), (1, 1));
    // Red at a newer commit: one comment naming it, however many ticks.
    reconcile(&mut forge, &red(NEWER), "main", &mut memory).unwrap();
    reconcile(&mut forge, &red(NEWER), "main", &mut Memory::default()).unwrap();
    let notes = &forge.comments[&101];
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains(NEWER));
    // Green: closed as completed, with a comment.
    let (applied, _) = reconcile(&mut forge, &green(), "main", &mut memory).unwrap();
    assert_eq!(
        applied,
        vec![Applied {
            action: "closed",
            issue: 101
        }]
    );
    assert_eq!(forge.closed, vec![(101, false)]);
}

#[test]
fn reconcile_reports_a_found_issue_as_reused_once_per_host() {
    let mut forge = FakeForge {
        open: vec![ListedIssue {
            number: 7,
            body: auto_body(SHA),
        }],
        ..FakeForge::default()
    };
    let mut memory = Memory::default();
    let (applied, _) = reconcile(&mut forge, &red(SHA), "main", &mut memory).unwrap();
    assert_eq!(
        applied,
        vec![Applied {
            action: "reused",
            issue: 7
        }]
    );
    let (applied, _) = reconcile(&mut forge, &red(SHA), "main", &mut memory).unwrap();
    assert!(applied.is_empty());
    assert!(forge.created.is_empty());
}

#[test]
fn reconcile_closes_the_newer_duplicate_and_keeps_a_pr_backed_issue_on_green() {
    let mut forge = FakeForge {
        open: vec![
            ListedIssue {
                number: 21,
                body: auto_body(SHA),
            },
            ListedIssue {
                number: 20,
                body: auto_body(SHA),
            },
        ],
        ..FakeForge::default()
    };
    let (applied, _) = reconcile(&mut forge, &red(SHA), "main", &mut Memory::default()).unwrap();
    assert!(applied.contains(&Applied {
        action: "closed_duplicate",
        issue: 21
    }));
    assert_eq!(forge.closed, vec![(21, true)]);
    assert!(forge.comments[&21][0].contains("Duplicate of #20"));

    forge.with_pr.insert(20);
    let (applied, open) = reconcile(&mut forge, &green(), "main", &mut Memory::default()).unwrap();
    assert!(applied.is_empty(), "{applied:?}");
    assert_eq!(open, 1);
}

#[test]
fn reconcile_creates_nothing_when_the_listing_fails_or_the_verdict_is_unknown() {
    let mut forge = FakeForge {
        list_fails: true,
        ..FakeForge::default()
    };
    assert!(reconcile(&mut forge, &red(SHA), "main", &mut Memory::default()).is_err());
    forge.list_fails = false;
    let (applied, _) =
        reconcile(&mut forge, &Verdict::Unknown, "main", &mut Memory::default()).unwrap();
    assert!(applied.is_empty());
    assert!(forge.created.is_empty());
}

#[test]
fn scheduling_waits_out_the_interval_but_follows_a_gate_change() {
    let now = Instant::now();
    let interval = Duration::from_secs(600);
    let mut s = Schedule::default();
    assert!(due(&s, now, interval, None), "first look is due");
    assert!(!due(&s, now, interval, Some(0)), "unknown gate verdict never is");
    s.last_at = Some(now);
    s.last_kind = Verdict::Unknown.kind();
    assert!(!due(&s, now, interval, None));
    assert!(due(&s, now, interval, Some(red(SHA).kind())), "gate turned red");
    s.last_kind = red(SHA).kind();
    assert!(!due(&s, now, interval, Some(red(SHA).kind())));
    assert!(due(&s, now + interval, interval, Some(red(SHA).kind())));

    s.green_and_none = true;
    s.last_listing = Some(now);
    assert!(!green_needs_listing(&s, now + interval));
    assert!(green_needs_listing(&s, now + GREEN_RELIST_EVERY));
}

#[test]
fn a_gate_verdict_is_noted_per_root() {
    use crate::main_health_gate::{GateOutcome, UnevaluatedClass};
    let root = tempfile::tempdir().unwrap();
    note_gate_outcome(root.path(), &GateOutcome::red("cargo test failed"));
    assert_eq!(
        gate_note(root.path()),
        Some(GateNote::Red {
            tail: "cargo test failed".to_string()
        })
    );
    note_gate_outcome(root.path(), &GateOutcome::unevaluated(UnevaluatedClass::Timeout, "slow"));
    assert!(matches!(gate_note(root.path()), Some(GateNote::Red { .. })));
    note_gate_outcome(
        root.path(),
        &GateOutcome::Green {
            elapsed: Duration::ZERO,
        },
    );
    assert_eq!(gate_note(root.path()), Some(GateNote::Green));
}

#[test]
fn a_change_emits_one_main_red_autofile_span() {
    use crate::observability::ops::capture;
    let (_, captured) = capture::capture(|| {
        emit(
            "o/r",
            &red(SHA),
            &Applied {
                action: "created",
                issue: 5,
            },
        );
    });
    let spans = captured.spans;
    assert_eq!(spans.len(), 1, "{spans:?}");
    let span = &spans[0];
    assert_eq!(span.name.as_str(), "main_red.autofile");
    let attr = |k: &str| span.attributes.get(k).map(String::as_str);
    assert_eq!(attr("loom.repo"), Some("o/r"));
    assert_eq!(attr("loom.issue"), Some("5"));
    assert_eq!(attr("loom.main_red.sha"), Some(SHA));
    assert_eq!(attr("loom.main_red.action"), Some("created"));
    assert_eq!(attr("loom.main_red.source"), Some("ci"));
}
