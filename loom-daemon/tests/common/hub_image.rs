//! Pull a Docker Hub image from this repository's GHCR copy (#11755), then a
//! mirror that does not rate-limit anonymous pulls, falling back to Docker Hub
//! with bounded retry (#11232).
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
//! An image on the pin list (`.github/ci-images.txt`) is pulled by its pinned
//! digest from `ghcr.io/rjwalters/loom-ci-images`, which
//! `.github/workflows/ci-images.yml` fills from the same list. GHCR is tried
//! first; until the copy has run, or while a package is still private (a fork
//! PR pulls without credentials), that attempt fails fast and the mirror
//! serves. The Docker Hub fallback stays until GHCR has proven itself in CI.
//!
//! Overrides, for reproducing each path by hand:
//! - `LOOM_TEST_IMAGE_GHCR`: GHCR prefix (default
//!   `ghcr.io/rjwalters/loom-ci-images`); empty skips GHCR.
//! - `LOOM_TEST_IMAGE_MIRROR`: mirror host (default `mirror.gcr.io`); empty
//!   skips the mirror.
//! - `LOOM_TEST_IMAGE_HUB`: host to use in place of Docker Hub for the
//!   fallback, e.g. an unreachable one to prove the mirror path stands alone.
//!
//! Every `docker` call has a deadline, so a stalled registry costs one attempt
//! instead of the job: the attempt count bounds retries, the deadline bounds
//! time.
//!
//! An image that cannot be pulled at all is not a test failure (#11756). A
//! test that only checks telemetry SQL or collector config guards itself with
//! [`available`]: on a pull request it is skipped with a
//! `::warning::image-unavailable <ref>` annotation and a job-summary line, so
//! a registry outage cannot block merges; on `main` (and locally) it still
//! fails, so a permanently broken image cannot hide behind skips. The
//! decision is the pure [`on_unavailable`] + [`outcome`].

// Each test binary uses its own subset of this module.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Where `.github/workflows/ci-images.yml` copies every pinned image.
const DEFAULT_GHCR: &str = "ghcr.io/rjwalters/loom-ci-images";
const GHCR_ATTEMPTS: usize = 2;
const DEFAULT_MIRROR: &str = "mirror.gcr.io";
const MIRROR_ATTEMPTS: usize = 2;
/// Waits between Docker Hub attempts; one more attempt than entries.
const HUB_BACKOFF: [Duration; 3] = [
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];
/// Deadline for one `docker pull`. Eight attempts (GHCR, mirror, Docker Hub)
/// plus backoff stay under the 30-minute job limit, so exhaustion is reported
/// rather than cancelled.
const PULL_DEADLINE: Duration = Duration::from_secs(180);
/// How much of a failed `docker` call's stderr is kept for the report.
const STDERR_TAIL_BYTES: u64 = 4096;
/// Deadline for a local `docker image inspect` / `docker tag`.
const LOCAL_DEADLINE: Duration = Duration::from_secs(60);

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

/// The pin list (#11755): every third-party image CI pulls, as
/// `<repo>:<tag>@sha256:<digest>`. The copy workflow reads the same file.
pub const PIN_LIST: &str = include_str!("../../../.github/ci-images.txt");

/// The pin list's entries, comments and blank lines dropped.
pub fn pins() -> Vec<&'static str> {
    PIN_LIST
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

/// The pin-list entry for `image`, if it has one: the exact entry for a
/// digest-pinned reference, or the entry with the same `<repo>:<tag>` for a
/// tag-only one (no tag means `latest`).
pub fn pinned(image: &str) -> Option<String> {
    let entries = pins();
    if image.contains('@') {
        return entries
            .into_iter()
            .find(|entry| *entry == image)
            .map(str::to_string);
    }
    let last = image.rsplit('/').next().unwrap_or(image);
    let wanted = if last.contains(':') {
        image.to_string()
    } else {
        format!("{image}:latest")
    };
    entries
        .into_iter()
        .find(|entry| {
            entry
                .split_once('@')
                .is_some_and(|(name, _)| name == wanted)
        })
        .map(str::to_string)
}

