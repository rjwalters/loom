//! Tests for the intake reconcile singleton (W7): the mode table, the
//! fail-closed repo set, and the pass's re-read before every label.

use super::*;
use std::os::unix::fs::PermissionsExt;

fn now() -> DateTime<Utc> {
    "2026-10-03T12:00:00Z".parse().unwrap()
}

fn config(dir: &Path, json: &str) {
    let path = dir.join(crate::config_resolver::LEGACY_CONFIG_REL);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json).unwrap();
}

/// A job name no other test arms, so the process-wide registries stay ours.
fn job(tag: &str) -> String {
    format!("intake-test-{tag}-{}", std::process::id())
}

fn armed(job: &str) -> bool {
    captain::armed_singleton_job_names()
        .iter()
        .any(|j| j == job)
}

fn captainless(job: &str) -> bool {
    captain::captainless_singleton_job_names()
        .iter()
        .any(|j| j == job)
}

// ===== modes =====

#[test]
fn the_mode_table() {
    let armed = CaptainGate::Armed {
        captain: "cap".into(),
    };
    let refused = CaptainGate::Refused {
        captain: "cap".into(),
        current_host_id: "other".into(),
    };
    let legacy = |reason| Mode::Legacy { reason };
    // The key absent or false: per host, whatever the captain gate says.
    for gate in [&armed, &refused, &CaptainGate::NoCaptainDeclared] {
        assert_eq!(resolve_mode(gate, false), legacy("not_configured"));
    }
    // The key set, no captain: still per host (fail-open).
    assert_eq!(resolve_mode(&CaptainGate::NoCaptainDeclared, true), legacy("no_captain"));
    assert_eq!(
        resolve_mode(&armed, true),
        Mode::Captain {
            captain: "cap".into()
        }
    );
    assert_eq!(
        resolve_mode(&refused, true),
        Mode::StandDown {
            captain: "cap".into()
        }
    );
}

