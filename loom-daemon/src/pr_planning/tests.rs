use super::*;
use crate::{
    forge_identity::{FleetLogins, Roster},
    provenance::marker::Marker,
};

fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&Roster::default()), None, vec!["session-app[bot]".into()])
}
fn body(origin: &str) -> String {
    format!("<!-- loom:provenance v1 build=unknown unknown unknown prompts=unknown unknown sweep=none story=none trace=unknown host=unknown base=unknown run=none origin={origin} -->")
}
fn pr(n: u64, origin: &str, labels: &[&str]) -> Value {
    json!({"number":n,"state":"open","draft":false,"title":"test", "created_at":format!("2026-09-{:02}", n),
        "body":body(origin),"user":{"login":"session-app[bot]","type":"Bot"},"author_association":"NONE",
        "labels":labels.iter().map(|l| json!({"name":l})).collect::<Vec<_>>()})
}
fn ids(rows: &[Value]) -> Vec<u64> {
    rows.iter().map(|r| r["number"].as_u64().unwrap()).collect()
}

#[test]
fn every_role_prefers_priority_then_interactive_and_disabled_retains_priority() {
    for (role, label) in [
        (PrRole::Judge, "loom:review-requested"),
        (PrRole::Doctor, "loom:changes-requested"),
        (PrRole::Champion, "loom:pr"),
    ] {
        let rows = vec![
            pr(1, "autonomous", &[label]),
            pr(2, "interactive", &[label]),
            pr(3, "unknown", &[label]),
            pr(4, "autonomous", &[label, "loom:important"]),
            pr(5, "interactive", &[label]),
        ];
        assert_eq!(ids(&ordered_queue(rows.clone(), role, true, &policy())), vec![4, 2, 5, 1, 3]);
        assert_eq!(ids(&ordered_queue(rows, role, false, &policy())), vec![4, 1, 2, 3, 5]);
    }
}

#[test]
fn malformed_conflicting_legacy_or_untrusted_origin_never_prioritizes() {
    let trusted = pr(1, "interactive", &[]);
    assert_eq!(WorkOrigin::trusted_pr(&trusted, &policy()), WorkOrigin::Interactive);
    let valid = body("interactive");
    for bad in [
        String::new(),
        valid.replace(" origin=interactive", ""),
        valid.replace("interactive", "bogus"),
        format!("{valid}\n{valid}"),
        valid.replace("origin=interactive", "origin=interactive origin=autonomous"),
        format!("```\n{valid}\n```"),
        valid.replace("v1", "v2"),
    ] {
        let mut row = trusted.clone();
        row["body"] = json!(bad);
        assert_eq!(WorkOrigin::trusted_pr(&row, &policy()), WorkOrigin::Unknown, "{row}");
    }
    for author in [
        json!({"login":"outsider","type":"User"}),
        json!({"login":"foreign[bot]","type":"Bot"}),
        Value::Null,
    ] {
        let mut row = trusted.clone();
        row["user"] = author;
        assert_eq!(WorkOrigin::trusted_pr(&row, &policy()), WorkOrigin::Unknown);
    }
    let mut human = trusted;
    human["user"] = json!({"login":"owner"});
    human["author_association"] = json!("OWNER");
    human["body"] = json!("");
    assert_eq!(WorkOrigin::trusted_pr(&human, &policy()), WorkOrigin::Unknown);
    let parsed = Marker::parse(&valid).unwrap();
    assert_eq!(Marker::parse(&parsed.render()), Some(parsed));
}

#[test]
fn human_fallback_joins_a_busy_queue_only_when_enabled_without_enrollment() {
    let rows = vec![
        pr(1, "autonomous", &["loom:review-requested"]),
        pr(2, "interactive", &[]),
        pr(3, "unknown", &[]),
    ];
    let enabled = ordered_queue(rows.clone(), PrRole::Judge, true, &policy());
    assert_eq!(ids(&enabled), vec![2, 1]);
    assert_eq!(enabled[0]["mode"], "fallback");
    assert_eq!(enabled[0]["labels"], json!([]));
    assert_eq!(ids(&ordered_queue(rows, PrRole::Judge, false, &policy())), vec![1]);
}

