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
    /// Issue number → what the re-read answers (absent = read failure).
    current: std::collections::BTreeMap<u32, Option<Vec<String>>>,
    write_denied: bool,
    post_fails: bool,
    calls: Vec<String>,
}

impl IntakeForge for Fake {
    fn list_open(&mut self, repo: &Repo) -> Result<Vec<IntakeRow>> {
        self.calls.push(format!("list {}", repo.slug));
        if self.listing_fails {
            return Err(anyhow!("listing changed mid-walk"));
        }
        Ok(self.listing.clone())
    }

    fn current_labels(&mut self, _: &Repo, number: u32) -> Result<Option<Vec<String>>> {
        self.calls.push(format!("reread {number}"));
        self.current
            .get(&number)
            .cloned()
            .ok_or_else(|| anyhow!("HTTP 502"))
    }

    fn may_write(&mut self, _: &Repo) -> bool {
        self.calls.push("scope".into());
        !self.write_denied
    }

    fn add_triage(&mut self, _: &Repo, number: u32) -> bool {
        self.calls.push(format!("post {number}"));
        !self.post_fails
    }
}

fn row(number: u32, labels: &[&str]) -> IntakeRow {
    IntakeRow {
        number,
        created_at: Some("2026-09-01T00:00:00Z".parse().unwrap()),
        labels: labels.iter().map(|s| (*s).to_string()).collect(),
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
    fn current_labels(&mut self, repo: &Repo, number: u32) -> Result<Option<Vec<String>>> {
        self.0.current_labels(repo, number)
    }
    fn may_write(&mut self, _: &Repo) -> bool {
        true
    }
    fn add_triage(&mut self, repo: &Repo, number: u32) -> bool {
        self.0.add_triage(repo, number)
    }
}

/// End to end through the real read and write paths: the listing drops the
/// pull request, #4 was labelled after the listing and is left alone, only
/// #1 gets the label, and the next listing of the unchanged repo is one `304`.
#[test]
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
  *"-X POST"*) echo "$*" >> "$d/posts.log" ;;
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
}
