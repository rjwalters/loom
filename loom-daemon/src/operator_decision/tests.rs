use super::cli::{apply, exit, validate_cmd, ApplyRequest, Forge, IssueState, Target};
use super::render::{
    compose_body, extract_block, parse_block, render_section, ORIGINAL_REPORT_HEADING, SECTION_END,
    SECTION_START,
};
use super::validate::{parse, validate, Reason};
use super::{Decision, DecisionOption, DECISION_LABEL, MALFORMED_LABEL};
use serde_json::json;

fn opt(id: &str, why: &str) -> DecisionOption {
    DecisionOption {
        id: id.into(),
        label: format!("Option {id}"),
        why: Some(why.into()),
    }
}

fn decision(n: usize) -> Decision {
    let ids = ["a", "b", "c", "d", "e"];
    Decision {
        question: "Which storage backend?".into(),
        context: "The cache outgrew a flat file.".into(),
        options: ids[..n]
            .iter()
            .map(|id| opt(id, &format!("why {id}")))
            .collect(),
        recommended: Some("a".into()),
        deadline: None,
        context_links: vec![],
    }
}

fn codes(d: &Decision) -> Vec<&'static str> {
    validate(d).iter().map(Reason::code).collect()
}

// ---- validate: valid inputs ------------------------------------------------

#[test]
fn valid_two_option_input() {
    assert!(validate(&decision(2)).is_empty());
}

#[test]
fn valid_four_option_input() {
    let mut d = decision(4);
    d.deadline = Some("2026-10-10".into());
    d.context_links = vec!["https://example.com/x".into()];
    assert!(validate(&d).is_empty());
}

#[test]
fn valid_json_parses_from_the_documented_shape() {
    let input = json!({
        "question": "Ship now or wait?", "context": "CI is green.",
        "options": [
            {"id": "a", "label": "Ship", "why": "wins a week"},
            {"id": "b", "label": "Wait", "why": "gives up the week; worst because nothing is gained"}
        ],
        "recommended": "a"
    })
    .to_string();
    assert!(validate(&parse(&input).unwrap()).is_empty());
}

// ---- validate: every reason code -------------------------------------------

#[test]
fn reason_no_question() {
    let mut d = decision(2);
    d.question = "   ".into();
    assert_eq!(codes(&d), vec!["no_question"]);
}

#[test]
fn reason_too_few_options() {
    assert_eq!(codes(&decision(1)), vec!["too_few_options"]);
    let mut d = decision(1);
    d.options.clear();
    d.recommended = None;
    assert_eq!(codes(&d), vec!["too_few_options", "recommended_missing"]);
}

#[test]
fn reason_too_many_options() {
    assert_eq!(codes(&decision(5)), vec!["too_many_options"]);
}

#[test]
fn reason_missing_why() {
    let mut d = decision(2);
    d.options[1].why = None;
    assert_eq!(validate(&d), vec![Reason::MissingWhy("b".into())]);
}

#[test]
fn reason_empty_why_counts_whitespace() {
    let mut d = decision(2);
    d.options[0].why = Some(" \n\t".into());
    assert_eq!(validate(&d), vec![Reason::EmptyWhy("a".into())]);
}

#[test]
fn reason_missing_why_from_json_without_the_key() {
    let input = r#"{"question":"q","options":[{"id":"a","label":"A","why":"w"},{"id":"b","label":"B"}],"recommended":"a"}"#;
    assert_eq!(codes(&parse(input).unwrap()), vec!["missing_why"]);
}

#[test]
fn reason_duplicate_id() {
    let mut d = decision(3);
    d.options[2].id = "a".into();
    assert_eq!(validate(&d), vec![Reason::DuplicateId("a".into())]);
}

#[test]
fn reason_recommended_missing() {
    let mut d = decision(2);
    d.recommended = None;
    assert_eq!(codes(&d), vec!["recommended_missing"]);
    d.recommended = Some("  ".into());
    assert_eq!(codes(&d), vec!["recommended_missing"]);
}