#[test]
fn only_an_explicit_true_enables_the_singleton() {
    let enabled = |json: &str| singleton_enabled(&serde_json::from_str(json).unwrap());
    assert!(enabled(r#"{"fleet": {"intakeReconcile": {"singleton": true}}}"#));
    assert!(!enabled(r#"{"fleet": {"intakeReconcile": {"singleton": false}}}"#));
    assert!(!enabled(r#"{"fleet": {"intakeReconcile": {"singleton": "yes"}}}"#));
    assert!(!enabled(r#"{"fleet": {"intakeReconcile": {}}}"#));
    assert!(!enabled(r#"{"fleet": {"captain": "cap"}}"#));
    assert!(!enabled("{}"));
}

/// With the new config absent the legacy pass is untouched: it keeps running
/// per host, on the captain and off it, with or without a captain declared,
/// and nothing is armed or reported captainless.
#[test]
fn config_absent_is_the_legacy_behaviour_on_every_host() {
    let job = job("absent");
    for json in ["{}", r#"{"fleet": {"captain": "cap"}}"#] {
        let dir = tempfile::tempdir().unwrap();
        config(dir.path(), json);
        for host in ["cap", "other"] {
            let mode = gate_as(&job, dir.path(), host);
            assert_eq!(
                mode,
                Mode::Legacy {
                    reason: "not_configured"
                },
                "{json} on {host}"
            );
            assert!(mode.legacy_runs());
            assert!(!stands_down(Some(&mode)));
            assert!(!armed(&job) && !captainless(&job), "{json} on {host}");
        }
    }
    // A process whose task never resolved a mode (a CLI, a test) is legacy too.
    assert!(!stands_down(None));
}

#[test]
fn enabled_with_a_captain_only_the_captain_runs_and_only_it_is_armed() {
    let job = job("enabled");
    let dir = tempfile::tempdir().unwrap();
    config(
        dir.path(),
        r#"{"fleet": {"captain": "cap", "intakeReconcile": {"singleton": true}}}"#,
    );
    // A dispatcher: no inline pass, nothing armed.
    let dispatcher = gate_as(&job, dir.path(), "other");
    assert_eq!(
        dispatcher,
        Mode::StandDown {
            captain: "cap".into()
        }
    );
    assert!(stands_down(Some(&dispatcher)));
    assert!(!armed(&job));
    // The captain: the inline pass stands down there too (its task runs
    // intake), and the job is armed.
    let on_captain = gate_as(&job, dir.path(), "cap");
    assert_eq!(
        on_captain,
        Mode::Captain {
            captain: "cap".into()
        }
    );
    assert!(stands_down(Some(&on_captain)));
    assert!(armed(&job));
    // The key is removed: back to per host, and disarmed within one tick.
    config(dir.path(), r#"{"fleet": {"captain": "cap"}}"#);
    assert!(gate_as(&job, dir.path(), "cap").legacy_runs());
    assert!(!armed(&job));
}

#[test]
fn enabled_without_a_captain_stays_per_host_and_is_not_reported_captainless() {
    let job = job("nocaptain");
    let dir = tempfile::tempdir().unwrap();
    config(dir.path(), r#"{"fleet": {"intakeReconcile": {"singleton": true}}}"#);
    let mode = gate_as(&job, dir.path(), "any");
    assert_eq!(
        mode,
        Mode::Legacy {
            reason: "no_captain"
        }
    );
    assert!(!stands_down(Some(&mode)));
    assert!(!armed(&job) && !captainless(&job));
}

// ===== repo set =====

fn repo(slug: &str) -> Repo {
    Repo {
        slug: slug.into(),
        host: None,
        root: PathBuf::from(format!("/ws/{slug}")),
    }
}

#[test]
fn the_repo_set_is_one_entry_per_slug_and_leaves_unresolvable_roots_out() {
    let roots: Vec<PathBuf> = ["/ws/b", "/ws/none", "/ws/a", "/ws/a-copy"]
        .iter()
        .map(PathBuf::from)
        .collect();
    let repos = repos_for(&roots, |root| match root.to_str().unwrap() {
        "/ws/a" => Some((Some("github.com".into()), "acme/a".into())),
        "/ws/a-copy" => Some((None, "Acme/A".into())),
        "/ws/b" => Some((None, "acme/b".into())),
        _ => None,
    });
    let slugs: Vec<&str> = repos.iter().map(|r| r.slug.as_str()).collect();
    assert_eq!(slugs, ["acme/a", "acme/b"]);
    assert_eq!(repos[0].root, PathBuf::from("/ws/a"));
    assert_eq!(repos[0].host.as_deref(), Some("github.com"));
}

/// A scripted forge recording every call.
#[derive(Default)]
struct Fake {
    listing: Vec<IntakeRow>,
    listing_fails: bool,
    listing_shed: bool,
    listing_deadline: bool,
    /// Issue number → what the re-read answers (absent = read failure).
    current: std::collections::BTreeMap<u32, Option<Vec<String>>>,
    write_denied: bool,
    post_fails: bool,
    /// Issue number → the label set the write answers (absent = just
    /// `loom:triage`).
    post_answers: std::collections::BTreeMap<u32, Vec<String>>,
    remove_fails: bool,
    /// Issue number → its `labeled` events (absent = events read failure).
    events: std::collections::BTreeMap<u32, Vec<LabelEvent>>,
    /// Labels whose removal fails.
    remove_label_fails: bool,
    calls: Vec<String>,
}

impl IntakeForge for Fake {
    fn list_open(&mut self, repo: &Repo) -> Result<Vec<IntakeRow>> {
        self.calls.push(format!("list {}", repo.slug));
        if self.listing_shed {
            return Err(anyhow::Error::new(store::ReadShed {
                until: std::time::SystemTime::now(),
            }));
        }
        if self.listing_deadline {
            return Err(anyhow::Error::new(store::TickDeadlineReached));
        }
        if self.listing_fails {
            return Err(anyhow!("listing changed mid-walk"));
        }
        Ok(self.listing.clone())
    }

    fn may_write(&mut self, _: &Repo) -> bool {
        self.calls.push("scope".into());
        !self.write_denied
    }

    fn remove_triage(&mut self, _: &Repo, number: u32) -> bool {
        self.calls.push(format!("delete {number}"));
        !self.remove_fails
    }
}

impl DemoteForge<Repo> for Fake {
    fn current_labels(&mut self, _: &Repo, number: u32) -> Result<Option<Vec<String>>> {
        self.calls.push(format!("reread {number}"));
        self.current
            .get(&number)
            .cloned()
            .ok_or_else(|| anyhow!("HTTP 502"))
    }

    fn add_triage(&mut self, _: &Repo, number: u32) -> Option<Vec<String>> {
        self.calls.push(format!("post {number}"));
        (!self.post_fails).then(|| {
            self.post_answers
                .get(&number)
                .cloned()
                .unwrap_or_else(|| vec![TRIAGE_LABEL.to_string()])
        })
    }

    fn label_events(&mut self, _: &Repo, number: u32) -> Result<Vec<LabelEvent>> {
        self.calls.push(format!("events {number}"));
        self.events
            .get(&number)
            .cloned()
            .ok_or_else(|| anyhow!("HTTP 502"))
    }

    fn remove_label(&mut self, _: &Repo, number: u32, label: &str) -> bool {
        self.calls.push(format!("unlabel {number} {label}"));
        !self.remove_label_fails
    }

    fn comment(&mut self, _: &Repo, number: u32, _: &str) -> bool {
        self.calls.push(format!("comment {number}"));
        true
    }
}

fn row(number: u32, labels: &[&str]) -> IntakeRow {
    IntakeRow {
        number,
        created_at: Some("2026-09-01T00:00:00Z".parse().unwrap()),
        labels: labels.iter().map(|s| (*s).to_string()).collect(),
        author: None,
    }
}

fn labels(names: &[&str]) -> Option<Vec<String>> {
    Some(names.iter().map(|s| (*s).to_string()).collect())
}

/// No slug list means no pass: not one forge call.
#[test]
fn no_slug_list_means_no_pass() {
    let roots = vec![PathBuf::from("/ws/unresolvable")];
    let repos = repos_for(&roots, |_| None);
    assert!(repos.is_empty());
    let mut forge = Fake {
        listing: vec![row(1, &[])],
        ..Fake::default()
    };
    let pass = run_pass(&mut forge, &repos, now(), 50, &|| false);
    assert_eq!(pass, PassReport::default());
    assert!(forge.calls.is_empty(), "{:?}", forge.calls);
}

/// An issue labelled (or closed) between the listing and the write is left
/// alone: the re-read sees the label and no label request is made for it.
#[test]
fn an_issue_labelled_between_the_listing_and_the_post_is_not_labelled_again() {
    let mut forge = Fake {
        // The listing snapshot: #1, #2 and #3 all look unlabelled.
        listing: vec![
            row(1, &[]),
            row(2, &["bug"]),
            row(3, &[]),
            row(4, &["loom:issue"]),
        ],
        ..Fake::default()
    };
    // Since then #1 was curated by someone else and #3 was closed.
    forge.current.insert(1, labels(&["loom:curated"]));
    forge.current.insert(2, labels(&["bug"]));
    forge.current.insert(3, None);
    let report = run_repo(&mut forge, &repo("acme/a"), now(), 50);
    assert_eq!(
        forge.calls,
        [
            "list acme/a",
            "scope",
            "reread 1",
            "reread 2",
            "post 2",
            "reread 3"
        ]
    );
    assert_eq!((report.labelled, report.raced), (1, 2));
}

#[test]
fn an_idle_repo_costs_a_listing_and_nothing_else() {
    let mut forge = Fake {
        listing: vec![row(1, &["loom:triage"]), row(2, &["loom:issue", "bug"])],
        ..Fake::default()
    };
    assert_eq!(run_repo(&mut forge, &repo("acme/a"), now(), 50), RepoReport::default());
    // No write-scope probe, no re-read, no write.
    assert_eq!(forge.calls, ["list acme/a"]);
}

#[test]
fn nothing_is_labelled_without_write_scope_a_listing_or_a_re_read() {
    // Write scope refused: no re-read, no write.
    let mut denied = Fake {
        listing: vec![row(1, &[])],
        write_denied: true,
        ..Fake::default()
    };
    assert!(run_repo(&mut denied, &repo("acme/a"), now(), 50).denied);
    assert_eq!(denied.calls, ["list acme/a", "scope"]);

    // An incomplete listing is a skipped repo.
    let mut failed = Fake {
        listing_fails: true,
        ..Fake::default()
    };
    assert!(run_repo(&mut failed, &repo("acme/a"), now(), 50).listing_failed);
    assert_eq!(failed.calls, ["list acme/a"]);

    // A failed re-read stops the repo's pass before any write.
    let mut blind = Fake {
        listing: vec![row(1, &[]), row(2, &[])],
        ..Fake::default()
    };
    assert_eq!(run_repo(&mut blind, &repo("acme/a"), now(), 50).labelled, 0);
    assert_eq!(blind.calls, ["list acme/a", "scope", "reread 1"]);

    // A failed write stops it too (likely rate limited).
    let mut refused = Fake {
        listing: vec![row(1, &[]), row(2, &[])],
        post_fails: true,
        ..Fake::default()
    };
    refused.current.insert(1, labels(&[]));
    refused.current.insert(2, labels(&[]));
    assert_eq!(run_repo(&mut refused, &repo("acme/a"), now(), 50).labelled, 0);
    assert_eq!(refused.calls, ["list acme/a", "scope", "reread 1", "post 1"]);
}

#[test]
fn the_pass_covers_every_repo_and_stops_while_the_breaker_is_open() {
    let repos = [repo("acme/a"), repo("acme/b")];
    let mut forge = Fake {
        listing: vec![row(1, &[])],
        ..Fake::default()
    };
    forge.current.insert(1, labels(&[]));
    let pass = run_pass(&mut forge, &repos, now(), 50, &|| false);
    assert_eq!((pass.repos, pass.labelled, pass.raced), (2, 2, 0));

    // The breaker opens after the first repo: the second is not listed.
    let mut forge = Fake::default();
    let seen = std::cell::Cell::new(0);
    let tripped = || seen.replace(seen.get() + 1) >= 1;
    let pass = run_pass(&mut forge, &repos, now(), 50, &tripped);
    assert_eq!(pass.repos, 1);
    assert_eq!(forge.calls, ["list acme/a"]);

    // Already open: not one call.
    let mut forge = Fake::default();
    run_pass(&mut forge, &repos, now(), 50, &|| true);
    assert!(forge.calls.is_empty());
}

// ===== the completed-pass heartbeat (#10924) =====

fn t(mins: i64) -> DateTime<Utc> {
    now() + chrono::Duration::minutes(mins)
}

/// A pass that walks every repo and reads every listing stamps the success
/// heartbeat even when it labels nothing: a quiet fleet is not a dead one.
#[test]
fn a_completed_quiet_pass_stamps_the_success_heartbeat() {
    let repos = [repo("acme/a"), repo("acme/b")];
    let mut forge = Fake {
        listing: vec![row(1, &["loom:triage"])], // nothing to label
        ..Fake::default()
    };
    let pass = run_pass(&mut forge, &repos, now(), 50, &|| false);
    assert!(pass.completed_scan(repos.len()));
    let hb = next_heartbeat(None, "cap", t(0), &pass, repos.len());
    assert_eq!(hb.success_at(), Some(t(0)));
    assert_eq!((hb.repos, hb.labelled), (2, 0));
}

/// The write-scope gate refusing a repo does not undo a completed scan:
/// listing outcomes are the scan, labels retry next pass by design.
#[test]
fn a_denied_write_does_not_undo_a_completed_scan() {
    let repos = [repo("acme/a")];
    let mut forge = Fake {
        listing: vec![row(1, &[])], // something to label
        write_denied: true,
        ..Fake::default()
    };
    let pass = run_pass(&mut forge, &repos, now(), 50, &|| false);
    assert!(pass.completed_scan(repos.len()));
    assert_eq!(next_heartbeat(None, "cap", t(0), &pass, repos.len()).success_at(), Some(t(0)));
}

/// A shed, failed or early-stopped pass cannot refresh the success stamp:
/// the previous completed pass's timestamp is kept.
#[test]
fn shed_failed_and_stopped_early_passes_keep_the_previous_stamp() {
    let repos = [repo("acme/a"), repo("acme/b")];
    let stamped = next_heartbeat(
        None,
        "cap",
        t(0),
        &PassReport {
            repos: 2,
            ..PassReport::default()
        },
        2,
    );
    // Shed: every reader out of budget.
    let mut forge = Fake {
        listing_shed: true,
        ..Fake::default()
    };
    let pass = run_pass(&mut forge, &repos, now(), 50, &|| false);
    assert!((pass.listing_failed, pass.shed) == (2, 2));
    assert_eq!(next_heartbeat(Some(&stamped), "cap", t(5), &pass, 2).success_at(), Some(t(0)));
    // Failed listing.
    let mut forge = Fake {
        listing_fails: true,
        ..Fake::default()
    };
    let pass = run_pass(&mut forge, &repos, now(), 50, &|| false);
    assert_eq!(pass.listing_failed, 2);
    assert_eq!(next_heartbeat(Some(&stamped), "cap", t(5), &pass, 2).success_at(), Some(t(0)));
    // Stopped early by the breaker after the first repo.
    let mut forge = Fake::default();
    let seen = std::cell::Cell::new(0);
    let tripped = || seen.replace(seen.get() + 1) >= 1;
    let pass = run_pass(&mut forge, &repos, now(), 50, &tripped);
    assert!(pass.stopped_early && pass.repos == 1);
    assert_eq!(next_heartbeat(Some(&stamped), "cap", t(5), &pass, 2).success_at(), Some(t(0)));
    // And with no previous stamp at all: never completed.
    assert_eq!(next_heartbeat(None, "cap", t(5), &pass, 2).success_at(), None);
}

/// The heartbeat file: round trip, and a record of another schema proves
/// nothing (a foreign file is not a success stamp).
#[test]
fn the_heartbeat_file_round_trips_and_rejects_other_schemas() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_heartbeat(dir.path()).is_none());
    let hb = next_heartbeat(
        None,
        "cap",
        t(0),
        &PassReport {
            repos: 1,
            ..PassReport::default()
        },
        1,
    );
    write_heartbeat(dir.path(), &hb);
    assert_eq!(read_heartbeat(dir.path()), Some(hb));
    let foreign = br#"{"schema": "intake-reconcile/pass/v0", "host": "cap"}"#;
    std::fs::write(heartbeat_path(dir.path()), foreign).unwrap();
    assert!(read_heartbeat(dir.path()).is_none());
}

// ===== the live forge, against a stub =====

#[test]
fn the_re_read_answers_none_for_anything_but_an_open_issue() {
    let open = r#"{"state": "open", "labels": [{"name": "bug"}, {"name": "loom:triage"}]}"#;
    assert_eq!(open_issue_labels(open).unwrap(), labels(&["bug", "loom:triage"]));
    assert_eq!(open_issue_labels(r#"{"state": "open", "labels": []}"#).unwrap(), labels(&[]));
    assert_eq!(open_issue_labels(r#"{"state": "closed", "labels": []}"#).unwrap(), None);
    let pull = r#"{"state": "open", "labels": [], "pull_request": {"url": "x"}}"#;
    assert_eq!(open_issue_labels(pull).unwrap(), None);
    // A body that does not say is a failed read, never "unlabelled".
    assert!(open_issue_labels(r#"{"state": "open"}"#).is_err());
    assert!(open_issue_labels("not json").is_err());
}

/// The live forge with the write-scope probe answered `yes` (the probe is
/// `write_scope`'s own, tested there).
struct Scoped(GhIntakeForge);

impl IntakeForge for Scoped {
    fn list_open(&mut self, repo: &Repo) -> Result<Vec<IntakeRow>> {
        self.0.list_open(repo)
    }
    fn may_write(&mut self, _: &Repo) -> bool {
        true
    }
    fn remove_triage(&mut self, repo: &Repo, number: u32) -> bool {
        self.0.remove_triage(repo, number)
    }
}

impl DemoteForge<Repo> for Scoped {
    fn current_labels(&mut self, repo: &Repo, number: u32) -> Result<Option<Vec<String>>> {
        self.0.current_labels(repo, number)
    }
    fn add_triage(&mut self, repo: &Repo, number: u32) -> Option<Vec<String>> {
        self.0.add_triage(repo, number)
    }
    fn label_events(&mut self, repo: &Repo, number: u32) -> Result<Vec<LabelEvent>> {
        self.0.label_events(repo, number)
    }
    fn remove_label(&mut self, repo: &Repo, number: u32, label: &str) -> bool {
        self.0.remove_label(repo, number, label)
    }
    fn comment(&mut self, repo: &Repo, number: u32, body: &str) -> bool {
        self.0.comment(repo, number, body)
    }
}

/// End to end through the real read and write paths: the listing drops the
/// pull request, #4 was labelled after the listing and is left alone, only
/// #1 gets the label, and the next listing of the unchanged repo is one `304`.
#[test]
#[serial_test::serial]
fn the_live_pass_re_reads_each_issue_and_an_idle_repo_is_a_304() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    let listing = r#"[
 {"number": 1, "state": "open", "created_at": "2026-09-01T00:00:00Z", "labels": []},
 {"number": 2, "state": "open", "created_at": "2026-09-01T00:00:00Z", "labels": [], "pull_request": {"url": "x"}},
 {"number": 3, "state": "open", "created_at": "2026-09-01T00:00:00Z", "labels": [{"name": "loom:curated"}]},
 {"number": 4, "state": "open", "created_at": "2026-09-02T00:00:00Z", "labels": []}
]"#;
    std::fs::write(dir.path().join("listing.json"), listing).unwrap();
    let script = format!(
        r#"#!/bin/sh
d={d}
ok() {{ printf 'HTTP/2.0 200 OK\r\nEtag: W/"%s"\r\n\r\n' "$1"; }}
case "$*" in
  *"-X POST"*) echo "$*" >> "$d/posts.log"
    echo '[{{"name": "loom:triage"}}]' ;;
  *"-X DELETE"*) echo "$*" >> "$d/deletes.log" ;;
  *If-None-Match*issues/[0-9]*) echo "conditional re-read: $*" >> "$d/calls.log"; exit 1 ;;
  *issues/1*) echo "reread 1" >> "$d/calls.log"; ok i1
    echo '{{"number": 1, "state": "open", "labels": []}}' ;;
  *issues/4*) echo "reread 4" >> "$d/calls.log"; ok i4
    echo '{{"number": 4, "state": "open", "labels": [{{"name": "loom:issue"}}]}}' ;;
  *"issues?state=open&sort=created&direction=asc"*)
    case "$*" in
      *'If-None-Match: W/"l1"'*)
        echo "list 304" >> "$d/calls.log"
        printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
        echo 'gh: Not Modified (HTTP 304)' 1>&2
        exit 1 ;;
    esac
    echo "list 200" >> "$d/calls.log"; ok l1
    cat "$d/listing.json" ;;
  *) echo "unexpected: $*" >> "$d/calls.log"; exit 1 ;;
esac
"#
    );
    let gh = dir.path().join("gh");
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let repo = Repo {
        slug: "acme/widgets".into(),
        host: None,
        root: dir.path().to_path_buf(),
    };
    let mut forge = Scoped(GhIntakeForge::new(gh));

    let report = run_repo(&mut forge, &repo, now(), 50);
    assert_eq!((report.labelled, report.raced), (1, 1), "{report:?}");
    let calls = || std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
    assert_eq!(calls(), "list 200\nreread 1\nreread 4\n");
    let posts = std::fs::read_to_string(dir.path().join("posts.log")).unwrap();
    assert_eq!(posts.lines().count(), 1, "{posts}");
    assert!(posts.contains("repos/acme/widgets/issues/1/labels"), "{posts}");
    assert!(posts.contains("labels[]=loom:triage"), "{posts}");

    // Nothing moved on the forge since: the next listing presents the stored
    // validator and costs exactly one 304, served from the cached page.
    std::fs::remove_file(dir.path().join("calls.log")).unwrap();
    let rows = forge.list_open(&repo).unwrap();
    assert_eq!(calls(), "list 304\n");
    assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [1, 3, 4]);
    assert!(!dir.path().join("deletes.log").exists(), "nothing was reverted");

    // The re-read is unconditional every time (the stub fails a conditional
    // one): a second re-read of #1 sends no validator either.
    std::fs::remove_file(dir.path().join("calls.log")).unwrap();
    assert_eq!(forge.current_labels(&repo, 1).unwrap(), labels(&[]));
    assert_eq!(calls(), "reread 1\n");
}

/// #12118 / #12316: the intake listing, not a telemetry sampler, journals
/// `issue.estimate` for a freshly curated issue, once.
#[test]
#[serial_test::serial]
fn the_live_listing_journals_issue_estimate_for_a_fresh_curated_issue() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    let updated = Utc::now().to_rfc3339();
    let listing = serde_json::json!([{
        "number": 7, "state": "open", "created_at": updated, "updated_at": updated,
        "labels": [{"name": "loom:curated"}],
        "body": "<!-- loom:estimate lines=7 files=1 doctor_risk=0.3 -->",
        "user": {"login": "owner"}, "author_association": "OWNER"
    }]);
    std::fs::write(dir.path().join("listing.json"), listing.to_string()).unwrap();
    let script = format!(
        r#"#!/bin/sh
d={d}
printf 'HTTP/2.0 200 OK\r\nEtag: W/"l1"\r\n\r\n'
cat "$d/listing.json"
"#
    );
    let gh = dir.path().join("gh");
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let repo = Repo {
        slug: "acme/widgets".into(),
        host: None,
        root: dir.path().to_path_buf(),
    };
    let mut forge = GhIntakeForge::new(gh);
    forge.list_open(&repo).unwrap();
    let journal = || {
        std::fs::read_dir(dir.path().join(".loom/logs/issue-estimates"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    };
    assert_eq!(journal(), 1);
    forge.list_open(&repo).unwrap();
    assert_eq!(journal(), 1, "re-observing the same marker journals nothing");
}

/// The write's own answer is the compare step: when it shows another
/// `loom:*` label, the `loom:triage` just added is removed (a DELETE on the
/// writer) and counted.
#[test]
fn the_live_write_reverts_when_its_answer_carries_another_lifecycle_label() {
    let dir = tempfile::tempdir().unwrap();
    let gh = dir.path().join("gh");
    let script = format!(
        r#"#!/bin/sh
d={d}
case "$*" in
  *"-X POST"*) echo "$*" >> "$d/writes.log"
    echo '[{{"name": "bug"}}, {{"name": "loom:triage"}}, {{"name": "loom:issue"}}]' ;;
  *"-X DELETE"*) echo "$*" >> "$d/writes.log" ;;
  *) exit 1 ;;
esac
"#,
        d = dir.path().display()
    );
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let repo = Repo {
        slug: "acme/widgets".into(),
        host: None,
        root: dir.path().to_path_buf(),
    };
    let mut forge = GhIntakeForge::new(gh);
    let after = forge.add_triage(&repo, 9).unwrap();
    assert!(has_other_lifecycle_label(&after), "{after:?}");
    assert!(forge.remove_triage(&repo, 9));
    let writes = std::fs::read_to_string(dir.path().join("writes.log")).unwrap();
    let lines: Vec<&str> = writes.lines().collect();
    assert_eq!(lines.len(), 2, "{writes}");
    assert!(lines[1].contains("-X DELETE repos/acme/widgets/issues/9/labels/loom:triage"));
}

/// The pass with a fake forge: a write answered with `loom:issue` is
/// reverted and counted; a write answered with `loom:triage` alone is not.
#[test]
fn a_write_answered_with_another_lifecycle_label_is_reverted_and_counted() {
    let before = crate::forge_call_stats::counters::get(TRIAGE_REVERTED);
    let mut forge = Fake {
        listing: vec![row(1, &[]), row(2, &[])],
        ..Fake::default()
    };
    forge.current.insert(1, labels(&[]));
    forge.current.insert(2, labels(&[]));
    forge
        .post_answers
        .insert(1, vec!["loom:triage".into(), "loom:issue".into()]);
    let report = run_repo(&mut forge, &repo("acme/a"), now(), 50);
    assert_eq!(
        forge.calls,
        [
            "list acme/a",
            "scope",
            "reread 1",
            "post 1",
            "delete 1",
            "reread 2",
            "post 2"
        ]
    );
    assert_eq!((report.labelled, report.reverted), (1, 1));
    assert_eq!(crate::forge_call_stats::counters::get(TRIAGE_REVERTED), before + 1);

    // A failed removal stops the repo's pass (likely rate limited).
    let mut stuck = Fake {
        listing: vec![row(1, &[]), row(2, &[])],
        remove_fails: true,
        ..Fake::default()
    };
    stuck.current.insert(1, labels(&[]));
    stuck.current.insert(2, labels(&[]));
    stuck.post_answers.insert(1, vec!["loom:curated".into()]);
    let report = run_repo(&mut stuck, &repo("acme/a"), now(), 50);
    assert_eq!(stuck.calls, ["list acme/a", "scope", "reread 1", "post 1", "delete 1"]);
    assert_eq!((report.labelled, report.reverted), (0, 1));
}

// ===== readers out of budget =====

/// A shed listing is a skipped repo, counted, with no further call.
#[test]
fn a_shed_listing_skips_the_repo_and_is_counted() {
    let before = crate::forge_call_stats::counters::get(LISTING_SHED);
    let mut forge = Fake {
        listing: vec![row(1, &[])],
        listing_shed: true,
        ..Fake::default()
    };
    let pass = run_pass(&mut forge, &[repo("acme/a"), repo("acme/b")], now(), 50, &|| false);
    assert_eq!((pass.repos, pass.shed, pass.labelled), (0, 2, 0));
    assert_eq!(forge.calls, ["list acme/a", "list acme/b"]);
    assert_eq!(crate::forge_call_stats::counters::get(LISTING_SHED), before + 2);
}

/// A tick-deadline stop is a failed listing, but not a budget shed.
#[test]
fn a_tick_deadline_listing_is_failed_but_not_counted_as_shed() {
    let before = crate::forge_call_stats::counters::get(LISTING_SHED);
    let mut forge = Fake {
        listing: vec![row(1, &[])],
        listing_deadline: true,
        ..Fake::default()
    };
    let report = run_repo(&mut forge, &repo("acme/a"), now(), 50);
    assert!(report.listing_failed);
    assert!(!report.listing_shed);
    assert_eq!(forge.calls, ["list acme/a"]);
    assert_eq!(crate::forge_call_stats::counters::get(LISTING_SHED), before);
}

/// End to end with every reader withdrawn for budget: the captain's walk
/// makes no call at all (so none on the writer) and the repo is skipped.
#[test]
fn with_readers_out_of_budget_the_live_walk_makes_no_writer_call() {
    use crate::forge_identity::{ExhaustCause, RouteDecision};
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.log");
    let gh = dir.path().join("gh");
    std::fs::write(&gh, format!("#!/bin/sh\necho \"$*\" >> '{}'\nexit 1\n", log.display()))
        .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The root must resolve to its repo, as every root in the captain's set
    // does (an unresolved one has no reader pool and is never listed).
    let slug = format!("acme/shed-{}", std::process::id());
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&[
        "remote",
        "add",
        "origin",
        &format!("https://github.com/{slug}.git"),
    ]);
    let _route = store::install_test_route(|_| RouteDecision::Exhausted {
        until: std::time::SystemTime::now() + Duration::from_secs(600),
        cause: ExhaustCause::Budget,
    });
    let repo = Repo {
        slug,
        host: Some("github.com".into()),
        root: dir.path().to_path_buf(),
    };
    let mut forge = GhIntakeForge::new(gh);
    let before = crate::forge_call_stats::counters::get(LISTING_SHED);
    let pass = run_pass(&mut forge, std::slice::from_ref(&repo), now(), 50, &|| false);
    assert_eq!((pass.repos, pass.shed), (0, 1));
    assert!(!log.exists(), "no gh call at all: {:?}", std::fs::read_to_string(&log));
    assert_eq!(crate::forge_call_stats::counters::get(LISTING_SHED), before + 1);
}

// ===== coverage and wiring =====

#[test]
fn the_covered_set_is_logged_once_per_change() {
    let mut last = None;
    let ab = [repo("acme/a"), repo("acme/b")];
    assert_eq!(
        coverage_changed(&mut last, &ab),
        Some(vec!["acme/a".to_string(), "acme/b".to_string()])
    );
    assert_eq!(coverage_changed(&mut last, &ab), None, "unchanged: not again");
    assert_eq!(coverage_changed(&mut last, &ab[..1]), Some(vec!["acme/a".to_string()]));
    // Losing the captaincy clears it, so taking it back logs again.
    last = None;
    assert!(coverage_changed(&mut last, &ab[..1]).is_some());
}

/// The work finder's inline pass asks [`legacy_stands_down`] before its
/// cadence: under a resolved captain mode it never runs (and never records a
/// run), and under a legacy one it does. This test is the only writer of the
/// process-wide mode in the test binary; `maybe_run` itself is inert under
/// `cfg(test)`, so its body is covered through `inline_pass_due`.
#[test]
fn the_inline_pass_asks_the_resolved_mode_before_its_cadence() {
    let set = |mode: Option<Mode>| *MODE.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    let due_unasked = || -> bool { panic!("a stood-down host must not consult the cadence") };
    for mode in [
        Mode::Captain {
            captain: "cap".into(),
        },
        Mode::StandDown {
            captain: "cap".into(),
        },
    ] {
        set(Some(mode.clone()));
        assert!(
            !super::super::inline_pass_due(true, legacy_stands_down, due_unasked),
            "{mode:?}"
        );
    }
    for mode in [
        None,
        Some(Mode::Legacy {
            reason: "no_captain",
        }),
    ] {
        set(mode.clone());
        assert!(super::super::inline_pass_due(true, legacy_stands_down, || true), "{mode:?}");
        assert!(!super::super::inline_pass_due(true, legacy_stands_down, || false));
    }
    assert!(!super::super::inline_pass_due(false, || false, || true), "disabled");
    set(None);
}

// ===== demotion of issues opened past intake (#11820) =====

fn fresh(number: u32, labels: &[&str]) -> IntakeRow {
    IntakeRow {
        number,
        created_at: Some("2026-10-03T10:00:00Z".parse().unwrap()),
        labels: labels.iter().map(|s| (*s).to_string()).collect(),
        author: Some("alice".into()),
    }
}

fn labeled(label: &str, actor: &str, at: &str) -> LabelEvent {
    LabelEvent {
        label: label.into(),
        actor: Some(actor.into()),
        created_at: Some(at.parse().unwrap()),
    }
}

/// Through `run_repo`: an issue opened with `loom:issue` and one with
/// `loom:curated` are demoted; a later promotion is untouched; the ordinary
/// unlabelled issue is labelled as before, with no raced/reverted count.
#[test]
fn run_repo_demotes_issues_opened_past_intake_and_leaves_promotions() {
    let mut forge = Fake {
        listing: vec![
            fresh(1, &["loom:issue"]),
            fresh(2, &["loom:curated"]),
            fresh(3, &["loom:issue"]),
            row(4, &[]),
        ],
        ..Fake::default()
    };
    forge
        .events
        .insert(1, vec![labeled("loom:issue", "alice", "2026-10-03T10:00:00Z")]);
    forge
        .events
        .insert(2, vec![labeled("loom:curated", "alice", "2026-10-03T10:00:05Z")]);
    forge
        .events
        .insert(3, vec![labeled("loom:issue", "rjwalters", "2026-10-03T11:00:00Z")]);
    forge.current.insert(1, labels(&["loom:issue"]));
    forge.current.insert(2, labels(&["loom:curated"]));
    forge.current.insert(4, labels(&[]));
    let report = run_repo(&mut forge, &repo("acme/demote-a"), now(), 50);
    assert_eq!(
        (report.demoted, report.labelled, report.raced, report.reverted),
        (2, 1, 0, 0),
        "{report:?}"
    );
    assert_eq!(
        forge.calls,
        [
            "list acme/demote-a",
            "scope",
            "events 1",
            "reread 1",
            "post 1",
            "unlabel 1 loom:issue",
            "comment 1",
            "events 2",
            "reread 2",
            "post 2",
            "unlabel 2 loom:curated",
            "comment 2",
            "events 3",
            "reread 4",
            "post 4",
        ]
    );
    // The pass folds it in.
    let pass = run_pass(&mut Fake::default(), &[], now(), 50, &|| false);
    assert_eq!(pass.demoted, 0);
}

/// Events reads share the cap: with cap 2 and two candidates, the unlabelled
/// step gets nothing this pass (oldest candidates first).
#[test]
fn demotion_events_reads_count_against_the_cap() {
    let mut forge = Fake {
        listing: vec![
            fresh(1, &["loom:issue"]),
            fresh(2, &["loom:issue"]),
            fresh(3, &["loom:issue"]),
            row(4, &[]),
        ],
        ..Fake::default()
    };
    for n in [1, 2, 3] {
        forge
            .events
            .insert(n, vec![labeled("loom:issue", "bob", "2026-10-03T10:00:00Z")]);
    }
    let report = run_repo(&mut forge, &repo("acme/demote-cap"), now(), 2);
    assert_eq!((report.demoted, report.labelled), (0, 0));
    assert_eq!(forge.calls, ["list acme/demote-cap", "scope", "events 1", "events 2"]);
    // Both are remembered promotions; the next pass reads #3, then labels #4.
    forge.calls.clear();
    forge.current.insert(4, labels(&[]));
    let report = run_repo(&mut forge, &repo("acme/demote-cap"), now(), 2);
    assert_eq!(report.labelled, 1);
    assert_eq!(
        forge.calls,
        [
            "list acme/demote-cap",
            "scope",
            "events 3",
            "reread 4",
            "post 4"
        ]
    );
}

/// A failed events read stops the repo's pass with nothing written; a
/// write-scope refusal writes nothing either.
#[test]
fn demotion_fails_closed() {
    let mut shed = Fake {
        listing: vec![fresh(1, &["loom:issue"]), row(4, &[])],
        ..Fake::default()
    };
    let report = run_repo(&mut shed, &repo("acme/demote-shed"), now(), 50);
    assert_eq!((report.demoted, report.labelled), (0, 0));
    assert_eq!(shed.calls, ["list acme/demote-shed", "scope", "events 1"]);

    let mut denied = Fake {
        listing: vec![fresh(1, &["loom:issue"])],
        write_denied: true,
        ..Fake::default()
    };
    let report = run_repo(&mut denied, &repo("acme/demote-denied"), now(), 50);
    assert!(report.denied);
    assert_eq!(denied.calls, ["list acme/demote-denied", "scope"]);
}

/// The heartbeat carries the count and still reads a record written before
/// the field existed.
#[test]
fn the_heartbeat_counts_demotions_and_reads_old_records() {
    let pass = PassReport {
        repos: 1,
        demoted: 2,
        ..PassReport::default()
    };
    let hb = next_heartbeat(None, "cap", now(), &pass, 1);
    assert_eq!(hb.demoted, 2);
    let mut old = serde_json::to_value(&hb).unwrap();
    old.as_object_mut().unwrap().remove("demoted");
    let back: PassHeartbeat = serde_json::from_value(old).unwrap();
    assert_eq!(back.demoted, 0);
}
