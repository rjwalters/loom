//! #9389: an unlabeled, same-repo `feature/issue-<N>` PR is discoverable by
//! Judge whatever the labeled queue holds. Discovery only: comment-only
//! fallback mode, no label change, and never for a fork or another branch.
use super::*;
use crate::forge_identity::{FleetLogins, Roster};

const BASE_ID: u64 = 100;
const FORK_ID: u64 = 999;

fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&Roster::default()), None, vec!["session-app[bot]".into()])
}
fn repo(id: u64, name: &str) -> Value {
    json!({"id":id,"full_name":name})
}
/// A REST pull row: head branch `head_ref` in repository `head_repo`.
fn pr(n: u64, origin: &str, labels: &[&str], head_ref: &str, head_repo: Value) -> Value {
    let body = format!("<!-- loom:provenance v1 build=unknown unknown unknown prompts=unknown unknown sweep=none story=none trace=unknown host=unknown base=unknown run=none origin={origin} -->");
    json!({"number":n,"state":"open","draft":false,"title":"test","created_at":format!("2026-09-{n:02}"),
        "body":body,"user":{"login":"session-app[bot]","type":"Bot"},"author_association":"NONE",
        "labels":labels.iter().map(|l| json!({"name":l})).collect::<Vec<_>>(),
        "head":{"ref":head_ref,"label":format!("acme:{head_ref}"),"repo":head_repo},
        "base":{"ref":"main","repo":repo(BASE_ID, "acme/widgets")}})
}
fn shaped(n: u64, origin: &str) -> Value {
    pr(n, origin, &[], "feature/issue-7", repo(BASE_ID, "acme/widgets"))
}
fn labeled(n: u64) -> Value {
    let head = format!("feature/issue-{n}");
    pr(
        n,
        "autonomous",
        &["loom:review-requested"],
        &head,
        repo(BASE_ID, "acme/widgets"),
    )
}
fn ids(rows: &[Value]) -> Vec<u64> {
    rows.iter().map(|r| r["number"].as_u64().unwrap()).collect()
}

#[test]
fn unlabeled_issue_branch_pr_joins_a_busy_queue_behind_labeled_work() {
    // `unknown` is a bare `gh pr create`; `autonomous` from a fleet App is the
    // Builder PR whose label POST failed. Neither depends on the preference.
    for origin in ["unknown", "autonomous"] {
        for prefer in [true, false] {
            // Listed newest first: the fallback row is ahead of the labeled one.
            let rows = vec![shaped(2, origin), labeled(1)];
            let out = ordered_queue(rows, PrRole::Judge, prefer, &policy());
            assert_eq!(ids(&out), vec![1, 2], "{origin} prefer={prefer}");
            assert_eq!(out[0]["mode"], "workflow");
            assert_eq!(out[1]["mode"], "fallback");
            assert_eq!(out[1]["labels"], json!([]), "discovery never labels");
            assert_eq!(out[1]["priorityReason"], "ordinary");
        }
    }
}

#[test]
fn interactive_fallback_keeps_its_preference_and_shape_alone_goes_last() {
    let mut human = pr(3, "interactive", &[], "topic", repo(BASE_ID, "acme/widgets"));
    human["author_association"] = json!("OWNER");
    let rows = vec![shaped(4, "unknown"), human, labeled(2), labeled(1)];
    assert_eq!(
        ids(&ordered_queue(rows.clone(), PrRole::Judge, true, &policy())),
        vec![3, 2, 1, 4]
    );
    assert_eq!(ids(&ordered_queue(rows, PrRole::Judge, false, &policy())), vec![2, 1, 4]);
}

#[test]
fn a_fork_cannot_spoof_the_shape_with_a_branch_name() {
    let fork = pr(2, "unknown", &[], "feature/issue-7", repo(FORK_ID, "mallory/widgets"));
    // The fork controls its own branch name and so the `owner:branch` label.
    let mut spoofed_label = fork.clone();
    spoofed_label["head"]["label"] = json!("acme:feature/issue-7");
    // Same repository name, different repository: identity is the id.
    let same_name = pr(2, "unknown", &[], "feature/issue-7", repo(FORK_ID, "acme/widgets"));
    let deleted = pr(2, "unknown", &[], "feature/issue-7", Value::Null);
    let mut half_identified = shaped(2, "unknown");
    half_identified["head"]["repo"] = json!({"full_name":"acme/widgets"});
    for row in [fork, spoofed_label, same_name, deleted, half_identified] {
        assert!(!workflow_shaped(&row), "{}", row["head"]);
        for prefer in [true, false] {
            let out =
                ordered_queue(vec![row.clone(), labeled(1)], PrRole::Judge, prefer, &policy());
            assert_eq!(ids(&out), vec![1], "{}", row["head"]);
        }
    }
}

