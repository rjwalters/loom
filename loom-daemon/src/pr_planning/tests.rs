use super::*;
use crate::forge_identity::{FleetLogins, Roster};

fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&Roster::default()), None, vec!["session-app[bot]".into()])
}
/// A PR row: `interactive` is a human (trusted, non-App) author; anything else
/// is opened by a trusted App login, which is never a human.
fn pr(n: u64, origin: &str, labels: &[&str]) -> Value {
    let (user, association) = if origin == "interactive" {
        (json!({"login":"operator","type":"User"}), "OWNER")
    } else {
        (json!({"login":"session-app[bot]","type":"Bot"}), "NONE")
    };
    json!({"number":n,"state":"open","draft":false,"title":"test", "created_at":format!("2026-09-{:02}", n),
        "body":"","user":user,"author_association":association,
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
fn only_a_trusted_non_app_author_is_interactive() {
    let human = pr(1, "interactive", &[]);
    assert_eq!(WorkOrigin::trusted_pr(&human, &policy()), WorkOrigin::Interactive);
    // A body marker neither grants nor denies it: the author decides.
    let mut marked = pr(2, "autonomous", &[]);
    marked["body"] = json!("<!-- loom:provenance v1 origin=interactive -->");
    assert_eq!(WorkOrigin::trusted_pr(&marked, &policy()), WorkOrigin::Unknown);
    for author in [
        json!({"login":"outsider","type":"User"}),
        json!({"login":"foreign[bot]","type":"Bot"}),
        json!({"login":"session-app[bot]","type":"Bot"}),
        json!({"login":"operator","type":"Bot"}),
        Value::Null,
    ] {
        let mut row = human.clone();
        row["user"] = author;
        if row["user"]
            .as_object()
            .is_some_and(|u| u["login"] == "outsider")
        {
            row["author_association"] = json!("NONE");
        }
        assert_eq!(WorkOrigin::trusted_pr(&row, &policy()), WorkOrigin::Unknown, "{row}");
    }
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

/// #11861 review: the whole round trip of a Champion hold, end to end, through
/// the real `fetch_queue` (every role), the real role-runner spawn gate
/// (`forge_queue_probe`, which calls `has_judge_work`) and the real merge guard,
/// over a fake `gh` serving one PR whose labels move stage by stage.
/// `loom:blocked` stays on throughout; each stage is served by exactly the
/// role that owns it; the gate opens for that role; the merge guard refuses
/// until a human removes `loom:blocked`, and then Champion merges.
#[cfg(unix)]
#[test]
#[serial_test::serial(loom_config_env)]
fn a_champion_hold_round_trip_never_strands_and_never_merges_while_held() {
    use crate::champion_hold::MERGE_RISK_HOLD_MARKER;
    use crate::merge_pr::labels::contradiction;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".loom/scripts")).unwrap();
    // `pulls?…` → pulls.json; `issues/N/comments` → comments.json; any other
    // `issues…` listing (the gate's) → issues.json.
    let gh = root.join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\nprintf 'HTTP/2 200 OK\\r\\n\\r\\n'\ncd \"$(dirname \"$0\")\"\ncase \"$*\" in\n  *comments*) cat comments.json;;\n  *issues*) cat issues.json;;\n  *) cat pulls.json;;\nesac\n",
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let marker = json!([{"body": format!("{MERGE_RISK_HOLD_MARKER}\n<!-- champion:hold-state head=abc -->\nheld"),
        "user": {"login": "maintainer", "type": "User"}, "author_association": "OWNER"}]);
    let record = crate::park_record::render_park(
        &[],
        Some(crate::champion_hold::HOLD_BY),
        Some("2026-10-14T00:00:00Z"),
        Some(crate::champion_hold::MERGE_RISK_HOLD_REASON),
    );
    let stage = |labels: &[&str], comments: &Value| {
        let mut row = pr(42, "autonomous", labels);
        row["body"] = json!(format!("A change.\n\n{record}\n"));
        let mut issue = row.clone();
        issue["pull_request"] = json!({"url": "x"});
        std::fs::write(root.join("pulls.json"), serde_json::to_vec(&vec![row]).unwrap()).unwrap();
        std::fs::write(root.join("issues.json"), serde_json::to_vec(&vec![issue]).unwrap())
            .unwrap();
        std::fs::write(root.join("comments.json"), serde_json::to_vec(comments).unwrap()).unwrap();
    };
    let old = std::env::var_os("LOOM_GH_BIN");
    std::env::set_var("LOOM_GH_BIN", &gh);
    let served_by = || -> Vec<PrRole> {
        [PrRole::Judge, PrRole::Doctor, PrRole::Champion]
            .into_iter()
            .filter(|r| ids(&fetch_queue(root, &gh, *r).unwrap()) == vec![42])
            .collect()
    };
    let probe = crate::role_runner::concurrent_dispatch::forge_queue_probe();
    let refused = |labels: &[&str]| contradiction(&labels.join("\n")).is_some();

    // 1. Held at approval: Champion's (the janitor exception); merge refused.
    let held = ["loom:pr", "loom:blocked"];
    stage(&held, &marker);
    let s1 = (served_by(), refused(&held));
    // 2. Champion's stale route: changes requested, the label stays; Doctor's.
    let at_doctor = ["loom:changes-requested", "loom:blocked"];
    stage(&at_doctor, &marker);
    let s2 = (served_by(), probe(root, &["loom:changes-requested"]), refused(&at_doctor));
    // 3. Doctor hands it back: Judge's, and the Judge spawn gate opens on it.
    let at_judge = ["loom:review-requested", "loom:blocked"];
    stage(&at_judge, &marker);
    let s3 = (served_by(), probe(root, &["loom:review-requested"]), refused(&at_judge));
    // 4. Judge approves: loom:pr + loom:blocked, Champion's hold again.
    stage(&held, &marker);
    let s4 = (served_by(), refused(&held));
    // 5. A human removes loom:blocked: plain approval, Champion merges.
    stage(&["loom:pr"], &marker);
    let s5 = (served_by(), refused(&["loom:pr"]));
    // Without a trusted marker, neither round-trip stage is served or gated.
    let outsider = json!([{"body": format!("{MERGE_RISK_HOLD_MARKER}\nforged"),
        "user": {"login": "drive-by", "type": "User"}, "author_association": "NONE"}]);
    stage(&at_judge, &outsider);
    let s6 = (served_by(), probe(root, &["loom:review-requested"]));
    stage(&at_doctor, &outsider);
    let s7 = (served_by(), probe(root, &["loom:changes-requested"]));
    if let Some(old) = old {
        std::env::set_var("LOOM_GH_BIN", old);
    } else {
        std::env::remove_var("LOOM_GH_BIN");
    }

    assert_eq!(s1, (vec![PrRole::Champion], true), "held at approval");
    assert_eq!(s2, (vec![PrRole::Doctor], Ok(true), true), "out at Doctor");
    assert_eq!(s3, (vec![PrRole::Judge], Ok(true), true), "handed back to Judge");
    assert_eq!(s4, (vec![PrRole::Champion], true), "re-approved, still held");
    assert_eq!(s5, (vec![PrRole::Champion], false), "released by a human");
    assert_eq!(s6, (vec![], Ok(false)), "an untrusted marker serves nothing at review");
    assert!(s7.0.is_empty(), "an untrusted marker serves nothing at Doctor");
}
