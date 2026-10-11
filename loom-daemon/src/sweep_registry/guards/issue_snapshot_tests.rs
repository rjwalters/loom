//! W9: the conditional 2.5/2.7 issue reads (`issue_snapshot`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::sweep_registry::test_support::{
    fake_gh_graphql_arm, running_issue_sweep_id, touch_sweep_command, wait_until_dead,
    FIXTURE_CHILD_WAIT_MS,
};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

/// Turns the conditional reads on for this test thread (test builds keep
/// them off unless a daemon store is opted in), and off again on drop.
struct StoreOn;
impl StoreOn {
    fn new(dir: &Path) -> Self {
        let store = dir.join("etag-store");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::forge_etag_store::set_test_daemon_store_dir(Some(store));
        Self
    }
}
impl Drop for StoreOn {
    fn drop(&mut self) {
        crate::forge_etag_store::set_test_daemon_store_dir(None);
    }
}

/// A fake `gh` serving `api --include repos/…/issues/<n>` from
/// `issue-<n>.json` + `issue-<n>.etag` in `ws` (a `304` only to the current
/// ETag; exit 1 while `fail-view` exists), the open-PR listing as empty, the
/// unconditional `--jq` reads as an open, label-less issue, and `repo view`.
/// `views.log` records `view <n> sent=<If-None-Match>` per conditional read
/// and `legacy` per unconditional one (the no-op hold's own REST read, #10156, is not a guard read).
fn snapshot_registry(ws: &Path) -> (SweepRegistry, PathBuf) {
    let views = ws.join("views.log");
    let fake_gh = ws.join("fake-gh.sh");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{ws}/gh.log"
if [[ "$1" == "api" && "$2" == "--include" ]]; then
  url="$3"; sent=""; prev=""
  for a in "$@"; do
    if [[ "$prev" == "-H" ]]; then sent="${{a#If-None-Match: }}"; fi
    prev="$a"
  done
  if [[ "$url" == */issues/* ]]; then
    n="${{url##*/}}"
    echo "view $n sent=$sent" >> "{views}"
    if [[ -f "{ws}/fail-view" ]]; then exit 1; fi
    etag="$(cat "{ws}/issue-$n.etag")"
    if [[ -n "$sent" && "$sent" == "$etag" ]]; then
      printf 'HTTP/2.0 304 Not Modified\r\nEtag: %s\r\n\r\n' "$etag"
      exit 0
    fi
    printf 'HTTP/2.0 200 OK\r\nEtag: %s\r\n\r\n' "$etag"
    cat "{ws}/issue-$n.json"
    exit 0
  fi
  printf 'HTTP/2.0 200 OK\r\nEtag: "pulls-1"\r\n\r\n[]\n'
  exit 0
fi
if [[ "$1" == "api" && "$2" == repos/* ]]; then
  if [[ "$*" != *"body: .body"* && ( "$*" == *is_pr* || "$*" == *labels* ) ]]; then echo legacy >> "{views}"; fi
  printf '%s\n' '{{"state":"open","is_pr":false}}'
  exit 0
fi
{gql}if [[ "$1" == "repo" && "$2" == "view" ]]; then
  printf 'acme/widget\n'
  exit 0
fi
exit 0
"#,
        ws = ws.display(),
        views = views.display(),
        gql = fake_gh_graphql_arm("", 0),
    );
    std::fs::write(&fake_gh, script).unwrap();
    std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let scripts = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    let spawn = scripts.join("spawn-claude.sh");
    std::fs::write(&spawn, "#!/usr/bin/env bash\necho spawned\nexit 0\n").unwrap();
    std::fs::set_permissions(&spawn, std::fs::Permissions::from_mode(0o755)).unwrap();
    touch_sweep_command(ws);
    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    let reg = SweepRegistry::new(config);
    *reg.owner_repo_cache.lock().unwrap() = Some(("acme".into(), "widget".into()));
    (reg, views)
}

/// Make issue `n` read as `state` with `labels`, under ETag `etag`.
fn set_issue(ws: &Path, n: u32, state: &str, labels: &[&str], etag: &str) {
    let labels: Vec<serde_json::Value> = labels
        .iter()
        .map(|l| serde_json::json!({"name": l, "node_id": "LA_x", "color": "000000"}))
        .collect();
    let body = serde_json::json!({
        "number": n, "state": state, "labels": labels,
        "updated_at": "2026-10-01T00:00:00Z", "title": "t", "body": null,
    });
    std::fs::write(ws.join(format!("issue-{n}.json")), body.to_string()).unwrap();
    std::fs::write(ws.join(format!("issue-{n}.etag")), format!("\"{etag}\"")).unwrap();
}

fn views(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn parse_issue_view_reads_state_pr_ness_and_labels_like_the_unconditional_probes() {
    let open = r#"{"state":"open","labels":[{"name":"loom:issue"},{"name":"points:3"}]}"#;
    assert_eq!(
        parse_issue_view(open),
        Some(IssueView {
            closed_or_pr: Some(false),
            labels: vec!["loom:issue".into(), "points:3".into()],
        })
    );
    let closed = r#"{"state":"closed","labels":[]}"#;
    assert_eq!(parse_issue_view(closed).unwrap().closed_or_pr, Some(true));
    // A PR in ANY state is terminal (#4504), an open one included.
    let pr = r#"{"state":"open","labels":[],"pull_request":{"url":"u"}}"#;
    assert_eq!(parse_issue_view(pr).unwrap().closed_or_pr, Some(true));
    let null_pr = r#"{"state":"open","labels":[],"pull_request":null}"#;
    assert_eq!(parse_issue_view(null_pr).unwrap().closed_or_pr, Some(false));
    // An unrecognised state is no verdict, never a refusal.
    let odd = r#"{"state":"weird","labels":[]}"#;
    assert_eq!(parse_issue_view(odd).unwrap().closed_or_pr, None);
    // Not an issue body: the caller falls back to the unconditional read.
    assert_eq!(parse_issue_view(r#"{"state":"open"}"#), None);
    assert_eq!(parse_issue_view(r#"{"state":"open","labels":[{"id":1}]}"#), None);
    assert_eq!(parse_issue_view("not json"), None);
}

/// The steady state W9 exists for: a repeat read of an unchanged issue sends
/// the stored ETag and is answered `304`, for both 2.5 and 2.7, and the
/// verdicts are those of the stored body.
#[test]
#[serial]
fn a_repeat_read_of_an_unchanged_issue_is_a_304_for_both_guards() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let _on = StoreOn::new(ws);
    let (reg, log) = snapshot_registry(ws);
    set_issue(ws, 9101, "open", &["loom:issue"], "e1");

    for _ in 0..2 {
        assert_eq!(reg.guard_closed_or_pr(9101), Some(false));
        assert_eq!(reg.guard_issue_labels(9101), Some(vec!["loom:issue".to_string()]));
    }
    assert_eq!(
        views(&log),
        vec![
            "view 9101 sent=",
            "view 9101 sent=",
            "view 9101 sent=\"e1\"",
            "view 9101 sent=\"e1\"",
        ],
        "each guard keeps its own entry, and the repeat is conditional"
    );
}

/// A `304` serves the stored body and the guard decides on that body: a
/// closed issue stays refused at 2.5 on a `304`, and an issue that closes
/// between reads (new ETag) is a fresh `200` that refuses.
#[test]
#[serial]
fn the_verdict_comes_from_the_body_a_304_serves() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let _on = StoreOn::new(ws);
    let (reg, log) = snapshot_registry(ws);
    set_issue(ws, 9102, "closed", &[], "c1");
    assert_eq!(reg.guard_closed_or_pr(9102), Some(true));
    assert_eq!(reg.guard_closed_or_pr(9102), Some(true), "the 304's stored body is closed");

    set_issue(ws, 9103, "open", &[], "o1");
    assert_eq!(reg.guard_closed_or_pr(9103), Some(false));
    set_issue(ws, 9103, "closed", &[], "o2");
    assert_eq!(reg.guard_closed_or_pr(9103), Some(true), "a changed issue is a fresh 200");
    assert!(
        views(&log).contains(&"view 9103 sent=\"o1\"".to_string()),
        "the stale ETag was sent and the forge answered with the new body"
    );
}

/// Read-after-own-write: after this process writes issue N through the
/// facade, the next guard reads of N send no `If-None-Match` even though an
/// entry exists and the forge would answer `304`.
#[test]
#[serial]
fn a_read_after_our_own_write_is_unconditional() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let _on = StoreOn::new(ws);
    let (reg, log) = snapshot_registry(ws);
    set_issue(ws, 9104, "open", &["loom:issue"], "w1");
    assert_eq!(reg.guard_closed_or_pr(9104), Some(false));
    assert_eq!(reg.guard_issue_labels(9104), Some(vec!["loom:issue".to_string()]));

    // Any write-intent facade invocation naming #9104 pins it.
    let _ = reg
        .gh_write("guard.flip_building", ["issue", "edit", "9104", "--add-label", "loom:building"]);
    assert_eq!(reg.guard_closed_or_pr(9104), Some(false));
    assert_eq!(reg.guard_issue_labels(9104), Some(vec!["loom:issue".to_string()]));
    assert_eq!(
        &views(&log)[2..],
        &["view 9104 sent=".to_string(), "view 9104 sent=".to_string()],
        "both reads after the write went out without an ETag"
    );
}

/// Fail-open: when the conditional read fails, each guard runs today's
/// unconditional probe and returns its verdict.
#[test]
#[serial]
fn a_failed_conditional_read_falls_back_to_the_unconditional_probe() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let _on = StoreOn::new(ws);
    let (reg, log) = snapshot_registry(ws);
    set_issue(ws, 9105, "closed", &["loom:blocked"], "f1");
    std::fs::write(ws.join("fail-view"), "").unwrap();
    // The unconditional fake answers "open, no labels": the fallback's verdict.
    assert_eq!(reg.guard_closed_or_pr(9105), Some(false));
    assert!(reg
        .guard_issue_labels(9105)
        .is_some_and(|l| !l.iter().any(|x| x == "loom:blocked")));
    let seen = views(&log);
    assert_eq!(seen.iter().filter(|l| *l == "legacy").count(), 2, "{seen:?}");
    assert_eq!(crate::forge_call_stats::counters::get(FALLBACK_COUNTER), 2);
}

/// Kill switch: `LOOM_GUARD_ISSUE_SNAPSHOT=0` issues only the unconditional
/// reads.
#[test]
#[serial]
fn the_kill_switch_restores_the_unconditional_reads() {
    std::env::set_var(GUARD_ISSUE_SNAPSHOT_ENV, "0");
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let _on = StoreOn::new(ws);
    let (reg, log) = snapshot_registry(ws);
    set_issue(ws, 9106, "open", &[], "k1");
    let _ = reg.guard_closed_or_pr(9106);
    let _ = reg.guard_issue_labels(9106);
    std::env::remove_var(GUARD_ISSUE_SNAPSHOT_ENV);
    assert_eq!(views(&log), vec!["legacy", "legacy"]);
}

/// Full dispatch: a closed issue served from a `304` is refused at 2.5 with
/// no label flip, and an open one passes every guard on conditional reads
/// alone (no unconditional issue read before the flip).
#[test]
#[serial]
fn dispatch_runs_its_guards_on_the_conditional_reads() {
    std::env::remove_var("LOOM_REPO");
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let _on = StoreOn::new(ws);
    let (mut reg, log) = snapshot_registry(ws);

    set_issue(ws, 9107, "closed", &[], "d1");
    assert_eq!(reg.guard_closed_or_pr(9107), Some(true));
    let err = reg
        .dispatch(&SweepKind::Issue(9107), None, None, None, None)
        .expect_err("a closed issue is refused at 2.5");
    assert!(err.to_string().contains("closed-issue guard"), "{err:#}");
    assert!(views(&log).contains(&"view 9107 sent=\"d1\"".to_string()));
    let gh = std::fs::read_to_string(ws.join("gh.log")).unwrap_or_default();
    assert!(!gh.contains("issue edit 9107"), "no flip on a refused dispatch: {gh}");

    set_issue(ws, 9108, "open", &["loom:issue"], "p1");
    let out = reg
        .dispatch(&SweepKind::Issue(9108), None, None, None, None)
        .expect("an open, unparked issue with no open PR dispatches");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));
    let seen = views(&log);
    assert!(
        seen.iter().filter(|l| l.starts_with("view 9108 ")).count() >= 2,
        "2.5 and 2.7 read conditionally: {seen:?}"
    );
    assert!(!seen.iter().any(|l| l == "legacy"), "no unconditional issue read: {seen:?}");
    if let Some(id) = running_issue_sweep_id(&reg, 9108) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
}

/// Attribution is unchanged (acceptance criterion): an issue that is parked
/// AND cooling down AND backed off is refused by the 2.7 park guard, read
/// from the conditional labels read.
#[test]
#[serial]
fn the_park_guard_still_wins_attribution_on_the_conditional_read() {
    std::env::remove_var("LOOM_REPO");
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let _on = StoreOn::new(ws);
    let (mut reg, log) = snapshot_registry(ws);
    set_issue(ws, 9109, "open", &["loom:blocked"], "b1");
    reg.record_noop_release(9109, Some("still blocked".into()));
    reg.record_open_pr_guard_backoff(9109);
    let err = reg
        .dispatch(&SweepKind::Issue(9109), None, None, None, None)
        .expect_err("a parked issue is refused");
    let typed = err
        .downcast_ref::<ParkedIssueDispatchError>()
        .expect("the 2.7 park guard wins over the 2.75 cooldown and the 2.8 backoff");
    assert_eq!(typed.issue, 9109);
    assert!(!views(&log).iter().any(|l| l == "legacy"));
}

/// Cost (W9): an own write drops the `If-None-Match` from both guard reads
/// but moves neither identity. 2.5 stays on its reader; 2.7 is on the writer
/// with or without a write.
#[test]
fn an_own_write_drops_the_etag_but_keeps_each_guards_identity() {
    use crate::forge_etag_store::ReadPin;
    let state = |own_write| guard_read_pin(false, own_write);
    let labels = |own_write| guard_read_pin(true, own_write);
    assert_eq!(state(false), ReadPin::default(), "2.5: reader-first, conditional");
    assert_eq!(
        state(true),
        ReadPin {
            writer: false,
            unconditional: true
        },
        "2.5 after an own write: still the reader, no If-None-Match"
    );
    assert_eq!(
        labels(false),
        ReadPin {
            writer: true,
            unconditional: false
        }
    );
    assert_eq!(
        labels(true),
        ReadPin {
            writer: true,
            unconditional: true
        },
        "2.7 after an own write: the writer, no If-None-Match"
    );
}
