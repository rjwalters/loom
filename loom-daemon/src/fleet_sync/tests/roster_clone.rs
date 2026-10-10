//! Roster autoApply clones (#11218). No test here runs git or touches the
//! network: every clone goes through [`Fake`], a [`Cloner`] that writes (or
//! fails to write) a `.git` directory into the destination it is handed.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;
use crate::fleet_store::roster::{Change, Plan};

/// What [`Fake`] does with each clone.
#[derive(Clone, Copy)]
enum Behaviour {
    /// Write `dest/.git` and succeed.
    Clone,
    /// Write a partial `dest` (no `.git`), then fail.
    FailPartial,
    /// Write a `dest` with no `.git` and claim success.
    NoGit,
}

struct Fake {
    behaviour: Behaviour,
    calls: RefCell<Vec<(String, String, PathBuf)>>,
}

impl Fake {
    fn new(behaviour: Behaviour) -> Self {
        Self {
            behaviour,
            calls: RefCell::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<(String, String, PathBuf)> {
        self.calls.borrow().clone()
    }
}

impl Cloner for Fake {
    fn clone_into(&self, slug: &str, url: &str, dest: &Path, _: Duration) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push((slug.to_string(), url.to_string(), dest.to_path_buf()));
        assert!(!dest.exists(), "the destination must not exist when a clone starts");
        match self.behaviour {
            Behaviour::Clone => {
                std::fs::create_dir_all(dest.join(".git")).unwrap();
                std::fs::write(dest.join("README"), "hi").unwrap();
                Ok(())
            }
            Behaviour::FailPartial => {
                std::fs::create_dir_all(dest.join("half")).unwrap();
                Err("git clone failed (exit status: 128): fatal: repository not found".into())
            }
            Behaviour::NoGit => {
                std::fs::create_dir_all(dest).unwrap();
                Ok(())
            }
        }
    }
}

fn missing(root: &Path, name: &str, remote: Option<&str>) -> Change {
    Change::MissingClone {
        name: name.to_string(),
        path: root.join(name),
        remote: remote.map(str::to_string),
        priority: 73,
        maintain_only: false,
    }
}

fn gh(name: &str) -> String {
    format!("git@github.com:acme/{name}.git")
}

fn plan(changes: Vec<Change>) -> Plan {
    Plan {
        changes,
        ..Plan::default()
    }
}

fn config(max: usize) -> CloneConfig {
    CloneConfig {
        max_per_pass: max,
        timeout: Duration::from_secs(60),
    }
}

/// Run [`summarize`], collecting every change handed to the apply seam.
fn run(plan: &Plan, auto: bool, clones: &Clones<'_>) -> (RosterPass, Vec<Change>) {
    let mut applied = Vec::new();
    let pass = summarize(plan, auto, clones, &mut |c| {
        applied.push(c.clone());
        Ok(())
    });
    (pass, applied)
}

#[test]
fn auto_apply_off_clones_nothing_and_reports_as_before() {
    let root = tempfile::tempdir().unwrap();
    let fake = Fake::new(Behaviour::Clone);
    let p = plan(vec![missing(root.path(), "wiki", Some(&gh("wiki")))]);
    let (pass, applied) = run(&p, false, &Clones::on(&fake, config(2)));
    assert!(fake.calls().is_empty(), "autoApply off must never clone");
    assert!(applied.is_empty());
    assert_eq!(pass.drift, vec![roster::describe(&p.changes[0])]);
    assert_eq!((pass.applied, pass.unapplied), (0, 0));
    assert!(pass.clones.is_empty());
    assert!(!root.path().join("wiki").exists());
    // And the snapshot carries no new key.
    let json = serde_json::to_value(&pass).unwrap();
    assert!(json.get("clones").is_none(), "{json}");
}

#[test]
fn clones_off_counts_a_missing_clone_unapplied_exactly_as_before() {
    let root = tempfile::tempdir().unwrap();
    let p = plan(vec![missing(root.path(), "wiki", Some(&gh("wiki")))]);
    let (pass, applied) = run(&p, true, &Clones::off());
    assert!(applied.is_empty());
    assert_eq!((pass.applied, pass.unapplied), (0, 1));
    assert!(pass.clones.is_empty());
    assert_eq!(pass.drift, vec![roster::describe(&p.changes[0])]);
}

#[test]
fn a_missing_clone_is_cloned_over_https_then_registered_as_an_add() {
    let root = tempfile::tempdir().unwrap();
    let fake = Fake::new(Behaviour::Clone);
    let mut change = missing(root.path(), "wiki", Some(&gh("wiki")));
    if let Change::MissingClone { maintain_only, .. } = &mut change {
        *maintain_only = true;
    }
    let p = plan(vec![change]);
    let (pass, applied) = run(&p, true, &Clones::on(&fake, config(2)));

    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "acme/wiki");
    assert_eq!(calls[0].1, "https://github.com/acme/wiki.git", "never the git@ remote");
    assert_eq!(calls[0].2, root.path().join(".wiki.loom-clone"), "a temporary sibling");