#[test]
fn other_branches_drafts_and_holds_stay_out_of_a_busy_queue() {
    let base = || repo(BASE_ID, "acme/widgets");
    for head_ref in [
        "topic",
        "feature/issue-",
        "feature/issue-7-retry",
        "feature/issue-7a",
        "feature/issue-+7",
        "x/feature/issue-7",
        "acme:feature/issue-7",
        "Feature/issue-7",
    ] {
        let row = pr(2, "unknown", &[], head_ref, base());
        assert!(!workflow_shaped(&row), "{head_ref}");
        let out = ordered_queue(vec![row, labeled(1)], PrRole::Judge, true, &policy());
        assert_eq!(ids(&out), vec![1], "{head_ref}");
    }
    let mut draft = shaped(2, "unknown");
    draft["draft"] = json!(true);
    let blocked = pr(3, "unknown", &["loom:blocked"], "feature/issue-7", base());
    let operator_only = pr(4, "unknown", &["loom:operator-only"], "feature/issue-7", base());
    let out = ordered_queue(
        vec![draft, blocked, operator_only, labeled(1)],
        PrRole::Judge,
        true,
        &policy(),
    );
    assert_eq!(ids(&out), vec![1]);
}

/// Repository identity falls back to the full name only when the forge sent
/// no id on either side.
#[test]
fn same_repo_by_name_only_without_any_id() {
    let mut row = shaped(2, "unknown");
    row["head"]["repo"] = json!({"full_name":"Acme/Widgets"});
    row["base"]["repo"] = json!({"full_name":"acme/widgets"});
    assert!(workflow_shaped(&row));
    row["head"]["repo"] = json!({"full_name":"mallory/widgets"});
    assert!(!workflow_shaped(&row));
    row["head"]["repo"] = json!({"full_name":""});
    row["base"]["repo"] = json!({"full_name":""});
    assert!(!workflow_shaped(&row));
}

/// When every open PR carries a `loom:*` label nothing is fallback: head
/// metadata changes neither the order nor a single field of the plan.
#[test]
fn a_fully_labeled_queue_is_unchanged_by_head_metadata() {
    let rows = vec![
        labeled(3),
        pr(2, "unknown", &["loom:pr"], "feature/issue-2", repo(BASE_ID, "acme/widgets")),
        labeled(1),
    ];
    let strip = |mut r: Value| {
        r.as_object_mut().unwrap().remove("head");
        r.as_object_mut().unwrap().remove("base");
        r
    };
    for prefer in [true, false] {
        let with = ordered_queue(rows.clone(), PrRole::Judge, prefer, &policy());
        let without = ordered_queue(
            rows.iter().cloned().map(strip).collect(),
            PrRole::Judge,
            prefer,
            &policy(),
        );
        assert_eq!(ids(&with), vec![3, 1]);
        assert!(with.iter().all(|r| r["mode"] == "workflow"));
        assert_eq!(with.into_iter().map(strip).collect::<Vec<_>>(), without);
    }
}

/// #11861 review: a Champion-held PR that Doctor handed back
/// (`loom:review-requested` + `loom:blocked`, standing trusted marker) is
/// served to Judge even when it is the only review work. Without the trusted
/// marker, it is not.
#[cfg(unix)]
#[test]
#[serial_test::serial(loom_config_env)]
fn judge_is_served_a_held_pr_handed_back_only_with_a_trusted_marker() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".loom/scripts")).unwrap();
    let gh = root.join("gh");
    std::fs::write(
        &gh,
        r#"#!/bin/sh
printf 'HTTP/2 200 OK\r\n\r\n'
case "$*" in
  *comments*) cat comments.json;;
  *issues*) cat issues.json;;
  *) cat pulls.json;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let labels = ["loom:review-requested", "loom:blocked"];
    let park = crate::park_record::render_park(
        &[],
        Some("champion"),
        None,
        Some(crate::champion_hold::MERGE_RISK_HOLD_REASON),
    );
    let mut row = pr(42, "autonomous", &labels, "feature/issue-42", repo(BASE_ID, "acme/widgets"));
    row["body"] = json!(park);
    let mut issue = row.clone();
    issue["pull_request"] = json!({});
    std::fs::write(root.join("pulls.json"), serde_json::to_vec(&vec![row]).unwrap()).unwrap();
    std::fs::write(root.join("issues.json"), serde_json::to_vec(&vec![issue]).unwrap()).unwrap();
    let marker = |body: &str| {
        let comments = json!([{"body":body,"user":{"login":"owner"},"author_association":"OWNER"}]);
        std::fs::write(root.join("comments.json"), serde_json::to_vec(&comments).unwrap()).unwrap();
    };

    let old = std::env::var_os("LOOM_GH_BIN");
    std::env::set_var("LOOM_GH_BIN", &gh);
    let mut seen = Vec::new();
    for (case, body, want) in [
        ("standing marker", crate::champion_hold::MERGE_RISK_HOLD_MARKER, true),
        ("no marker", "just a comment", false),
    ] {
        marker(body);
        let served = fetch_queue(root, &gh, PrRole::Judge).map(|q| !q.is_empty());
        seen.push((case, want, served.map_err(|e| e.to_string())));
    }
    if let Some(old) = old {
        std::env::set_var("LOOM_GH_BIN", old);
    } else {
        std::env::remove_var("LOOM_GH_BIN");
    }
    for (case, want, served) in seen {
        assert_eq!(served, Ok(want), "{case}: fetch_queue");
    }
}
