use super::tree_carveout_e2e::with_env;
use super::{forge, SHA_A, SHA_B};
use crate::claim_reconciliation::{open_pr_listing::test_support, VERDICT_STALENESS_ENABLED_ENV};
use serde_json::json;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .current_dir(root)
        .args(args)
        .status()
        .unwrap()
        .success());
}

#[test]
#[serial]
fn periodic_reconcile_requires_an_exact_intent_and_positive_queue_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/held-review.git",
        ],
    );
    std::fs::create_dir(root.join(".loom")).unwrap();
    let gh = root.join("fake-gh");
    let command = format!(
        r#"#!/usr/bin/env bash
set -eu
root='{root}'
{listing}
if [ "$1" = api ]; then
  case "$2" in
    repos/acme/held-review) echo '{{"push":true}}'; exit 0 ;;
    repos/acme/held-review/pulls/147) cat "$root/pull.json"; exit 0 ;;
    repos/acme/held-review/issues/147/comments*) cat "$root/comments.json"; exit 0 ;;
    graphql) cat "$root/queue.json"; exit 0 ;;
    -X)
      printf '%s\n' "$*" >> "$root/writes"
      case "$3:$4" in
        POST:repos/acme/held-review/issues/147/labels)
          cp "$root/after-add.json" "$root/pull.json";;
        DELETE:repos/acme/held-review/issues/147/labels/loom%3Apr)
          cp "$root/final.json" "$root/pull.json";;
        *) exit 45;;
      esac
      exit 0;;
  esac
fi
exit 47
"#,
        root = root.display(),
        listing = test_support::pulls_arm(&[
            test_support::row(147, &["loom:pr", "loom:blocked"]).sha(SHA_B)
        ]),
    );
    std::fs::write(&gh, command).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let put = |name: &str, value: serde_json::Value| {
        std::fs::write(root.join(name), value.to_string()).unwrap();
    };
    let pull = |labels: &[&str]| {
        json!({
            "state":"open", "head":{"sha":SHA_B}, "auto_merge":null, "body":"",
            "labels": labels.iter().map(|name| json!({"name":name})).collect::<Vec<_>>(),
        })
    };
    put("pull.json", pull(&["loom:blocked", "loom:pr"]));
    put("after-add.json", pull(&["loom:blocked", "loom:pr", "loom:review-requested"]));
    put("final.json", pull(&["loom:blocked", "loom:review-requested"]));
    let row = |id: u64, body: String| {
        json!({
            "id":id, "user":{"login":"loom-fleet-dispatch[bot]","type":"Bot"},
            "author_association":"NONE", "body":body,
        })
    };
    let mut comments = vec![
        row(1, format!("<!-- loom:verdict-sha sha={SHA_A} verdict=approved -->")),
        row(
            2,
            format!(
                "<!-- champion:critical-file-hold -->\n<!-- champion:hold-state head={SHA_A} -->"
            ),
        ),
    ];
    put("comments.json", json!(comments));
    let locks = root.join("verdict-locks");
    let run = || {
        with_env(
            &[
                (VERDICT_STALENESS_ENABLED_ENV, Some("1")),
                ("GH_REPO", None),
                ("LOOM_VERDICT_LOCK_DIR", Some(locks.to_str().unwrap())),
            ],
            || forge::reconcile_pr_verdicts(&gh, root),
        )
    };
    assert_eq!(run().invalidated, 0);
    assert!(!root.join("writes").exists());

    comments.push(row(
        3,
        format!("<!-- loom:held-doctor-review from={SHA_A} to={SHA_B} hold=2 -->"),
    ));
    put("comments.json", json!(comments));
    let response = |pr| json!({"data":{"repository":{"pullRequest":pr}}});
    put(
        "queue.json",
        response(json!({"id":"PR_FAKE","state":"OPEN","headRefOid":SHA_B})),
    );
    assert_eq!(run().invalidated, 0, "missing mergeQueueEntry is not an idle read");
    assert!(!root.join("writes").exists());

    put(
        "queue.json",
        response(json!({"id":"PR_FAKE","state":"OPEN","headRefOid":SHA_B,"mergeQueueEntry":null})),
    );
    assert_eq!(run().invalidated, 1);
    assert_eq!(run().invalidated, 0, "repeated list result does not duplicate writes");
    let writes = std::fs::read_to_string(root.join("writes")).unwrap();
    assert_eq!(writes.lines().count(), 2, "{writes}");
    assert!(writes.contains("labels[]=loom:review-requested"));
    assert!(writes.contains("/labels/loom%3Apr"));
    assert!(!writes.contains("loom:blocked"));
}