#[test]
fn reason_unknown_recommended() {
    let mut d = decision(2);
    d.recommended = Some("z".into());
    assert_eq!(validate(&d), vec![Reason::UnknownRecommended("z".into())]);
}

#[test]
fn reason_recommended_not_first() {
    let mut d = decision(3);
    d.recommended = Some("b".into());
    assert_eq!(
        validate(&d),
        vec![Reason::RecommendedNotFirst {
            recommended: "b".into(),
            first: "a".into()
        }]
    );
}

#[test]
fn reason_empty_id_and_label() {
    let mut d = decision(2);
    d.options[1].id = String::new();
    d.options[0].label = " ".into();
    assert_eq!(codes(&d), vec!["empty_label", "empty_id"]);
}

#[test]
fn reason_invalid_json() {
    assert_eq!(parse("not json").unwrap_err().code(), "invalid_json");
}

#[test]
fn every_failing_reason_is_reported_not_just_the_first() {
    let d = Decision {
        question: String::new(),
        options: vec![DecisionOption {
            id: "a".into(),
            label: "A".into(),
            why: None,
        }],
        recommended: Some("q".into()),
        ..Decision::default()
    };
    assert_eq!(
        codes(&d),
        vec![
            "no_question",
            "too_few_options",
            "missing_why",
            "unknown_recommended"
        ]
    );
}

#[test]
fn validate_cmd_exit_codes_and_named_reasons() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let good = serde_json::to_string(&decision(2)).unwrap();
    assert_eq!(validate_cmd(&good, &mut out, &mut err), exit::OK);
    assert!(String::from_utf8_lossy(&out).contains("VALID=true"));

    let mut bad = decision(2);
    bad.recommended = Some("b".into());
    bad.options[0].why = Some(String::new());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = validate_cmd(&serde_json::to_string(&bad).unwrap(), &mut out, &mut err);
    assert_eq!(code, exit::REFUSED);
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("REASON=empty_why"), "{err}");
    assert!(err.contains("REASON=recommended_not_first"), "{err}");
}

// ---- render -----------------------------------------------------------------

#[test]
fn render_round_trip() {
    for n in [2, 4] {
        let mut d = decision(n);
        d.deadline = Some("Friday".into());
        d.context_links = vec!["#123".into()];
        let section = render_section(&d);
        let back = parse_block(&section).expect("block parses");
        assert_eq!(back, d);
        assert_eq!(back.recommended.as_deref(), Some(back.options[0].id.as_str()));
        assert!(validate(&back).is_empty());
    }
}

#[test]
fn readable_list_is_ranked_and_marks_the_recommendation() {
    let s = render_section(&decision(3));
    let a = s.find("1. **Option a** (recommended): why a").unwrap();
    let b = s.find("2. **Option b**: why b").unwrap();
    let c = s.find("3. **Option c**: why c").unwrap();
    assert!(a < b && b < c, "{s}");
}

#[test]
fn first_fenced_block_is_the_decision_block() {
    let body = compose_body("Some prose.\n\n```sh\necho hi\n```\n", &decision(2));
    let first_fence = body.lines().find(|l| l.starts_with("```")).unwrap();
    assert_eq!(first_fence, "```decision");
    assert!(extract_block(&body).is_some());
}

#[test]
fn relabel_preserves_the_original_report() {
    let original = "The cache is slow.\n\nShould we switch?";
    let body = compose_body(original, &decision(2));
    assert!(body.starts_with(SECTION_START));
    let tail = &body[body.find(SECTION_END).unwrap()..];
    assert!(tail.contains(&format!("{ORIGINAL_REPORT_HEADING}\n\n{original}")));
}

