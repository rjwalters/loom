//! The Docker Hub mirror helper (#11232), without Docker: reference mapping,
//! the pin list (#11755), and the pull's deadline, GHCR-first order, fallback
//! and terminal report against a fake `docker` that stalls or fails on demand.

#[path = "common/hub_image.rs"]
mod hub_image;

const DIGEST: &str = "sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

#[test]
fn digest_pinned_reference_keeps_its_digest_and_drops_its_tag() {
    let image = format!("clickhouse/clickhouse-server:25.12.5@{DIGEST}");
    assert_eq!(
        hub_image::on_registry("mirror.gcr.io", &image),
        format!("mirror.gcr.io/clickhouse/clickhouse-server@{DIGEST}")
    );
}

#[test]
fn official_image_gains_the_library_namespace() {
    assert_eq!(
        hub_image::on_registry("mirror.gcr.io", "ubuntu:24.04"),
        "mirror.gcr.io/library/ubuntu:24.04"
    );
    assert_eq!(
        hub_image::on_registry("mirror.gcr.io", "ubuntu"),
        "mirror.gcr.io/library/ubuntu:latest"
    );
}

#[test]
fn only_hostless_references_are_docker_hub() {
    assert!(hub_image::is_docker_hub("ubuntu:24.04"));
    assert!(hub_image::is_docker_hub("otel/opentelemetry-collector-contrib:0.139.0"));
    assert!(!hub_image::is_docker_hub("ghcr.io/rjwalters/loom-worker-session:latest"));
    assert!(!hub_image::is_docker_hub("localhost:5000/ubuntu:24.04"));
    assert!(!hub_image::is_docker_hub("localhost/ubuntu"));
}

/// `on_unavailable` over a fixed environment, never the process's own.
fn policy(vars: &[(&str, &str)]) -> hub_image::OnUnavailable {
    hub_image::on_unavailable(|name| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_string())
    })
}

#[test]
fn a_pull_request_skips_an_unavailable_image() {
    use hub_image::OnUnavailable::Skip;
    for event in ["pull_request", "pull_request_target", "merge_group"] {
        let env = [
            ("GITHUB_ACTIONS", "true"),
            ("GITHUB_EVENT_NAME", event),
            ("GITHUB_REF", "refs/pull/11756/merge"),
        ];
        assert_eq!(policy(&env), Skip, "{event}");
    }
}

#[test]
fn main_and_everything_else_fail_closed() {
    use hub_image::OnUnavailable::Fail;
    let ci = |event: &'static str, git_ref: &'static str| {
        policy(&[
            ("GITHUB_ACTIONS", "true"),
            ("GITHUB_EVENT_NAME", event),
            ("GITHUB_REF", git_ref),
        ])
    };
    assert_eq!(ci("push", "refs/heads/main"), Fail);
    // A PR-shaped event that somehow carries main's ref is still main.
    assert_eq!(ci("pull_request", "refs/heads/main"), Fail);
    assert_eq!(ci("schedule", "refs/heads/main"), Fail);
    assert_eq!(ci("workflow_dispatch", "refs/heads/feature/x"), Fail);
    assert_eq!(ci("push", "refs/heads/feature/x"), Fail);
    // Outside Actions (a developer's own --ignored run) the event is not trusted.
    assert_eq!(policy(&[("GITHUB_EVENT_NAME", "pull_request")]), Fail);
    assert_eq!(policy(&[]), Fail);
}

#[test]
fn the_override_wins_in_both_directions() {
    use hub_image::OnUnavailable::{Fail, Skip};
    assert_eq!(policy(&[("LOOM_TEST_IMAGE_UNAVAILABLE", "skip")]), Skip);
    let pr = [
        ("GITHUB_ACTIONS", "true"),
        ("GITHUB_EVENT_NAME", "pull_request"),
        ("LOOM_TEST_IMAGE_UNAVAILABLE", "fail"),
    ];
    assert_eq!(policy(&pr), Fail);
}

#[test]
fn only_an_exhausted_pull_is_skipped_or_failed() {
    use hub_image::{outcome, OnUnavailable, Outcome};
    let pulled = || Ok::<_, Vec<String>>("mirror.test/library/ubuntu:24.04".to_string());
    let failed = || Err::<String, _>(vec!["toomanyrequests".to_string()]);
    for policy in [OnUnavailable::Skip, OnUnavailable::Fail] {
        assert_eq!(
            outcome(pulled(), policy),
            Outcome::Use("mirror.test/library/ubuntu:24.04".to_string())
        );
    }
    assert_eq!(
        outcome(failed(), OnUnavailable::Skip),
        Outcome::Skip(vec!["toomanyrequests".to_string()])
    );
    assert_eq!(
        outcome(failed(), OnUnavailable::Fail),
        Outcome::Fail(vec!["toomanyrequests".to_string()])
    );
}

