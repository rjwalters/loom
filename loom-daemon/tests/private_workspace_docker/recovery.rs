//! Unattended private-session recovery against a real Docker engine (#12772).
//!
//! `private_workspace::recover` is the entry the session reconciler calls for a
//! configured private-clone account whose container is stopped or gone. The
//! fake-runner suite (`private_workspace/recover_tests.rs`) proves its logic;
//! this module proves the real-engine behaviour that logic relies on: unpublished
//! work and every registered repository survive, and every unsafe shape is
//! refused with no mutation. Credential-free, like the rest of the harness.
use super::*;
use crate::dispatch::Environment;
use loom_daemon::tokens_pool::private_workspace::{self, dispatch::Selection, JobKind};

fn container(name: &str) -> String {
    format!("loom-codex-session-{name}")
}

/// The container's immutable ID, or `None` when no container has that name.
fn container_id(name: &str) -> Option<String> {
    let out = Command::new("docker")
        .args(["inspect", "--format", "{{.Id}}", &container(name)])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
}

fn running(name: &str) -> bool {
    Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{.State.Running}}",
            &container(name),
        ])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

fn volume_exists(name: &str) -> bool {
    Command::new("docker")
        .args(["volume", "inspect", &format!("loom-codex-workspace-{name}")])
        .output()
        .unwrap()
        .status
        .success()
}

/// Recover through the production entry with no extra caller-side hold.
fn recover(
    root: &std::path::Path,
    name: &str,
    image: Option<&str>,
) -> anyhow::Result<private_workspace::Status> {
    private_workspace::recover(root, name, image, &|| Ok(()))
}

/// Register two repositories on `a` and leave unpublished, uncommitted work in
/// the primary clone and in each additional clone.
fn seed(f: &Fixture, a: &str) {
    f.start(a);
    let (one, two) = (publish(f, "other", "second"), publish(f, "zed", "second"));
    for url in [&one, &two] {
        f.cli(&[
            "session",
            "start",
            a,
            "--private-clone",
            url,
            "--image",
            &f.image,
            "--json",
        ]);
    }
    checked(
        f.job(a, "role", "echo unfinished > /workspace/repo/keep")
            .output()
            .unwrap(),
    );
    for (url, command) in [(&one, "echo mine > mine"), (&two, "echo yours > mine")] {
        checked(
            f.command(&[
                "session",
                "job",
                a,
                "--kind",
                "role",
                "--owner",
                "docker-proof",
                "--repository",
                url,
                "--",
                "sh",
                "-c",
                command,
            ])
            .output()
            .unwrap(),
        );
    }
}

/// Everything `seed` left behind is still there, byte for byte, and both
/// repositories are still registered.
#[track_caller]
fn assert_survived(f: &Fixture, a: &str) {
    assert_eq!(f.exec(a, "cat /workspace/repo/keep").trim(), "unfinished");
    assert_eq!(f.exec(a, "cat /workspace/work/other-second-*/mine").trim(), "mine");
    assert_eq!(f.exec(a, "cat /workspace/work/zed-second-*/mine").trim(), "yours");
    let status: Value = serde_json::from_str(&f.cli(&["session", "status", a, "--json"])).unwrap();
    assert_eq!(status["config"]["repositories"].as_array().unwrap().len(), 2, "{status}");
}

fn environment(f: &Fixture, root: &std::path::Path, name: &str) -> Environment {
    containment::environment(f, root, name, "unused.invalid")
}

#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn a_stopped_session_resumes_in_place_with_unpublished_work_and_every_repository() {
    let f = Fixture::new();
    let a = &f.names[0];
    seed(&f, a);
    let root = containment::registry_workspace(&f);
    let _env = environment(&f, &root, a);
    let before = container_id(a).unwrap();
    docker(&["stop", &container(a)]);
    assert!(!running(a));

    let status = recover(&root, a, Some(&f.image)).unwrap();

    assert!(status.running);
    assert!(running(a));
    // Resumed, not replaced: same container, same writable layer.
    assert_eq!(container_id(a).unwrap(), before);
    assert_eq!(status.container_id.as_deref(), Some(before.as_str()));
    assert_survived(&f, a);
}