#[test]
fn compose_is_idempotent_and_replaces_instead_of_stacking() {
    let original = "Prose report.";
    let once = compose_body(original, &decision(2));
    let twice = compose_body(&once, &decision(2));
    assert_eq!(once, twice);

    let changed = compose_body(&once, &decision(3));
    assert_eq!(changed.matches(SECTION_START).count(), 1);
    assert_eq!(changed.matches("```decision").count(), 1);
    assert_eq!(changed.matches(ORIGINAL_REPORT_HEADING).count(), 1);
    assert_eq!(parse_block(&changed).unwrap(), decision(3));
    assert!(changed.contains(original));
}

#[test]
fn compose_replaces_a_hand_written_decision_block() {
    let original = "Intro.\n\n```decision\n{\"question\": \"old\"}\n```\n\nMore prose.";
    let body = compose_body(original, &decision(2));
    assert_eq!(body.matches("```decision").count(), 1);
    assert_eq!(parse_block(&body).unwrap(), decision(2));
    assert!(body.contains("Intro.") && body.contains("More prose."));
}

#[test]
fn compose_on_empty_body_is_just_the_section() {
    let body = compose_body("", &decision(2));
    assert_eq!(body, render_section(&decision(2)));
    assert!(!body.contains(ORIGINAL_REPORT_HEADING));
}

// ---- apply against a recording fake forge -----------------------------------

#[derive(Default)]
struct FakeForge {
    state: IssueState,
    calls: Vec<String>,
    fail_body: bool,
}

impl FakeForge {
    fn mutations(&self) -> Vec<&String> {
        self.calls
            .iter()
            .filter(|c| !c.starts_with("view"))
            .collect()
    }
}

impl Forge for FakeForge {
    fn view(&mut self, issue: u64) -> Result<IssueState, String> {
        self.calls.push(format!("view {issue}"));
        Ok(self.state.clone())
    }
    fn set_body(&mut self, issue: u64, body: &str) -> Result<(), String> {
        self.calls.push(format!("set_body {issue}"));
        if self.fail_body {
            return Err("boom".into());
        }
        self.state.body = body.to_string();
        Ok(())
    }
    fn add_labels(&mut self, issue: u64, labels: &[String]) -> Result<(), String> {
        self.calls
            .push(format!("add_labels {issue} {}", labels.join(",")));
        self.state.labels.extend(labels.iter().cloned());
        Ok(())
    }
    fn remove_label(&mut self, issue: u64, label: &str) -> Result<(), String> {
        self.calls.push(format!("remove_label {issue} {label}"));
        self.state.labels.retain(|l| l != label);
        Ok(())
    }
    fn create(
        &mut self,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<String, (i32, String)> {
        self.calls
            .push(format!("create {title} {}", labels.join(",")));
        self.state.body = body.to_string();
        Ok("https://github.com/o/r/issues/99".into())
    }
}

fn req(target: Target) -> ApplyRequest {
    ApplyRequest {
        target,
        also_labels: vec![],
        remove_labels: vec![],
        dry_run: false,
    }
}

fn run(forge: &mut FakeForge, d: &Decision, r: &ApplyRequest) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = apply(forge, &serde_json::to_string(d).unwrap(), r, &mut out, &mut err);
    (code, String::from_utf8_lossy(&out).into(), String::from_utf8_lossy(&err).into())
}

#[test]
fn apply_refusal_makes_no_forge_call_at_all() {
    let mut f = FakeForge::default();
    let (code, _, err) = run(&mut f, &decision(1), &req(Target::Existing(7)));
    assert_eq!(code, exit::REFUSED);
    assert!(err.contains("REASON=too_few_options"));
    assert!(f.calls.is_empty(), "{:?}", f.calls);
}

