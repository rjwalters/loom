//! The Docker Hub mirror helper (#11232), without Docker: reference mapping,
//! and the pull's deadline, fallback and terminal report against a fake
//! `docker` that stalls or fails on demand.

#[allow(dead_code)] // `resolve` needs Docker; exercised by the Docker-backed proofs.
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

#[cfg(unix)]
mod fake_docker {
    use super::{hub_image, DIGEST};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    /// A `docker` that has no local images, hangs on any pull from
    /// `stall.test`, refuses any pull from `down.test`, and serves the rest.
    /// Every invocation is appended to `calls` beside it.
    const SCRIPT: &str = r#"#!/bin/sh
echo "$*" >> "$(dirname "$0")/calls"
case "$1 $*" in
  image*) exit 1 ;;
  pull*stall.test/*) exec sleep 30 ;;
  pull*down.test/*) echo "toomanyrequests: fake rate limit" >&2; exit 1 ;;
esac
exit 0
"#;

    /// One fake `docker` for the whole binary, written before any test spawns
    /// it: a script written while a sibling thread forks is briefly "text file
    /// busy" to exec. Each test gets its own hard link, so `calls` stays per
    /// test.
    fn puller(dir: &Path, mirror: &str, hub: &str) -> hub_image::Puller {
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
    fn tag_only_image_is_tagged_back_to_its_docker_hub_name() {
        let dir = tempfile::tempdir().unwrap();
        let reference = puller(dir.path(), "up.test", "down.test").pull("ubuntu:24.04");
        assert_eq!(reference, Ok("ubuntu:24.04".to_string()));
        assert_eq!(calls(dir.path(), "tag up.test/library/ubuntu:24.04 ubuntu:24.04"), 1);
        assert_eq!(calls(dir.path(), "pull --quiet down.test/"), 0);
    }

    #[test]
    fn exhausted_pull_reports_every_attempt_as_infrastructure() {
        let dir = tempfile::tempdir().unwrap();
        let failures = puller(dir.path(), "stall.test", "down.test")
            .pull("ubuntu:24.04")
            .unwrap_err();
        assert_eq!(failures.len(), 6, "{failures:?}");
        assert!(failures[..2].iter().all(|f| f.contains("timed out")), "{failures:?}");
        assert!(failures[2..].iter().all(|f| f.contains("toomanyrequests")), "{failures:?}");

        let summary = dir.path().join("summary.md");
        std::fs::write(&summary, "").unwrap();
        let message =
            hub_image::report_infrastructure_failure("ubuntu:24.04", &failures, Some(&summary));
        assert!(message.starts_with("INFRASTRUCTURE FAILURE (not a test failure)"));
        assert!(message.contains("down.test/library/ubuntu:24.04 (attempt 4)"));
        let written = std::fs::read_to_string(&summary).unwrap();
        assert!(written.contains("### Infrastructure failure: image pull"));
        assert!(written.contains("ubuntu:24.04"));
    }
}
