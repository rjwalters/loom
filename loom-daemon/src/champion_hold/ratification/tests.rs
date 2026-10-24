use super::*;

const HEAD: &str = "abc1234";

fn file(name: &str, status: &str, content: Option<&str>) -> ChangedFile {
    ChangedFile {
        filename: name.into(),
        previous_filename: None,
        status: status.into(),
        head_content: content.map(str::to_string),
    }
}

fn held(files: &[ChangedFile]) -> bool {
    matches!(scan(files, &Config::default()), Verdict::Hold(_))
}

/// Minimal reconstructions of the 2026-10-20 shadow-run fixtures. The PRs'
/// contents live in private chip repos and are not copied here; each fixture
/// carries the ratification-by-merge wording its spec used, in the path layout
/// the repo used.
const FIXTURES: [(&str, &str, &str); 8] = [
    (
        "sky130-dsp-fft#6",
        "spec/decisions/0001-generator.md",
        "Status: Proposed\n\nThis record is **ratified on merge**.",
    ),
    (
        "sg13g2-noc#6",
        "spec/spec.md",
        "Ratified by the operator's approval of the PR that lands it.",
    ),
    (
        "sky130-droop-detector#8",
        "decisions/0003-threshold.md",
        "Merging this PR ratifies the threshold.",
    ),
    (
        "sky130-lvds-tx#9",
        "docs/decisions/0002-swing.md",
        "Ratified only on human merge approval.",
    ),
    (
        "sg13g2-dll#8",
        "spec/README.md",
        "Status: Ratified. Ratification happens on the operator's merge.",
    ),
    ("gf180-antifuse#61", "spec/antifuse.md", "ratified on\nmerge"),
    (
        "gf180-antifuse#92",
        "notes/adc-decision-record.md",
        "Merging this PR is the ratification.",
    ),
    ("gf180-sram#228", "spec/sram.md", "Ratified by the Operator."),
];

#[test]
fn every_2026_10_20_fixture_is_held() {
    for (pr, path, body) in FIXTURES {
        let v = scan(&[file(path, "added", Some(body))], &Config::default());
        assert_eq!(v, Verdict::Hold(vec![path.to_string()]), "{pr}");
    }
}

#[test]
fn a_pr_that_only_cites_a_spec_is_not_held() {
    // Touches no spec/decision file at all.
    let rtl = file("rtl/fft.v", "modified", Some("// see spec/spec.md: ratified on merge"));
    assert!(!held(&[rtl]));
    // Touches a spec file that carries no ratification clause.
    let plain = file("spec/spec.md", "modified", Some("The FFT is 256 points. Status: Proposed"));
    assert_eq!(scan(&[plain], &Config::default()), Verdict::Clear);
}

#[test]
fn edge_cases_follow_the_head_not_the_diff() {
    let declares = "Ratified on merge.";
    // Deleted: nothing at the head to ratify.
    assert!(!held(&[file("spec/spec.md", "removed", None)]));
    // A PR that removes the clause: head lacks the phrase.
    assert!(!held(&[file(
        "spec/spec.md",
        "modified",
        Some("Ratification is pending.")
    )]));
    // Renamed out of spec/**: still matched by its previous name, read at the new path.
    let mut moved = file("docs/overview.md", "renamed", Some(declares));
    moved.previous_filename = Some("spec/spec.md".into());
    assert!(held(&[moved]));
    // Renamed in from elsewhere: matched by the new name.
    assert!(held(&[file("spec/spec.md", "renamed", Some(declares))]));
    // Fail closed: a candidate that cannot be read is never Clear.
    assert_eq!(
        scan(&[file("spec/spec.md", "modified", None)], &Config::default()),
        Verdict::Unreadable(vec!["spec/spec.md".into()])
    );
    // A fenced example still holds: a false hold costs one comment.
    assert!(held(&[file(
        "spec/spec.md",
        "modified",
        Some("```\nratified on merge\n```")
    )]));
}

#[test]
fn a_bare_status_line_needs_a_ratification_clause() {
    let cfg = Config::default();
    assert!(!declares_ratification("Status: Proposed\n\nBody.", &cfg));
    assert!(!declares_ratification("Status: Ratified", &cfg));
    assert!(declares_ratification(
        "Status: Proposed\nThe operator ratifies this by approving the PR.",
        &cfg
    ));
}

#[test]
fn config_extends_paths_and_phrases_but_cannot_remove_built_ins() {
    let cfg = Config {
        paths: vec!["chips/*/design.md".into()],
        phrases: vec![r"sign-off\s+is\s+merge".into()],
    };
    // An explicit path alone does not hold: the phrase test still applies.
    let on_path = file("chips/fft/design.md", "added", Some("An ordinary design."));
    assert_eq!(scan(&[on_path], &cfg), Verdict::Clear);
    let declaring = file("chips/fft/design.md", "added", Some("Sign-off is merge."));
    assert!(matches!(scan(&[declaring], &cfg), Verdict::Hold(_)));
    // Built-ins stay, whatever the config says (additive only).
    assert!(declares_ratification("ratified on merge", &cfg));
    // An invalid extra regex degrades to a literal, never to a panic or a pass.
    let bad = Config {
        paths: vec![],
        phrases: vec!["(unclosed".into()],
    };
    assert!(declares_ratification("see (unclosed here", &bad));
    assert!(declares_ratification("ratified on merge", &bad));
    // From the effective config document.
    let doc = serde_json::json!({"champion": {"ratificationPaths": ["a/*.md"], "ratificationPhrases": ["x"]}});
    assert_eq!(
        Config::from_effective(&doc),
        Config {
            paths: vec!["a/*.md".into()],
            phrases: vec!["x".into()]
        }
    );
}