#[test]
fn apply_dry_run_issues_no_mutation() {
    let mut f = FakeForge {
        state: IssueState {
            body: "prose".into(),
            labels: vec!["loom:building".into()],
        },
        ..FakeForge::default()
    };
    let mut r = req(Target::Existing(7));
    r.dry_run = true;
    r.also_labels = vec!["loom:operator-only".into()];
    r.remove_labels = vec!["loom:building".into()];
    let (code, out, _) = run(&mut f, &decision(2), &r);
    assert_eq!(code, exit::OK);
    assert!(f.mutations().is_empty(), "{:?}", f.calls);
    assert!(out.contains("DRY_RUN=true"));
    assert!(out.contains("LABELS_ADD=loom:operator-decision,loom:operator-only"));
    assert!(out.contains("LABELS_REMOVE=loom:building"));
    assert!(out.contains("```decision"));

    let mut f = FakeForge::default();
    let mut r = req(Target::New { title: "t".into() });
    r.dry_run = true;
    let (code, _, _) = run(&mut f, &decision(2), &r);
    assert_eq!(code, exit::OK);
    assert!(f.calls.is_empty(), "{:?}", f.calls);
}

#[test]
fn apply_relabel_writes_body_before_labels() {
    let mut f = FakeForge {
        state: IssueState {
            body: "Original prose.".into(),
            labels: vec!["loom:building".into(), MALFORMED_LABEL.into()],
        },
        ..FakeForge::default()
    };
    let mut r = req(Target::Existing(7));
    r.also_labels = vec!["loom:operator-only".into()];
    r.remove_labels = vec!["loom:building".into(), "loom:not-present".into()];
    let (code, _, err) = run(&mut f, &decision(2), &r);
    assert_eq!(code, exit::OK, "{err}");
    assert_eq!(
        f.calls,
        vec![
            "view 7",
            "set_body 7",
            "add_labels 7 loom:operator-decision,loom:operator-only",
            "remove_label 7 loom:building",
            "remove_label 7 loom:decision-malformed",
        ]
    );
    assert!(f.state.body.contains("Original prose."));
    assert!(f.state.labels.contains(&DECISION_LABEL.to_string()));
    assert!(!f.state.labels.contains(&MALFORMED_LABEL.to_string()));
}

#[test]
fn apply_body_failure_applies_no_label() {
    let mut f = FakeForge {
        state: IssueState {
            body: "prose".into(),
            labels: vec![],
        },
        fail_body: true,
        ..FakeForge::default()
    };
    let (code, _, err) = run(&mut f, &decision(2), &req(Target::Existing(7)));
    assert_eq!(code, exit::FORGE);
    assert!(err.contains("no label applied"));
    assert_eq!(f.calls, vec!["view 7", "set_body 7"]);
}

#[test]
fn apply_reapply_is_idempotent() {
    let mut f = FakeForge {
        state: IssueState {
            body: "prose".into(),
            labels: vec![],
        },
        ..FakeForge::default()
    };
    let r = req(Target::Existing(7));
    assert_eq!(run(&mut f, &decision(2), &r).0, exit::OK);
    let body = f.state.body.clone();
    f.calls.clear();
    let (code, out, _) = run(&mut f, &decision(2), &r);
    assert_eq!(code, exit::OK);
    assert_eq!(f.state.body, body);
    // Same body, label already present: nothing left to mutate.
    assert_eq!(f.calls, vec!["view 7"]);
    assert!(out.contains("BODY_CHANGED=false"));
}

#[test]
fn apply_new_files_with_labels() {
    let mut f = FakeForge::default();
    let mut r = req(Target::New {
        title: "Pick a backend".into(),
    });
    r.also_labels = vec!["loom:operator-only".into(), DECISION_LABEL.into()];
    let (code, out, _) = run(&mut f, &decision(4), &r);
    assert_eq!(code, exit::OK);
    assert_eq!(f.calls, vec!["create Pick a backend loom:operator-decision,loom:operator-only"]);
    assert!(out.contains("CREATED=https://github.com/o/r/issues/99"));
    assert_eq!(parse_block(&f.state.body).unwrap(), decision(4));
}
