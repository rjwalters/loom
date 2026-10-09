//! Pull a Docker Hub image through a mirror that does not rate-limit anonymous
//! pulls, falling back to Docker Hub with bounded retry (#11232).
//!
//! GitHub-hosted runners share egress IPs, so an anonymous `docker.io` pull can
//! hit `toomanyrequests` through no fault of the change under test. Every
//! Docker-backed proof asks [`resolve`] for the reference to hand to `docker`.
//!
//! A digest-pinned image keeps its digest: the mirror reference is the same
//! `sha256:` under another registry host, and the engine verifies the content
//! against it, so a mirror cannot serve different bytes. A tag-only image is
//! pulled from the mirror and tagged back to its Docker Hub name, so later
//! consumers of that name (a `FROM` line, the daemon under test) find it local.
//!
//! Included per test binary with `#[path = "common/hub_image.rs"]`, not through
//! `common/mod.rs`, which pulls in the daemon harness these proofs do not use.
//!
//! Overrides, for reproducing each path by hand:
//! - `LOOM_TEST_IMAGE_MIRROR`: mirror host (default `mirror.gcr.io`); empty
//!   skips the mirror.
//! - `LOOM_TEST_IMAGE_HUB`: host to use in place of Docker Hub for the
//!   fallback, e.g. an unreachable one to prove the mirror path stands alone.

use std::collections::HashMap;
use std::io::Write;
use std::process::Command;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

const DEFAULT_MIRROR: &str = "mirror.gcr.io";
const MIRROR_ATTEMPTS: usize = 2;
/// Waits between Docker Hub attempts; one more attempt than entries.
const HUB_BACKOFF_SECS: [u64; 3] = [15, 30, 60];

/// True when `image` names a Docker Hub repository (no registry host).
pub fn is_docker_hub(image: &str) -> bool {
    match image.split_once('/') {
        Some((first, _)) => !(first.contains('.') || first.contains(':') || first == "localhost"),
        None => true,
    }
}

/// `image` as served by `registry`: official images gain `library/`, and a
/// digest-pinned reference drops its tag so the digest alone selects content.
pub fn on_registry(registry: &str, image: &str) -> String {
    let (name, digest) = match image.split_once('@') {
        Some((name, digest)) => (name, Some(digest)),
        None => (image, None),
    };
    let (repo, tag) = name.rsplit_once(':').unwrap_or((name, "latest"));
    let repo = if repo.contains('/') {
        repo.to_string()
    } else {
        format!("library/{repo}")
    };
    match digest {
        Some(digest) => format!("{registry}/{repo}@{digest}"),
        None => format!("{registry}/{repo}:{tag}"),
    }
}

fn docker(args: &[&str]) -> Result<(), String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("docker is required for this test");
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn is_local(reference: &str) -> bool {
    docker(&["image", "inspect", reference]).is_ok()
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Returns the reference to pass to `docker` for the Docker Hub image `image`,
/// pulling it if it is not already local. Panics, naming the failure as
/// infrastructure, when neither the mirror nor Docker Hub can serve it.
pub fn resolve(image: &str) -> String {
    static RESOLVED: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    // Held across the pull on purpose: parallel tests in one binary wait for
    // the first pull instead of racing their own.
    let mut resolved = RESOLVED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(reference) = resolved.get(image) {
        return reference.clone();
    }
    let reference = pull(image);
    resolved.insert(image.to_string(), reference.clone());
    reference
}

fn pull(image: &str) -> String {
    if !is_docker_hub(image) || is_local(image) {
        return image.to_string();
    }
    let pinned = image.contains('@');
    let mut failures = Vec::new();

    let mirror = env_or("LOOM_TEST_IMAGE_MIRROR", DEFAULT_MIRROR);
    if !mirror.is_empty() {
        let mirrored = on_registry(&mirror, image);
        if pinned && is_local(&mirrored) {
            return mirrored;
        }
        for attempt in 1..=MIRROR_ATTEMPTS {
            let pulled = docker(&["pull", "--quiet", &mirrored]).and_then(|()| {
                if pinned {
                    Ok(mirrored.clone())
                } else {
                    docker(&["tag", &mirrored, image]).map(|()| image.to_string())
                }
            });
            match pulled {
                Ok(reference) => return reference,
                Err(error) => failures.push(format!("{mirrored} (attempt {attempt}): {error}")),
            }
        }
        eprintln!("hub_image: mirror failed for {image}; falling back to Docker Hub");
    }

    let hub = match std::env::var("LOOM_TEST_IMAGE_HUB") {
        Ok(host) if !host.is_empty() => on_registry(&host, image),
        _ => image.to_string(),
    };
    for attempt in 0..=HUB_BACKOFF_SECS.len() {
        match docker(&["pull", "--quiet", &hub]) {
            Ok(()) => return hub,
            Err(error) => failures.push(format!("{hub} (attempt {}): {error}", attempt + 1)),
        }
        if let Some(seconds) = HUB_BACKOFF_SECS.get(attempt) {
            eprintln!("hub_image: Docker Hub pull of {image} failed; retrying in {seconds}s");
            std::thread::sleep(Duration::from_secs(*seconds));
        }
    }
    infrastructure_failure(image, &failures)
}

/// Reports an exhausted pull as infrastructure — in the job summary when CI
/// provides one — so a registry outage is not read as a test failure.
fn infrastructure_failure(image: &str, failures: &[String]) -> ! {
    let headline = format!(
        "INFRASTRUCTURE FAILURE (not a test failure): could not pull {image} \
         from the mirror or from Docker Hub"
    );
    println!("::error title=Infrastructure: image pull failed::{headline}");
    if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        if let Ok(mut summary) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = writeln!(
                summary,
                "### Infrastructure failure: image pull\n\n{headline}. Re-run the job; \
                 the change under test did not cause this.\n"
            );
        }
    }
    panic!("{headline}\n{}", failures.join("\n"));
}
