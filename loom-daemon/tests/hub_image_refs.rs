//! Reference mapping for the Docker Hub mirror helper (#11232). Pure string
//! checks, no Docker: the pull itself is exercised by every Docker-backed proof.

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