#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn a_missing_container_is_recreated_on_the_surviving_volume_without_losing_work() {
    let f = Fixture::new();
    let a = &f.names[0];
    seed(&f, a);
    let root = containment::registry_workspace(&f);
    let _env = environment(&f, &root, a);
    let before = container_id(a).unwrap();
    docker(&["rm", "-f", &container(a)]);
    assert!(container_id(a).is_none());

    let status = recover(&root, a, Some(&f.image)).unwrap();

    assert!(status.running && running(a));
    assert_ne!(container_id(a).unwrap(), before, "a missing container is replaced");
    assert_survived(&f, a);
    // Job admission refuses any dirty clone by design (unpublished work must not
    // be reused over), and `seed` left uncommitted files in every clone. Survival
    // is already proven above, so clear them before checking admission.
    f.exec(a, "rm -f /workspace/repo/keep /workspace/work/*/mine");
    // The ordinary job path still admits the recovered account.
    checked(f.job(a, "role", "true").output().unwrap());
}

#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn a_missing_volume_is_never_replaced_by_an_empty_one() {
    let f = Fixture::new();
    let a = &f.names[0];
    f.start(a);
    let root = containment::registry_workspace(&f);
    let _env = environment(&f, &root, a);
    docker(&["rm", "-f", &container(a)]);
    docker(&["volume", "rm", &format!("loom-codex-workspace-{a}")]);

    let refused = recover(&root, a, Some(&f.image)).unwrap_err().to_string();

    assert!(refused.contains("volume") && refused.contains("missing"), "{refused}");
    assert!(!volume_exists(a), "recovery must not create an empty replacement volume");
    assert!(container_id(a).is_none(), "no container may be created without the volume");
}

#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn an_unavailable_or_unrecorded_image_is_never_substituted() {
    let f = Fixture::new();
    let a = &f.names[0];
    seed(&f, a);
    let root = containment::registry_workspace(&f);
    let _env = environment(&f, &root, a);
    docker(&["rm", "-f", &container(a)]);

    let absent = format!("loom-private-test:absent-{}", uuid::Uuid::new_v4().simple());
    let missing = recover(&root, a, Some(&absent)).unwrap_err().to_string();
    assert!(missing.contains("not available locally"), "{missing}");
    let unknown = recover(&root, a, None).unwrap_err().to_string();
    assert!(unknown.contains("refusing to substitute"), "{unknown}");
    assert!(container_id(a).is_none());

    // The refusals changed nothing: a later recovery with the real image works.
    recover(&root, a, Some(&f.image)).unwrap();
    assert_survived(&f, a);
}

#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn a_foreign_container_under_the_session_name_is_refused_and_left_alone() {
    let f = Fixture::new();
    let a = &f.names[0];
    f.start(a);
    let root = containment::registry_workspace(&f);
    let _env = environment(&f, &root, a);
    docker(&["rm", "-f", &container(a)]);
    // Same name, none of the account's labels, mounts or settings.
    docker(&[
        "run",
        "-d",
        "--name",
        &container(a),
        "--entrypoint",
        "/bin/sleep",
        &f.image,
        "infinity",
    ]);
    let foreign = container_id(a).unwrap();

    let refused = recover(&root, a, Some(&f.image));

    assert!(refused.is_err());
    assert_eq!(container_id(a).as_deref(), Some(foreign.as_str()), "foreign container touched");
    assert!(running(a));
}

#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn failed_control_verification_does_not_report_a_recovered_session() {
    let f = Fixture::new();
    let a = &f.names[0];
    f.start(a);
    let root = containment::registry_workspace(&f);
    let _env = environment(&f, &root, a);
    // An image whose image-owned control bundle is gone: it creates and runs, but
    // the loom-private-control-v1 boundary cannot be observed.
    let bare = f.derive("no-control", "RUN rm -rf /opt/loom/private-control");
    docker(&["rm", "-f", &container(a)]);

    let refused = recover(&root, a, Some(&bare));

    let error = refused.expect_err("an unverifiable control boundary must not count as recovered");
    assert!(!error.to_string().trim().is_empty());
}