    let dest = root.path().join("wiki");
    assert!(dest.join(".git").is_dir(), "renamed into place");
    assert!(!root.path().join(".wiki.loom-clone").exists());
    assert_eq!(
        applied,
        vec![Change::Add {
            name: "wiki".into(),
            path: dest.clone(),
            priority: 73,
            maintain_only: true,
        }]
    );
    assert_eq!((pass.applied, pass.unapplied), (1, 0));
    assert_eq!(pass.error, None);
    assert_eq!(pass.clones.len(), 1);
    let a = &pass.clones[0];
    assert_eq!(a.outcome, Outcome::Cloned);
    assert!(a.registered);
    assert_eq!(a.repo.as_deref(), Some("acme/wiki"));
    assert!(pass.drift[0].starts_with("+ add"), "{:?}", pass.drift);
    assert!(pass.drift[0].contains("cloned this pass"), "{:?}", pass.drift);
}

#[test]
fn a_failed_clone_is_unapplied_recorded_and_leaves_nothing_behind() {
    let root = tempfile::tempdir().unwrap();
    let fake = Fake::new(Behaviour::FailPartial);
    let p = plan(vec![missing(root.path(), "wiki", Some(&gh("wiki")))]);
    let (pass, applied) = run(&p, true, &Clones::on(&fake, config(2)));
    assert_eq!(fake.calls().len(), 1);
    assert!(applied.is_empty(), "no registry change after a failed clone");
    assert_eq!((pass.applied, pass.unapplied), (0, 1));
    assert_eq!(pass.error, None, "a clone failure is not a fail-closed roster error");
    assert_eq!(pass.clones[0].outcome, Outcome::Failed);
    assert!(pass.clones[0]
        .detail
        .as_deref()
        .unwrap()
        .contains("repository not found"));
    assert!(!root.path().join("wiki").exists());
    assert!(!root.path().join(".wiki.loom-clone").exists(), "the partial clone is removed");
    // The drift line is still the missing one: next pass retries.
    assert_eq!(pass.drift, vec![roster::describe(&p.changes[0])]);
}

#[test]
fn a_clone_without_a_git_dir_is_a_failure_never_moved_into_place() {
    let root = tempfile::tempdir().unwrap();
    let fake = Fake::new(Behaviour::NoGit);
    let p = plan(vec![missing(root.path(), "wiki", Some(&gh("wiki")))]);
    let (pass, applied) = run(&p, true, &Clones::on(&fake, config(2)));
    assert!(applied.is_empty());
    assert_eq!(pass.clones[0].outcome, Outcome::Failed);
    assert!(!root.path().join("wiki").exists());
    assert!(!root.path().join(".wiki.loom-clone").exists());
}

#[test]
fn an_occupied_path_is_refused_and_never_touched() {
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("wiki");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("notes.txt"), "mine").unwrap();
    std::fs::write(root.path().join("file"), "x").unwrap();
    let fake = Fake::new(Behaviour::Clone);
    let p = plan(vec![
        missing(root.path(), "wiki", Some(&gh("wiki"))),
        missing(root.path(), "file", Some(&gh("file"))),
    ]);
    let (pass, applied) = run(&p, true, &Clones::on(&fake, config(2)));
    assert!(fake.calls().is_empty(), "an occupied path is never cloned over");
    assert!(applied.is_empty());
    assert_eq!(pass.unapplied, 2);
    assert!(pass.clones.iter().all(|a| a.outcome == Outcome::Refused));
    assert!(pass.clones[0]
        .detail
        .as_deref()
        .unwrap()
        .contains("left untouched"));
    assert_eq!(std::fs::read_to_string(dest.join("notes.txt")).unwrap(), "mine");
    assert!(!dest.join(".git").exists());
}

#[test]
fn an_empty_directory_is_cloned_into() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("wiki")).unwrap();
    let fake = Fake::new(Behaviour::Clone);
    let p = plan(vec![missing(root.path(), "wiki", Some(&gh("wiki")))]);
    let (pass, _) = run(&p, true, &Clones::on(&fake, config(2)));
    assert_eq!(pass.clones[0].outcome, Outcome::Cloned);
    assert!(root.path().join("wiki/.git").is_dir());
}