#[test]
fn a_skip_is_a_greppable_warning_and_a_summary_line_per_test() {
    let dir = tempfile::tempdir().unwrap();
    let summary = dir.path().join("summary.md");
    std::fs::write(&summary, "").unwrap();
    let failures = vec!["mirror.gcr.io/library/ubuntu:24.04 (attempt 1): 503".to_string()];
    for test in ["first_test", "second_test"] {
        let annotation = hub_image::report_skip("ubuntu:24.04", test, &failures, Some(&summary));
        assert!(annotation.starts_with("::warning "), "{annotation}");
        assert!(annotation.contains("::image-unavailable ubuntu:24.04 "), "{annotation}");
        assert!(annotation.contains(test), "{annotation}");
    }
    let written = std::fs::read_to_string(&summary).unwrap();
    assert_eq!(written.matches("**SKIPPED**").count(), 2, "{written}");
    assert!(written.contains("`second_test`: image-unavailable `ubuntu:24.04`"));
}

#[test]
fn every_pin_is_a_digest_pinned_docker_hub_reference_listed_once() {
    let pins = hub_image::pins();
    assert!(!pins.is_empty());
    let mut seen = std::collections::BTreeSet::new();
    for pin in &pins {
        let (name, digest) = pin
            .split_once('@')
            .unwrap_or_else(|| panic!("no digest: {pin}"));
        let (_, tag) = name
            .rsplit_once(':')
            .unwrap_or_else(|| panic!("no tag: {pin}"));
        assert!(!tag.contains('/'), "no tag: {pin}");
        let hex = digest
            .strip_prefix("sha256:")
            .unwrap_or_else(|| panic!("not sha256: {pin}"));
        assert!(
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "malformed digest: {pin}"
        );
        assert!(hub_image::is_docker_hub(pin), "not a Docker Hub image: {pin}");
        assert!(seen.insert(*pin), "listed twice: {pin}");
    }
}

#[test]
fn the_pin_list_maps_tag_only_and_digest_requests_to_their_entry() {
    let ubuntu =
        "ubuntu:24.04@sha256:534baea6a22c03a63003dbc8dbe78fe34bc0d7e595d9a9dc9834884ff530eb55";
    assert_eq!(hub_image::pinned("ubuntu:24.04").as_deref(), Some(ubuntu));
    assert_eq!(hub_image::pinned(ubuntu).as_deref(), Some(ubuntu));
    let clickhouse = format!("clickhouse/clickhouse-server:25.12.5@{DIGEST}");
    assert_eq!(hub_image::pinned(&clickhouse), Some(clickhouse.clone()));
    // A different digest under a listed tag is not the pin.
    let other = format!("clickhouse/clickhouse-server:25.12.5@sha256:{}", "0".repeat(64));
    assert_eq!(hub_image::pinned(&other), None);
    assert_eq!(hub_image::pinned("ubuntu:22.04"), None);
    assert_eq!(hub_image::pinned("ubuntu"), None);
}

/// Every Docker Hub image a test or `ci.yml` hands to `docker` must be on the
/// pin list, or the copy workflow never copies it and CI pulls it from a
/// third party (#11755). The sources: `*IMAGE: &str = "<ref>"` constants and
/// Dockerfile `FROM` lines in the integration tests, and `LOOM_TEST_*_IMAGE`
/// values in `ci.yml`.
#[test]
fn every_docker_hub_image_ci_pulls_is_on_the_pin_list() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let workflow = tests.join("../../.github/workflows/ci.yml");
    let is_image = |candidate: &str| {
        let (name, _) = candidate.split_once('@').unwrap_or((candidate, ""));
        name.rsplit('/')
            .next()
            .is_some_and(|last| last.contains(':'))
            && candidate
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".:/_-@".contains(&b))
            && hub_image::is_docker_hub(candidate)
    };
    let mut used = Vec::new();
    let mut sources: Vec<_> = std::fs::read_dir(&tests)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    sources.push(workflow);
    for path in sources {
        let text = std::fs::read_to_string(&path).unwrap();
        for line in text.lines() {
            let line = line.trim();
            let candidate = if let Some((_, rest)) = line.split_once("IMAGE: &str = \"") {
                rest.split('"').next()
            } else if let Some((_, rest)) = line.split_once("\"FROM ") {
                rest.split_whitespace().next()
            } else if line.starts_with("LOOM_TEST_") && line.contains("_IMAGE:") {
                line.split_once(':')
                    .map(|(_, value)| value.trim().trim_matches('"'))
            } else {
                None
            };
            if let Some(image) = candidate.filter(|image| is_image(image)) {
                used.push((
                    path.file_name().unwrap().to_string_lossy().to_string(),
                    image.to_string(),
                ));
            }
        }
    }
    // The scan must keep seeing what it guards; a refactor that hides these
    // from it would otherwise pass vacuously.
    for expected in [
        "clickhouse/clickhouse-server",
        "otel/opentelemetry-collector-contrib",
        "ubuntu",
    ] {
        assert!(
            used.iter()
                .any(|(_, image)| image.starts_with(&format!("{expected}:"))),
            "scan no longer finds {expected}: {used:?}"
        );
    }
    let missing: Vec<_> = used
        .iter()
        .filter(|(_, image)| hub_image::pinned(image).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "add these to .github/ci-images.txt (by digest) so ci-images.yml copies them: {missing:?}"
    );
}

