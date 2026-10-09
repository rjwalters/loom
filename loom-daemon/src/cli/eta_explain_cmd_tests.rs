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

/// An export of two emitted land estimates (12:00 and 14:00) of one issue.
fn export_file(dir: &std::path::Path) -> std::path::PathBuf {
    let mut early = golden();
    early.estimate_id = "early".into();
    let mut late = golden();
    late.estimate_id = "late".into();
    late.as_of += chrono::Duration::hours(2);
    let line = |e: &Explanation, id: &str| {
        let ns = e.as_of.timestamp_nanos_opt().unwrap();
        serde_json::json!({
            "record_id": id,
            "repo": e.subject.repo,
            "estimate_id": e.estimate_id,
            "body": serde_json::to_string(e).unwrap(),
            "event_time_ns": ns.to_string(),
            "knowable_time_ns": ns.to_string(),
        })
        .to_string()
    };
    let path = dir.join("estimates.jsonl");
    std::fs::write(&path, format!("{}\n{}\n", line(&early, "r1"), line(&late, "r2"))).unwrap();
    path
}

fn args(target: &str, from_file: &std::path::Path) -> super::EtaExplainArgs {
    super::EtaExplainArgs {
        file: None,
        target: Some(target.to_string()),
        signoz: false,
        from_file: Some(from_file.to_path_buf()),
        endpoint: None,
        user: None,
        credential_file: None,
        at: None,
        kind: None,
        heuristic: None,
        lookback_days: 36500,
        repo_root: None,
        scope: None,
        diff: None,
        id: None,
        diff_id: None,
        json: true,
    }
}

#[test]
fn an_estimate_id_is_read_back_from_an_export() {
    let dir = tempfile::tempdir().unwrap();
    let path = export_file(dir.path());
    let got = args("late", &path).source().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].estimate_id, "late");
    assert!(args("missing", &path).source().is_err());
}

#[test]
fn at_picks_the_newest_emitted_estimate_not_after_t() {
    let dir = tempfile::tempdir().unwrap();
    let path = export_file(dir.path());
    let story = "rjwalters/loom#9289";
    let mut a = args(story, &path);
    a.at = Some("2026-09-20T13:00:00Z".into());
    assert_eq!(a.source().unwrap()[0].estimate_id, "early");
    let mut a = args(story, &path);
    a.at = Some("2026-09-20T14:00:00Z".into());
    assert_eq!(a.source().unwrap()[0].estimate_id, "late");
    let mut a = args(story, &path);
    a.at = Some("2026-09-20T11:00:00Z".into());
    let err = a.source().unwrap_err().to_string();
    assert!(err.contains("no estimate"), "{err}");
}

#[test]
fn at_without_an_emitted_source_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = export_file(dir.path());
    let mut a = args("rjwalters/loom#9289", &path);
    a.from_file = None;
    a.at = Some("2026-09-20T13:00:00Z".into());
    let err = a.source().unwrap_err().to_string();
    assert!(err.contains("--signoz"), "{err}");
    let mut a = args("late", &path);
    a.from_file = None;
    assert!(a.source().unwrap_err().to_string().contains("--signoz"));
    let mut a = args("late", &path);
    a.at = Some("2026-09-20T13:00:00Z".into());
    assert!(a.source().unwrap_err().to_string().contains("owner/repo#N"));
}