#[test]
fn a_stale_temporary_clone_is_replaced() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".wiki.loom-clone/junk")).unwrap();
    let fake = Fake::new(Behaviour::Clone);
    let p = plan(vec![missing(root.path(), "wiki", Some(&gh("wiki")))]);
    let (pass, _) = run(&p, true, &Clones::on(&fake, config(2)));
    assert_eq!(pass.clones[0].outcome, Outcome::Cloned);
    assert!(!root.path().join("wiki/junk").exists());
}

#[test]
fn the_root_is_created_on_a_fresh_host() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("GitHub");
    let fake = Fake::new(Behaviour::Clone);
    let p = plan(vec![missing(&root, "wiki", Some(&gh("wiki")))]);
    let (pass, _) = run(&p, true, &Clones::on(&fake, config(2)));
    assert_eq!(pass.clones[0].outcome, Outcome::Cloned);
    assert!(root.join("wiki/.git").is_dir());
}

#[test]
fn a_record_without_a_github_remote_is_refused_with_a_reason() {
    let root = tempfile::tempdir().unwrap();
    let fake = Fake::new(Behaviour::Clone);
    let p = plan(vec![
        missing(root.path(), "none", None),
        missing(root.path(), "other", Some("git@example.com:acme/other.git")),
    ]);
    let (pass, applied) = run(&p, true, &Clones::on(&fake, config(2)));
    assert!(fake.calls().is_empty());
    assert!(applied.is_empty());
    assert_eq!(pass.unapplied, 2);
    let why: Vec<_> = pass
        .clones
        .iter()
        .map(|a| a.detail.clone().unwrap())
        .collect();
    assert!(why[0].contains("no `remote`"), "{why:?}");
    assert!(why[1].contains("not a github.com repo"), "{why:?}");
}

#[test]
fn the_per_pass_cap_defers_the_rest_and_refusals_do_not_count() {
    let root = tempfile::tempdir().unwrap();
    let fake = Fake::new(Behaviour::Clone);
    let p = plan(vec![
        missing(root.path(), "norem", None),
        missing(root.path(), "a", Some(&gh("a"))),
        missing(root.path(), "b", Some(&gh("b"))),
        missing(root.path(), "c", Some(&gh("c"))),
    ]);
    let (pass, applied) = run(&p, true, &Clones::on(&fake, config(2)));
    assert_eq!(fake.calls().len(), 2, "at most two clones per pass");
    assert_eq!(applied.len(), 2);
    let outcomes: Vec<_> = pass.clones.iter().map(|a| a.outcome).collect();
    assert_eq!(
        outcomes,
        vec![
            Outcome::Refused,
            Outcome::Cloned,
            Outcome::Cloned,
            Outcome::Deferred
        ]
    );
    assert!(!root.path().join("c").exists());
    assert_eq!((pass.applied, pass.unapplied), (2, 2));
}

#[test]
fn a_failed_clone_counts_toward_the_cap() {
    let root = tempfile::tempdir().unwrap();
    let fake = Fake::new(Behaviour::FailPartial);
    let p = plan(vec![
        missing(root.path(), "a", Some(&gh("a"))),
        missing(root.path(), "b", Some(&gh("b"))),
    ]);
    let (pass, _) = run(&p, true, &Clones::on(&fake, config(1)));
    assert_eq!(fake.calls().len(), 1);
    assert_eq!(pass.clones[1].outcome, Outcome::Deferred);
}

#[test]
fn a_zero_cap_means_never_clone() {
    let root = tempfile::tempdir().unwrap();
    let fake = Fake::new(Behaviour::Clone);
    let p = plan(vec![missing(root.path(), "wiki", Some(&gh("wiki")))]);
    let (pass, applied) = run(&p, true, &Clones::on(&fake, config(0)));
    assert!(fake.calls().is_empty());
    assert!(applied.is_empty());
    assert!(pass.clones.is_empty(), "off, not deferred");
    assert_eq!(pass.unapplied, 1);
}

#[test]
fn a_registration_failure_after_a_clone_is_a_roster_error() {
    let root = tempfile::tempdir().unwrap();
    let fake = Fake::new(Behaviour::Clone);
    let p = plan(vec![missing(root.path(), "wiki", Some(&gh("wiki")))]);
    let pass = summarize(&p, true, &Clones::on(&fake, config(2)), &mut |_| {
        Err(anyhow::anyhow!("registry is read-only"))
    });
    assert_eq!((pass.applied, pass.unapplied), (0, 1));
    assert!(pass
        .error
        .as_deref()
        .unwrap()
        .contains("registry is read-only"));
    assert_eq!(pass.clones[0].outcome, Outcome::Cloned);
    assert!(!pass.clones[0].registered);
    // The clone stays: the next plan sees it cloned and registers it.
    assert!(root.path().join("wiki/.git").is_dir());
}