#[cfg(unix)]
mod fake_docker {
    use super::{hub_image, DIGEST};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    /// A `docker` that has no local images and serves any pull, except from:
    /// - `stall.test`: hangs;
    /// - `down.test`: refuses;
    /// - `loud.test`: succeeds after writing more stderr than a pipe holds;
    /// - `linger.test`: refuses, leaving a background child holding stderr.
    ///
    /// Every invocation is appended to `calls` beside it.
    const SCRIPT: &str = r#"#!/bin/sh
echo "$*" >> "$(dirname "$0")/calls"
case "$1 $*" in
  image*) exit 1 ;;
  pull*stall.test/*) exec sleep 30 ;;
  pull*down.test/*) echo "toomanyrequests: fake rate limit" >&2; exit 1 ;;
  pull*loud.test/*) head -c 1048576 /dev/zero >&2 ;;
  pull*linger.test/*) sleep 20 & echo "denied: fake refusal" >&2; exit 1 ;;
esac
exit 0
"#;

    /// One fake `docker` for the whole binary, written before any test spawns
    /// it: a script written while a sibling thread forks is briefly "text file
    /// busy" to exec. Each test gets its own hard link, so `calls` stays per
    /// test.
    fn puller(dir: &Path, mirror: &str, hub: &str) -> hub_image::Puller {
        ghcr_puller(dir, "", mirror, hub)
    }

    fn ghcr_puller(dir: &Path, ghcr: &str, mirror: &str, hub: &str) -> hub_image::Puller {
        static SHARED: OnceLock<tempfile::TempDir> = OnceLock::new();
        let shared = SHARED.get_or_init(|| {
            let shared = tempfile::tempdir().unwrap();
            let docker = shared.path().join("docker");
            std::fs::write(&docker, SCRIPT).unwrap();
            std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
            shared
        });
        let docker = dir.join("docker");
        std::fs::hard_link(shared.path().join("docker"), &docker).unwrap();
        hub_image::Puller {
            docker,
            ghcr: ghcr.to_string(),
            mirror: mirror.to_string(),
            hub: hub.to_string(),
            pull_deadline: Duration::from_millis(300),
            hub_backoff: vec![Duration::from_millis(10); 3],
        }
    }

    fn calls(dir: &Path, prefix: &str) -> usize {
        std::fs::read_to_string(dir.join("calls"))
            .unwrap()
            .lines()
            .filter(|call| call.starts_with(prefix))
            .count()
    }

    #[test]
    fn stalled_mirror_pull_is_killed_at_its_deadline_and_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let image = format!("clickhouse/clickhouse-server:25.12.5@{DIGEST}");
        let started = Instant::now();
        let reference = puller(dir.path(), "stall.test", "up.test").pull(&image);
        // Two stalled attempts at 300ms each, not two 30s sleeps.
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        assert_eq!(reference, Ok(format!("up.test/clickhouse/clickhouse-server@{DIGEST}")));
        assert_eq!(calls(dir.path(), "pull --quiet stall.test/"), 2);
        assert_eq!(calls(dir.path(), "pull --quiet up.test/"), 1);
    }

    #[test]
    fn stderr_larger_than_a_pipe_is_not_mistaken_for_a_stall() {
        let dir = tempfile::tempdir().unwrap();
        let image = format!("clickhouse/clickhouse-server:25.12.5@{DIGEST}");
        let reference = puller(dir.path(), "loud.test", "down.test").pull(&image);
        assert_eq!(reference, Ok(format!("loud.test/clickhouse/clickhouse-server@{DIGEST}")));
        assert_eq!(calls(dir.path(), "pull --quiet loud.test/"), 1);
    }

    #[test]
    fn failed_pull_does_not_wait_on_a_descendant_holding_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let failures = puller(dir.path(), "linger.test", "down.test")
            .pull("ubuntu:24.04")
            .unwrap_err();
        // Two refused attempts, not two 20s waits on the lingering `sleep`.
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        assert!(
            failures[..2]
                .iter()
                .all(|f| f.ends_with("denied: fake refusal")),
            "{failures:?}"
        );
    }

    #[test]
    fn tag_only_image_is_tagged_back_to_its_docker_hub_name() {
        let dir = tempfile::tempdir().unwrap();
        // Not on the pin list, so pulled by tag.
        let reference = puller(dir.path(), "up.test", "down.test").pull("busybox:1.36");
        assert_eq!(reference, Ok("busybox:1.36".to_string()));
        assert_eq!(calls(dir.path(), "tag up.test/library/busybox:1.36 busybox:1.36"), 1);
        assert_eq!(calls(dir.path(), "pull --quiet down.test/"), 0);
    }

    #[test]
    fn exhausted_pull_reports_every_attempt_as_infrastructure() {
        let dir = tempfile::tempdir().unwrap();
        let failures = puller(dir.path(), "stall.test", "down.test")
            .pull("busybox:1.36")
            .unwrap_err();
        assert_eq!(failures.len(), 6, "{failures:?}");
        assert!(failures[..2].iter().all(|f| f.contains("timed out")), "{failures:?}");
        assert!(failures[2..].iter().all(|f| f.contains("toomanyrequests")), "{failures:?}");

        let summary = dir.path().join("summary.md");
        std::fs::write(&summary, "").unwrap();
        let message =
            hub_image::report_infrastructure_failure("busybox:1.36", &failures, Some(&summary));
        assert!(message.starts_with("INFRASTRUCTURE FAILURE (not a test failure)"));
        assert!(message.contains("down.test/library/busybox:1.36 (attempt 4)"));
        let written = std::fs::read_to_string(&summary).unwrap();
        assert!(written.contains("### Infrastructure failure: image pull"));
        assert!(written.contains("busybox:1.36"));
    }

    const UBUNTU_PIN: &str =
        "sha256:534baea6a22c03a63003dbc8dbe78fe34bc0d7e595d9a9dc9834884ff530eb55";

    #[test]
    fn a_pinned_image_comes_from_ghcr_first() {
        let dir = tempfile::tempdir().unwrap();
        let image = format!("clickhouse/clickhouse-server:25.12.5@{DIGEST}");
        let reference =
            ghcr_puller(dir.path(), "ghcr.test/ci", "down.test", "down.test").pull(&image);
        assert_eq!(reference, Ok(format!("ghcr.test/ci/clickhouse/clickhouse-server@{DIGEST}")));
        assert_eq!(calls(dir.path(), "pull --quiet down.test/"), 0);
    }

    #[test]
    fn an_unreachable_ghcr_falls_back_to_the_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let image = format!("clickhouse/clickhouse-server:25.12.5@{DIGEST}");
        let reference =
            ghcr_puller(dir.path(), "down.test/ci", "up.test", "down.test").pull(&image);
        assert_eq!(reference, Ok(format!("up.test/clickhouse/clickhouse-server@{DIGEST}")));
        assert_eq!(calls(dir.path(), "pull --quiet down.test/ci/"), 2);
    }

    #[test]
    fn a_tag_only_request_for_a_pinned_image_gets_the_pinned_digest_under_its_tag() {
        let dir = tempfile::tempdir().unwrap();
        let reference =
            ghcr_puller(dir.path(), "ghcr.test/ci", "down.test", "down.test").pull("ubuntu:24.04");
        assert_eq!(reference, Ok("ubuntu:24.04".to_string()));
        let pulled = format!("ghcr.test/ci/library/ubuntu@{UBUNTU_PIN}");
        assert_eq!(calls(dir.path(), &format!("pull --quiet {pulled}")), 1);
        assert_eq!(calls(dir.path(), &format!("tag {pulled} ubuntu:24.04")), 1);
    }

    #[test]
    fn an_image_off_the_pin_list_never_tries_ghcr() {
        let dir = tempfile::tempdir().unwrap();
        let reference =
            ghcr_puller(dir.path(), "ghcr.test/ci", "up.test", "down.test").pull("busybox:1.36");
        assert_eq!(reference, Ok("busybox:1.36".to_string()));
        assert_eq!(calls(dir.path(), "pull --quiet ghcr.test/"), 0);
    }
}