#[test]
fn holds_and_drafts_do_not_block_other_work() {
    for (role, label) in [
        (PrRole::Judge, "loom:review-requested"),
        (PrRole::Doctor, "loom:changes-requested"),
        (PrRole::Champion, "loom:pr"),
    ] {
        let mut draft = pr(1, "interactive", &[label]);
        draft["draft"] = json!(true);
        let rows = vec![
            draft,
            pr(3, "interactive", &[label, "loom:operator-only"]),
            pr(4, "autonomous", &[label]),
            pr(5, "interactive", &[label, "loom:operator"]),
            pr(6, "interactive", &[label, "loom:frozen"]),
        ];
        assert_eq!(ids(&ordered_queue(rows, role, true, &policy())), vec![4], "{role:?}");
    }
    let mut conflict = pr(1, "autonomous", &["loom:pr"]);
    conflict["mergeable"] = json!(false);
    let mut held = conflict.clone();
    held["number"] = json!(2);
    held["labels"] = json!([{"name":"loom:pr"},{"name":"loom:blocked"}]);
    let feedback = pr(3, "interactive", &["loom:changes-requested"]);
    let claimed = pr(4, "interactive", &["loom:changes-requested", "loom:treating"]);
    let operator = pr(5, "interactive", &["loom:changes-requested", "loom:operator"]);
    let rows = vec![feedback, held, conflict, claimed, operator];
    assert_eq!(ids(&ordered_queue(rows.clone(), PrRole::Doctor, true, &policy())), vec![3, 1]);
    assert_eq!(ids(&ordered_queue(rows, PrRole::Doctor, false, &policy())), vec![1, 3]);
}

/// #11861: no role's queue serves a `loom:operator` PR, and only Champion's
/// serves a `loom:pr` + `loom:blocked` PR (its own merge-risk or critical-file
/// hold, which it re-evaluates and releases).
#[test]
fn no_role_serves_an_operator_pr_and_only_champion_serves_its_hold() {
    for (role, label) in [
        (PrRole::Judge, "loom:review-requested"),
        (PrRole::Doctor, "loom:changes-requested"),
        (PrRole::Champion, "loom:pr"),
    ] {
        for hold in [
            "loom:operator",
            "loom:operator-only",
            "loom:operator-decision",
        ] {
            let rows = vec![pr(1, "interactive", &[label, hold])];
            assert!(ordered_queue(rows, role, true, &policy()).is_empty(), "{role:?} {hold}");
        }
    }
    let held = || pr(7, "autonomous", &["loom:pr", "loom:blocked"]);
    assert_eq!(ids(&ordered_queue(vec![held()], PrRole::Champion, true, &policy())), vec![7]);
    let mut conflicted = held();
    conflicted["mergeable"] = json!("CONFLICTING");
    assert!(ordered_queue(vec![conflicted], PrRole::Doctor, true, &policy()).is_empty());
    // A Champion hold a human also parked for a decision is not Champion's.
    let both = pr(8, "autonomous", &["loom:pr", "loom:blocked", "loom:operator"]);
    assert!(ordered_queue(vec![both], PrRole::Champion, true, &policy()).is_empty());
    // Champion's exception needs `loom:pr`: a blocked PR without it is not served.
    let unapproved = pr(9, "autonomous", &["loom:review-requested", "loom:blocked"]);
    assert!(ordered_queue(vec![unapproved], PrRole::Champion, true, &policy()).is_empty());
}

#[test]
fn provenance_survives_doctor_and_re_review() {
    let mut row = pr(7, "interactive", &["loom:review-requested"]);
    let original = row["body"].clone();
    for (role, label) in [
        (PrRole::Judge, "loom:review-requested"),
        (PrRole::Doctor, "loom:changes-requested"),
        (PrRole::Judge, "loom:review-requested"),
        (PrRole::Champion, "loom:pr"),
    ] {
        row["labels"] = json!([{"name":label}]);
        let plan = ordered_queue(vec![row.clone()], role, true, &policy());
        assert_eq!(plan[0]["origin"], "interactive");
        assert_eq!(plan[0]["body"], original);
    }
}

#[test]
fn config_defaults_true_and_false_is_not_swallowed() {
    assert!(preference(&json!({})));
    assert!(!preference(&json!({"planning":{"preferHumanPrs":false}})));
    assert!(preference(&json!({"planning":{"preferHumanPrs":true}})));
}

#[test]
fn disabled_fallback_admission_waits_even_for_held_or_draft_labeled_queue() {
    for label in ["loom:blocked", "loom:operator-only", "draft"] {
        let mut held = pr(1, "interactive", &["loom:review-requested", label]);
        if label == "draft" {
            held["draft"] = json!(true);
        }
        assert!(
            ordered_queue(vec![held, pr(2, "unknown", &[])], PrRole::Judge, false, &policy())
                .is_empty()
        );
    }
}