#[test]
fn other_changes_are_applied_as_before_around_a_clone() {
    let root = tempfile::tempdir().unwrap();
    let fake = Fake::new(Behaviour::Clone);
    let remove = Change::Remove {
        name: "old".into(),
        path: root.path().join("old"),
        reason: "fleet: false".into(),
    };
    let p = plan(vec![
        remove.clone(),
        missing(root.path(), "wiki", Some(&gh("wiki"))),
    ]);
    let (pass, applied) = run(&p, true, &Clones::on(&fake, config(2)));
    assert_eq!(applied[0], remove);
    assert!(matches!(applied[1], Change::Add { .. }));
    assert_eq!(pass.applied, 2);
}

#[test]
fn github_remotes_parse_and_everything_else_does_not() {
    for ok in [
        "git@github.com:acme/wiki.git",
        "git@github.com:acme/wiki",
        "ssh://git@github.com/acme/wiki.git",
        "https://github.com/acme/wiki.git",
        "https://github.com/acme/wiki",
        "https://github.com/acme/wiki/",
        " https://github.com/acme/wiki.git ",
    ] {
        assert_eq!(github_slug(ok).as_deref(), Some("acme/wiki"), "{ok}");
    }
    assert_eq!(
        github_slug("git@github.com:Acme-1/w.i_k-i.git").as_deref(),
        Some("Acme-1/w.i_k-i")
    );
    for bad in [
        "",
        "garbage",
        "git@example.com:acme/wiki.git",
        "https://gitlab.com/acme/wiki.git",
        "https://github.com/acme",
        "https://github.com/acme/wiki/tree/main",
        "https://github.com/acme/../etc",
        "https://github.com/acme/-upload-pack",
        "https://github.com/acme/wi ki",
        "http://github.com/acme/wiki.git",
        "file:///srv/acme/wiki.git",
    ] {
        assert_eq!(github_slug(bad), None, "{bad}");
    }
    assert_eq!(https_url("acme/wiki"), "https://github.com/acme/wiki.git");
}

#[test]
fn config_defaults_overrides_and_clamps() {
    let none = |_: &str| None;
    let empty = serde_json::json!({});
    assert_eq!(resolve_config(&empty, &none), CloneConfig::default());
    assert_eq!(CloneConfig::default().max_per_pass, 2);
    assert_eq!(CloneConfig::default().timeout, Duration::from_secs(300));

    let cfg = serde_json::json!({"fleet": {"cloneMaxPerPass": 5, "cloneTimeoutSecs": "120"}});
    assert_eq!(
        resolve_config(&cfg, &none),
        CloneConfig {
            max_per_pass: 5,
            timeout: Duration::from_secs(120)
        }
    );
    let env = |k: &str| match k {
        MAX_PER_PASS_ENV => Some("0".to_string()),
        TIMEOUT_ENV => Some("1".to_string()),
        _ => None,
    };
    assert_eq!(
        resolve_config(&cfg, &env),
        CloneConfig {
            max_per_pass: 0,
            timeout: Duration::from_secs(MIN_TIMEOUT_SECS)
        },
        "env wins, and the timeout is clamped up"
    );
}

#[test]
fn status_lines_and_the_bus_payload() {
    let a = CloneAttempt {
        name: "wiki".into(),
        repo: Some("acme/wiki".into()),
        path: PathBuf::from("/home/someone/GitHub/wiki"),
        outcome: Outcome::Failed,
        duration_ms: 1500,
        registered: false,
        detail: Some("could not create /home/someone/GitHub/.wiki.loom-clone".into()),
    };
    let lines = lines(std::slice::from_ref(&a));
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("clone FAILED"), "{lines:?}");
    assert!(lines[0].contains("retried next pass"), "{lines:?}");

    let v = payload(&a, "host-1");
    assert_eq!(v["repo"], "acme/wiki");
    assert_eq!(v["dir"], "wiki");
    assert_eq!(v["outcome"], "failed");
    assert_eq!(v["durationMs"], 1500);
    assert_eq!(v["host"], "host-1");
    let text = v.to_string();
    assert!(!text.contains("/home/someone"), "no absolute path on the wire: {text}");
}

#[test]
fn a_snapshot_with_clones_round_trips() {
    let pass = RosterPass {
        clones: vec![CloneAttempt {
            name: "wiki".into(),
            repo: Some("acme/wiki".into()),
            path: PathBuf::from("/srv/wiki"),
            outcome: Outcome::Cloned,
            duration_ms: 10,
            registered: true,
            detail: None,
        }],
        ..RosterPass::default()
    };
    let json = serde_json::to_string(&pass).unwrap();
    assert!(json.contains("\"outcome\":\"cloned\""), "{json}");
    let back: RosterPass = serde_json::from_str(&json).unwrap();
    assert_eq!(back, pass);
}
