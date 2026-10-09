//! `eta explain` rendering against the golden `land-v1` explanation.

use super::{parse, pick, render_diff, render_report};
use loom_daemon::eta::explain::{self, Parity};
use loom_daemon::eta::explanation::Explanation;

/// The golden `land-v1` explanation the daemon's own tests pin.
const EXPLANATION: &str = include_str!("../eta/fixtures/explanation-golden.json");

const GOLDEN_TEXT: &str = include_str!("eta_explain_fixtures/report.txt");
const GOLDEN_JSON: &str = include_str!("eta_explain_fixtures/report.json");

fn golden() -> Explanation {
    serde_json::from_str(EXPLANATION).expect("the golden explanation parses")
}

/// Rewrite a golden file when `LOOM_ETA_BLESS=1`.
fn bless(name: &str, text: &str) {
    if std::env::var("LOOM_ETA_BLESS").is_ok_and(|v| v == "1") {
        let path = format!("{}/src/cli/eta_explain_fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::write(path, text).unwrap();
    }
}

#[test]
fn the_text_report_matches_its_golden() {
    let text = render_report(&explain::report(&golden()));
    bless("report.txt", &text);
    assert_eq!(text, GOLDEN_TEXT, "re-bless with LOOM_ETA_BLESS=1 only for a deliberate change");
    assert!(text.contains("parity: exact"), "{text}");
    assert!(text.contains("stages (stage_predictions):"), "{text}");
}

#[test]
fn the_json_report_matches_its_golden() {
    let report = explain::report(&golden());
    let mut text = serde_json::to_string_pretty(&report).unwrap();
    text.push('\n');
    bless("report.json", &text);
    assert_eq!(text, GOLDEN_JSON, "re-bless with LOOM_ETA_BLESS=1 only for a deliberate change");
    assert_eq!(report.parity, Parity::Exact);
    assert_eq!(report.recorded, report.replayed);
    assert!(!report.inputs.is_empty());
}

#[test]
fn a_single_object_and_jsonl_both_parse() {
    let one = golden();
    let (pretty, skipped) = parse(EXPLANATION);
    assert_eq!((pretty.len(), skipped), (1, 0));
    let line = serde_json::to_string(&one).unwrap();
    let (lines, skipped) = parse(&format!("{line}\nnot json\n\n{line}\n"));
    assert_eq!((lines.len(), skipped), (2, 1));
}

#[test]
fn pick_needs_an_id_when_the_file_holds_more_than_one() {
    let one = golden();
    assert!(pick(vec![one.clone()], None, "--file").is_ok());
    let err = pick(vec![one.clone(), one.clone()], None, "--file").unwrap_err();
    assert!(err.to_string().contains("holds 2 estimates"), "{err}");
    let id = one.estimate_id.clone();
    assert_eq!(
        pick(vec![one.clone(), one], Some(&id), "--file")
            .unwrap()
            .estimate_id,
        id
    );
}

#[test]
fn the_diff_names_the_changed_input_and_the_residual() {
    let a = golden();
    let b = explain::with_input(&a, "current_stage.rework_rounds", 1.0).unwrap();
    let d = explain::diff(&a, &b).expect("both replay");
    let text = render_diff(&d);
    assert!(text.contains("current_stage.rework_rounds"), "{text}");
    assert!(text.contains("residual"), "{text}");
    assert_eq!(d.residual_p50_sec, 0, "a single swap explains it all");
}