fn hold_note(head: &str) -> Comment {
    Comment::new(
        "fleet[bot]",
        &format!("{HOLD_MARKER}\n<!-- champion:hold-state head={head} -->\nmerging ratifies; the operator approves"),
    )
}

const RELEASE: &str = "clear the hold \u{2014} I read the spec and approve it";

#[test]
fn a_trusted_persons_comment_releases_the_hold() {
    let c = [hold_note(HEAD), Comment::new("rjwalters", RELEASE)];
    assert!(release_due(&c));
    assert_eq!(decide(&Verdict::Hold(vec!["spec/a.md".into()]), &c, HEAD), Action::Release);
    // Leading courtesy and mention are tolerated.
    assert!(is_release_instruction("@champion please clear the hold \u{2014} ok"));
}

#[test]
fn a_bot_or_champion_comment_does_not_release_it() {
    let verdict = Verdict::Hold(vec!["spec/a.md".into()]);
    for who in [
        Comment::new("fleet[bot]", RELEASE),
        Comment::new("Loom-Bot[BOT]", RELEASE),
    ] {
        let c = [hold_note(HEAD), who];
        assert!(!release_due(&c));
        assert_eq!(decide(&verdict, &c, HEAD), Action::Stands);
    }
    // Champion's own markers are never a person's release, whoever posts them.
    let own = Comment::new("rjwalters", &format!("<!-- champion:note -->\n{RELEASE}"));
    assert!(!release_due(&[hold_note(HEAD), own]));
    // A release that predates the hold does not carry over.
    assert!(!release_due(&[Comment::new("rjwalters", RELEASE), hold_note(HEAD)]));
}

#[test]
fn only_an_explicit_hold_clear_releases() {
    for no in [
        "do not clear the hold yet",
        "merge anyway",
        "ok to merge",
        "can we clear the hold?",
        "looks good to me",
        "please do not clear the hold",
    ] {
        assert!(!is_release_instruction(no), "{no}");
    }
    for yes in [
        "clear the hold \u{2014} approved",
        "Clear the hold.",
        "lift this hold: fine",
        "override the hold",
    ] {
        assert!(is_release_instruction(yes), "{yes}");
    }
}

#[test]
fn a_hand_removed_label_is_not_a_release() {
    // `decide` takes no label input: with the notice standing and no person's
    // comment, whatever the label state, the answer is "stands" (the tick
    // re-asserts `loom:blocked`).
    let verdict = Verdict::Hold(vec!["spec/a.md".into()]);
    assert_eq!(decide(&verdict, &[hold_note(HEAD)], HEAD), Action::Stands);
}

#[test]
fn the_state_machine_walks_hold_release_rearm_clear() {
    let verdict = Verdict::Hold(vec!["spec/a.md".into()]);
    // Fresh.
    assert_eq!(decide(&verdict, &[], HEAD), Action::Hold);
    // A quote of the marker is prose, not state.
    let quote = Comment::new("x", &format!("see {HOLD_MARKER}"));
    assert_eq!(decide(&verdict, &[quote], HEAD), Action::Hold);
    // Released at this head: nothing more, and the merge may proceed.
    let released = Comment::new(
        "fleet[bot]",
        &format!("{RELEASED_MARKER}\n<!-- champion:hold-state head={HEAD} -->"),
    );
    let c = [
        hold_note(HEAD),
        Comment::new("rjwalters", RELEASE),
        released.clone(),
    ];
    assert_eq!(
        episode(&c),
        Episode::Released {
            head: Some(HEAD.into())
        }
    );
    assert_eq!(decide(&verdict, &c, HEAD), Action::None);
    // A push after the release that still declares it re-arms.
    assert_eq!(decide(&verdict, &c, "def5678"), Action::Hold);
    // The head stopped declaring it: close the episode, once.
    assert_eq!(decide(&Verdict::Clear, &[hold_note(HEAD)], HEAD), Action::Clear);
    let cleared = Comment::new("fleet[bot]", CLEARED_MARKER);
    assert_eq!(decide(&Verdict::Clear, &[hold_note(HEAD), cleared], HEAD), Action::None);
    assert_eq!(decide(&Verdict::Clear, &[], HEAD), Action::None);
}

#[test]
fn an_unreadable_candidate_never_merges_and_never_drops_a_hold() {
    let unreadable = Verdict::Unreadable(vec!["spec/a.md".into()]);
    assert_eq!(decide(&unreadable, &[], HEAD), Action::Defer);
    assert_eq!(decide(&unreadable, &[hold_note(HEAD)], HEAD), Action::Stands);
}