/// Where and how patiently to pull. [`Puller::from_env`] is what the proofs
/// use; the fields are public so a test can aim it at a fake `docker`.
pub struct Puller {
    pub docker: PathBuf,
    /// GHCR prefix the copy workflow fills; empty skips GHCR.
    pub ghcr: String,
    /// Mirror host; empty skips the mirror.
    pub mirror: String,
    /// Host standing in for Docker Hub on the fallback; empty means Docker Hub.
    pub hub: String,
    pub pull_deadline: Duration,
    pub hub_backoff: Vec<Duration>,
}

impl Puller {
    pub fn from_env() -> Self {
        let env =
            |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
        Self {
            docker: PathBuf::from("docker"),
            ghcr: env("LOOM_TEST_IMAGE_GHCR", DEFAULT_GHCR),
            mirror: env("LOOM_TEST_IMAGE_MIRROR", DEFAULT_MIRROR),
            hub: env("LOOM_TEST_IMAGE_HUB", ""),
            pull_deadline: PULL_DEADLINE,
            hub_backoff: HUB_BACKOFF.to_vec(),
        }
    }

    /// Runs `docker <args>`, killing and reaping it at `deadline`.
    ///
    /// stderr goes to an unlinked temp file, not a pipe: a pipe would block a
    /// chatty child once full, and reading one to its end waits on any
    /// descendant still holding it. A file does neither, so collecting the
    /// diagnostic cannot outlive the deadline.
    fn docker(&self, args: &[&str], deadline: Duration) -> Result<(), String> {
        let mut stderr = tempfile::tempfile().expect("temp file for docker stderr");
        let mut child = Command::new(&self.docker)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr.try_clone().expect("clone stderr file"))
            .spawn()
            .expect("docker is required for this test");
        let started = Instant::now();
        loop {
            match child.try_wait().expect("wait for docker") {
                Some(status) if status.success() => return Ok(()),
                Some(_) => return Err(tail(&mut stderr)),
                None if started.elapsed() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {}s", deadline.as_secs_f32()));
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn is_local(&self, reference: &str) -> bool {
        self.docker(&["image", "inspect", reference], LOCAL_DEADLINE)
            .is_ok()
    }

    /// Returns the reference to pass to `docker` for `image`, pulling it if it
    /// is not already local, or every attempt's failure once all are spent.
    ///
    /// An image on the pin list ([`pinned`]) is pulled by its pinned digest
    /// from every source, GHCR first; a tag-only request for it is tagged back
    /// to the requested name, so a later `FROM ubuntu:24.04` finds the pinned
    /// content locally.
    pub fn pull(&self, image: &str) -> Result<String, Vec<String>> {
        if !is_docker_hub(image) || self.is_local(image) {
            return Ok(image.to_string());
        }
        let listed = pinned(image);
        let source = listed.clone().unwrap_or_else(|| image.to_string());
        let by_digest = source.contains('@');
        let mut failures = Vec::new();

        // GHCR carries only what the copy workflow copied: the pin list.
        let mut registries = Vec::new();
        if listed.is_some() && !self.ghcr.is_empty() {
            registries.push(("GHCR", self.ghcr.as_str(), GHCR_ATTEMPTS));
        }
        if !self.mirror.is_empty() {
            registries.push(("mirror", self.mirror.as_str(), MIRROR_ATTEMPTS));
        }
        for (label, registry, attempts) in registries {
            let reference = on_registry(registry, &source);
            if by_digest && self.is_local(&reference) {
                if let Ok(reference) = self.named(&reference, image) {
                    return Ok(reference);
                }
            }
            for attempt in 1..=attempts {
                let pulled = self
                    .docker(&["pull", "--quiet", &reference], self.pull_deadline)
                    .and_then(|()| self.named(&reference, image));
                match pulled {
                    Ok(reference) => return Ok(reference),
                    Err(error) => {
                        failures.push(format!("{reference} (attempt {attempt}): {error}"));
                    }
                }
            }
            eprintln!("hub_image: {label} ({registry}) failed for {image}; trying the next source");
        }

        let hub = if self.hub.is_empty() {
            source.clone()
        } else {
            on_registry(&self.hub, &source)
        };
        for attempt in 0..=self.hub_backoff.len() {
            let pulled = self
                .docker(&["pull", "--quiet", &hub], self.pull_deadline)
                .and_then(|()| self.named(&hub, image));
            match pulled {
                Ok(reference) => return Ok(reference),
                Err(error) => failures.push(format!("{hub} (attempt {}): {error}", attempt + 1)),
            }
            if let Some(wait) = self.hub_backoff.get(attempt) {
                eprintln!("hub_image: Docker Hub pull of {image} failed; retrying in {wait:?}");
                std::thread::sleep(*wait);
            }
        }
        Err(failures)
    }

    /// The reference to hand back for a pulled `reference`: a digest-pinned
    /// request uses it as is (the digest already names the content); a
    /// tag-only request gets it tagged back to the requested name.
    fn named(&self, reference: &str, image: &str) -> Result<String, String> {
        if image.contains('@') {
            return Ok(reference.to_string());
        }
        if reference != image {
            self.docker(&["tag", reference, image], LOCAL_DEADLINE)?;
        }
        Ok(image.to_string())
    }
}

/// The last [`STDERR_TAIL_BYTES`] of a finished child's stderr, trimmed.
fn tail(stderr: &mut std::fs::File) -> String {
    let len = stderr.metadata().map_or(0, |meta| meta.len());
    let mut bytes = Vec::new();
    if stderr
        .seek(SeekFrom::Start(len.saturating_sub(STDERR_TAIL_BYTES)))
        .is_ok()
    {
        let _ = stderr.take(STDERR_TAIL_BYTES).read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// What an exhausted pull means for the test that asked for the image (#11756).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnUnavailable {
    /// Skip the test, loudly: a pull request, where a registry outage must
    /// not block the merge of a change that did not cause it.
    Skip,
    /// Fail the test: `main`, local runs and anything else, so a permanently
    /// broken image cannot hide behind skips.
    Fail,
}

/// Decides [`OnUnavailable`] from the environment, read through `env` so the
/// decision is testable without touching the process environment.
///
/// Skips only when GitHub Actions says this is a pull request (or merge-queue)
/// run whose ref is not `main`. Everything else fails closed: a push to
/// `main`, a schedule, a `workflow_dispatch`, and a developer's own run.
/// `LOOM_TEST_IMAGE_UNAVAILABLE=skip|fail` overrides, for reproducing either
/// path by hand.
pub fn on_unavailable(env: impl Fn(&str) -> Option<String>) -> OnUnavailable {
    match env("LOOM_TEST_IMAGE_UNAVAILABLE").as_deref() {
        Some("skip") => return OnUnavailable::Skip,
        Some("fail") => return OnUnavailable::Fail,
        _ => {}
    }
    if env("GITHUB_ACTIONS").as_deref() != Some("true")
        || env("GITHUB_REF").as_deref() == Some("refs/heads/main")
    {
        return OnUnavailable::Fail;
    }
    match env("GITHUB_EVENT_NAME").as_deref() {
        Some("pull_request" | "pull_request_target" | "merge_group") => OnUnavailable::Skip,
        _ => OnUnavailable::Fail,
    }
}

/// What a test does with one pull result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Run, handing `docker` this reference.
    Use(String),
    /// Skip: the image is unavailable and this run may say so.
    Skip(Vec<String>),
    /// Fail: the image is unavailable and this run must not hide it.
    Fail(Vec<String>),
}

/// The pure decision: a pulled image is used; an exhausted pull skips or
/// fails as `policy` says. A test failure never reaches here — only the pull.
pub fn outcome(pulled: Result<String, Vec<String>>, policy: OnUnavailable) -> Outcome {
    match (pulled, policy) {
        (Ok(reference), _) => Outcome::Use(reference),
        (Err(failures), OnUnavailable::Skip) => Outcome::Skip(failures),
        (Err(failures), OnUnavailable::Fail) => Outcome::Fail(failures),
    }
}

/// A pull's result: the reference to use, or every attempt's failure.
type Pulled = Result<String, Vec<String>>;

/// One pull per image per test binary, success or failure: parallel tests
/// wait for the first pull instead of racing their own, and a binary of a
/// dozen tests does not re-run the whole bounded retry chain a dozen times
/// once the registry has already said no.
fn pulled(image: &str) -> Pulled {
    static PULLED: OnceLock<Mutex<HashMap<String, Pulled>>> = OnceLock::new();
    // Held across the pull on purpose (see above).
    let mut pulled = PULLED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    pulled
        .entry(image.to_string())
        .or_insert_with(|| Puller::from_env().pull(image))
        .clone()
}

fn summary_path() -> Option<PathBuf> {
    std::env::var_os("GITHUB_STEP_SUMMARY").map(PathBuf::from)
}

/// Returns the reference to pass to `docker` for the Docker Hub image `image`,
/// pulling it if it is not already local. Panics, naming the failure as
/// infrastructure, when neither the mirror nor Docker Hub can serve it.
///
/// A test that may be skipped when the image is unavailable guards itself
/// with [`available`] first; this function never skips.
pub fn resolve(image: &str) -> String {
    pulled(image).unwrap_or_else(|failures| {
        panic!("{}", report_infrastructure_failure(image, &failures, summary_path().as_deref()))
    })
}

/// True when `image` can be pulled, so the calling test should run.
///
/// When every pull path failed (rate limit, network, 5xx), the outcome
/// follows [`on_unavailable`]: on a pull request this reports the skip — a
/// `::warning::image-unavailable <ref>` annotation and a job-summary line
/// naming the test — and returns false, so the test returns early and passes;
/// everywhere else it panics as an infrastructure failure, as before.
///
/// ```ignore
/// if !hub_image::available(CLICKHOUSE_IMAGE) {
///     return;
/// }
/// ```
pub fn available(image: &str) -> bool {
    match outcome(pulled(image), on_unavailable(|name| std::env::var(name).ok())) {
        Outcome::Use(_) => true,
        Outcome::Skip(failures) => {
            let test = std::thread::current()
                .name()
                .unwrap_or("<unnamed test>")
                .to_string();
            report_skip(image, &test, &failures, summary_path().as_deref());
            false
        }
        Outcome::Fail(failures) => {
            panic!("{}", report_infrastructure_failure(image, &failures, summary_path().as_deref()))
        }
    }
}

/// Reports one skipped test: a greppable warning annotation on stderr, the
/// failures behind it, and a line in the job summary when CI provides one, so
/// a reviewer can count the tests that did not run. Returns the annotation.
///
/// Written to the stderr handle directly, not through `eprintln!`, so the
/// test harness's output capture cannot swallow it on a passing test.
pub fn report_skip(image: &str, test: &str, failures: &[String], summary: Option<&Path>) -> String {
    let annotation = format!(
        "::warning title=Image unavailable: test skipped::image-unavailable {image} \
         (skipped {test}; every pull path failed, so this run did not exercise it)"
    );
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{annotation}");
    for failure in failures {
        let _ = writeln!(stderr, "hub_image:   {failure}");
    }
    if let Some(path) = summary {
        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = writeln!(
                file,
                "- :warning: **SKIPPED** `{test}`: image-unavailable `{image}` \
                 (registry outage on a pull request; `main` still fails on this)"
            );
        }
    }
    annotation
}

/// Reports an exhausted pull as infrastructure — in the job summary when CI
/// provides one — so a registry outage is not read as a test failure. Returns
/// the message to panic with.
pub fn report_infrastructure_failure(
    image: &str,
    failures: &[String],
    summary: Option<&Path>,
) -> String {
    let headline = format!(
        "INFRASTRUCTURE FAILURE (not a test failure): could not pull {image} \
         from GHCR, the mirror or Docker Hub"
    );
    println!("::error title=Infrastructure: image pull failed::{headline}");
    if let Some(path) = summary {
        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = writeln!(
                file,
                "### Infrastructure failure: image pull\n\n{headline}. Re-run the job; \
                 the change under test did not cause this.\n"
            );
        }
    }
    format!("{headline}\n{}", failures.join("\n"))
}