#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn an_operator_stop_holds_the_session_down_and_a_racing_recover_never_leaves_it_running_and_held() {
    let f = Fixture::new();
    let a = &f.names[0];
    f.start(a);
    let root = containment::registry_workspace(&f);
    let _env = environment(&f, &root, a);
    let profile = f.root.path().join("profiles").join(a);

    // Deliberate stop, then recover: the hold wins and nothing is recreated.
    f.cli(&["session", "stop", a, "--json"]);
    assert!(loom_daemon::tokens_pool::session_hold::held_across(std::slice::from_ref(
        &profile
    )));
    let refused = recover(&root, a, Some(&f.image)).unwrap_err().to_string();
    assert!(refused.contains("held"), "{refused}");
    assert!(container_id(a).is_none());
    assert!(volume_exists(a));

    // An operator start lifts the hold; then stop and recover race. Whichever
    // order the lease serializes them in, the account must never end up with a
    // running container AND a hold.
    f.start(a);
    for _ in 0..3 {
        docker(&["stop", &container(a)]);
        let stopper = f
            .command(&["session", "stop", a, "--json"])
            .spawn()
            .unwrap();
        let _ = recover(&root, a, Some(&f.image));
        let _ = stopper.wait_with_output().unwrap();
        let held =
            loom_daemon::tokens_pool::session_hold::held_across(std::slice::from_ref(&profile));
        assert!(!(held && running(a)), "running while held");
        f.start(a);
    }
}

#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn recovery_never_runs_over_an_active_job_lease() {
    let f = Fixture::new();
    let a = &f.names[0];
    f.start(a);
    let root = containment::registry_workspace(&f);
    let _env = environment(&f, &root, a);
    let before = container_id(a).unwrap();
    let mut role = f
        .job(a, "role", "echo ready; sleep 5")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = role.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut line = String::new();
        let _ = std::io::BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(30))
            .expect("job did not start")
            .trim(),
        "ready"
    );

    let refused = recover(&root, a, Some(&f.image));

    assert!(refused.is_err(), "recovery must not run beside a live job");
    assert_eq!(container_id(a).as_deref(), Some(before.as_str()));
    assert!(running(a));
    assert!(role.wait().unwrap().success(), "the job was disturbed");
}

#[test]
#[serial_test::serial]
#[ignore = "requires Docker; explicitly run by CI"]
fn a_proof_for_the_pre_recreate_container_is_refused_and_the_next_job_gets_a_fresh_one() {
    let mut f = Fixture::with_adapters(true);
    f.repository = f.repository.replace("/gitea/", "/github/");
    let a = &f.names[0];
    let gh_host = reqwest::Url::parse(&f.repository)
        .unwrap()
        .host_str()
        .unwrap()
        .to_owned();
    let start = || {
        checked(
            f.command(&[
                "session",
                "start",
                a,
                "--private-clone",
                &f.repository,
                "--image",
                &f.image,
            ])
            .env("GH_HOST", &gh_host)
            .env("GH_TOKEN", auth::GH_TOKEN)
            .output()
            .unwrap(),
        );
    };
    // The operator's one-time hook-trust decision (see containment.rs).
    start();
    f.cli(&["session", "stop", a, "--json"]);
    containment::establish_hook_trust(&f.root.path().join("profiles").join(a));
    start();
    let root = containment::registry_workspace(&f);
    let _env = containment::environment(&f, &root, a, &gh_host);
    let prepare = |owner: &str| {
        Selection::prepare(&root, "codex", None, JobKind::Sweep, Some(8787), owner)
            .unwrap()
            .unwrap()
    };

    let first = prepare("recovery-before");
    let old = first.contain(&root).unwrap();
    let old_id = container_id(a).unwrap();
    assert!(old_id.starts_with(&old.provenance(Vec::new(), Default::default()).container));
    // The container is lost under the holder: its proof no longer verifies.
    docker(&["rm", "-f", &container(a)]);
    assert!(first.contain(&root).is_err(), "a proof outlived its container");
    drop(first);

    recover(&root, a, Some(&f.image)).unwrap();
    let new_id = container_id(a).unwrap();
    assert_ne!(new_id, old_id);

    // The next job proves containment afresh, for the container that exists now.
    let second = prepare("recovery-after");
    let fresh = second.contain(&root).unwrap();
    let provenance = fresh.provenance(Vec::new(), Default::default());
    assert!(new_id.starts_with(&provenance.container), "{provenance:?}");
    assert!(!old_id.starts_with(&provenance.container));
    assert_eq!(provenance.account, a.as_str());
}