#[cfg(unix)]
#[test]
#[serial_test::serial(loom_config_env)]
fn real_role_runner_gate_admits_only_guard_eligible_interactive_fallback() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".loom/scripts")).unwrap();
    let script = |name: &str, body: &str| {
        let path = root.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let gh = script(
        "gh",
        r#"printf 'HTTP/2 200 OK\r\n\r\n'
case "$*" in *issues*) echo '[]';; *) cat pulls.json;; esac"#,
    );
    script(".loom/scripts/judge-fallback-guard.sh", "test ! -f skip || exit 12; exit 0");
    std::fs::write(
        root.join("pulls.json"),
        serde_json::to_vec(&vec![{
            let mut r = pr(1, "interactive", &[]);
            r["author_association"] = json!("OWNER");
            r
        }])
        .unwrap(),
    )
    .unwrap();
    let old = std::env::var_os("LOOM_GH_BIN");
    std::env::set_var("LOOM_GH_BIN", &gh);
    let probe = crate::role_runner::concurrent_dispatch::forge_queue_probe();
    let enabled = probe(root, &["loom:review-requested"]);
    std::fs::write(root.join("skip"), "").unwrap();
    let skipped = probe(root, &["loom:review-requested"]);
    std::fs::write(root.join(".loom/config.json"), r#"{"planning":{"preferHumanPrs":false}}"#)
        .unwrap();
    std::fs::remove_file(root.join("skip")).unwrap();
    let disabled = probe(root, &["loom:review-requested"]);
    if let Some(old) = old {
        std::env::set_var("LOOM_GH_BIN", old);
    } else {
        std::env::remove_var("LOOM_GH_BIN");
    }
    assert_eq!(enabled, Ok(true));
    assert_eq!(skipped, Ok(false));
    assert_eq!(disabled, Ok(false));
}

/// #11510: every role drains by priority level, highest first, with the
/// reason `priority` and the level beside it.
#[test]
fn every_role_drains_very_important_before_important_before_ordinary_work() {
    for (role, label) in [
        (PrRole::Judge, "loom:review-requested"),
        (PrRole::Doctor, "loom:changes-requested"),
        (PrRole::Champion, "loom:pr"),
    ] {
        let rows = vec![
            pr(1, "interactive", &[label]),
            pr(2, "autonomous", &[label, "loom:important"]),
            pr(3, "autonomous", &[label, "loom:very-important"]),
            pr(4, "autonomous", &[label, "loom:important", "loom:very-important"]),
        ];
        for prefer in [true, false] {
            let out = ordered_queue(rows.clone(), role, prefer, &policy());
            assert_eq!(ids(&out), vec![3, 4, 2, 1], "{role:?} prefer={prefer}");
            assert_eq!(out[0]["priorityLevel"], 2);
            assert_eq!(out[0]["priorityReason"], "priority");
            assert_eq!(out[2]["priorityLevel"], 1);
            assert_eq!(out[3]["priorityLevel"], 0);
            assert!(out[0].get("operatorPriorityLevel").is_none());
        }
    }
}

/// #11743: a frozen PR is in no role's queue, whatever else it carries.
#[test]
fn a_frozen_pr_is_in_no_role_queue() {
    for (role, label) in [
        (PrRole::Judge, "loom:review-requested"),
        (PrRole::Doctor, "loom:changes-requested"),
        (PrRole::Champion, "loom:pr"),
    ] {
        let rows = vec![
            pr(1, "interactive", &[label, "loom:frozen"]),
            pr(2, "interactive", &[label]),
        ];
        assert_eq!(ids(&ordered_queue(rows, role, false, &policy())), vec![2], "{label}");
    }
}

/// #11861: a Champion hold sent to Doctor keeps `loom:blocked`; Doctor's queue
/// serves it only when `fetch_queue` verified a standing trusted hold marker
/// (`championHold`). The same labels without it (a Doctor-cycle-cap park) stay
/// out, and no other role's queue serves it.
#[test]
fn doctor_serves_a_verified_champion_hold_and_nothing_else_parked() {
    let mut verified = pr(1, "autonomous", &["loom:changes-requested", "loom:blocked"]);
    verified[CHAMPION_HOLD_FIELD] = json!(true);
    let capped = pr(2, "autonomous", &["loom:changes-requested", "loom:blocked"]);
    let mut operator =
        pr(3, "autonomous", &["loom:changes-requested", "loom:blocked", "loom:operator"]);
    operator[CHAMPION_HOLD_FIELD] = json!(true);
    let rows = vec![verified.clone(), capped, operator];
    assert_eq!(ids(&ordered_queue(rows, PrRole::Doctor, true, &policy())), vec![1]);
    for role in [PrRole::Judge, PrRole::Champion] {
        assert!(
            ordered_queue(vec![verified.clone()], role, true, &policy()).is_empty(),
            "{role:?}"
        );
    }
}
