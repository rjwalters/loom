//! The emitting host's **static shape** — CPU count, RAM, architecture and
//! (where readable) instance type / cloud region — for the OTLP `Resource`
//! every exported signal carries (#12874).
//!
//! The ETA predictor needs to tell an 8-core sweep from a 16-core one. Before
//! this module the only CPU-count signal was the per-host
//! `loom.host.logical_cpus` gauge (a time join away from any sweep record) and
//! nothing read an instance type at all. Putting the shape on the resource
//! makes every `loom.sweep` / `loom.phase` span and every `sweep.*` log
//! record resolve to it with no join.
//!
//! # Two cells, two lifetimes
//!
//! - **Static part** ([`static_shape`]) — logical CPU count (the same
//!   [`cpu_headroom::logical_cpu_count`] source as the gauge), total RAM
//!   ([`host_pressure::mem_total_bytes`]), the OTel-semconv architecture and,
//!   on macOS, `sysctl -n hw.model`. Computed lazily on first read and cached
//!   for the process: none of it changes while the daemon runs. (The
//!   resource *itself* is still rebuilt per batch — only these facts are
//!   memoized.)
//! - **Cloud part** ([`cloud_shape`]) — `host.type` / `cloud.provider` /
//!   `cloud.region` from a one-shot AWS IMDSv2 probe, Linux only
//!   ([`spawn_imds_probe`]). The probe runs in a spawned task with a total
//!   budget of [`IMDS_TIMEOUT`]; the resource reads the cell without
//!   blocking, so records exported before the probe finishes — or on a host
//!   where it never succeeds — simply lack the keys. Never retried.
//!
//! # Measured, never fabricated
//!
//! RAM that cannot be read is **absent**, never `0`; a probe that fails
//! leaves no `host.type` / `cloud.*` key rather than an `unknown` sentinel —
//! unlike `loom.fleet.config_sha`, absence here has one meaning ("this host
//! has no readable instance type"), so no sentinel is needed.
//!
//! # Not the executing host of a stage outcome
//!
//! `eta.stage_outcome` records are emitted by whichever host *observed* a
//! transition, so their resource shape is the observer's, not the executor's.
//! Attributing a stage outcome to its executing host's shape is a join through
//! the executing `host.id` on `sweep.*` records — a consumer concern, not this
//! module's.

use std::sync::OnceLock;
use std::time::Duration;

use crate::{cpu_headroom, host_pressure};

/// OTel semconv: logical CPU count of the host (int).
pub const HOST_CPU_COUNT: &str = "host.cpu.count";
/// Loom: total physical memory of the host in bytes (int; absent if unread).
pub const HOST_MEM_TOTAL_BYTES: &str = "loom.host.mem_total_bytes";
/// OTel semconv: host architecture (`amd64`, `arm64`, …).
pub const HOST_ARCH: &str = "host.arch";
/// OTel semconv: host type — the EC2 instance type, or a Mac's `hw.model`.
pub const HOST_TYPE: &str = "host.type";
/// OTel semconv: cloud provider (`aws`).
pub const CLOUD_PROVIDER: &str = "cloud.provider";
/// OTel semconv: cloud region (`us-east-1`, …).
pub const CLOUD_REGION: &str = "cloud.region";

/// Every resource key this module can emit — the set the bundled collector's
/// three `context: resource` `keep_keys` lists must admit (contract-tested).
pub const RESOURCE_KEYS: [&str; 6] = [
    HOST_CPU_COUNT,
    HOST_MEM_TOTAL_BYTES,
    HOST_ARCH,
    HOST_TYPE,
    CLOUD_PROVIDER,
    CLOUD_REGION,
];

/// The AWS instance metadata service's link-local base URL.
pub const IMDS_BASE: &str = "http://169.254.169.254";

/// The IMDS probe's **total** budget (token PUT + both GETs).
pub const IMDS_TIMEOUT: Duration = Duration::from_millis(1000);

/// The facts that never change while the daemon runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticShape {
    /// Logical CPU count (`available_parallelism`, floored at 1).
    pub cpu_count: usize,
    /// Total physical memory in bytes; `None` when unmeasurable.
    pub mem_total_bytes: Option<u64>,
    /// OTel-semconv architecture, see [`otel_arch`].
    pub arch: &'static str,
    /// A non-cloud host model (`sysctl hw.model` on macOS); `None` elsewhere.
    pub model: Option<String>,
}

/// What the IMDSv2 probe learned about an EC2 host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudShape {
    /// Always `aws` today (no other cloud's metadata service is probed).
    pub provider: &'static str,
    /// e.g. `c7i.2xlarge`.
    pub instance_type: String,
    /// e.g. `us-east-1`.
    pub region: String,
}

static STATIC: OnceLock<StaticShape> = OnceLock::new();
static CLOUD: OnceLock<CloudShape> = OnceLock::new();
static PROBE_SPAWNED: OnceLock<()> = OnceLock::new();

#[cfg(test)]
thread_local! {
    /// Per-test-thread stand-in for [`CLOUD`] (which tests never read), so a
    /// test that wants the cloud keys on the resource never leaks them into a
    /// sibling test's exact key-set assertion.
    static TEST_CLOUD: std::cell::RefCell<Option<CloudShape>> =
        const { std::cell::RefCell::new(None) };
}

/// Map Rust's `std::env::consts::ARCH` onto the OTel `host.arch` vocabulary;
/// an architecture outside the table passes through unchanged.
#[must_use]
pub fn otel_arch(raw: &'static str) -> &'static str {
    match raw {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "x86",
        "powerpc64" => "ppc64",
        "s390x" => "s390x",
        other => other,
    }
}

/// The host's static shape, computed on first call and cached.
pub fn static_shape() -> &'static StaticShape {
    STATIC.get_or_init(|| StaticShape {
        cpu_count: cpu_headroom::logical_cpu_count().max(1),
        mem_total_bytes: host_pressure::mem_total_bytes(),
        arch: otel_arch(std::env::consts::ARCH),
        model: host_model(),
    })
}

/// The cloud shape, once the IMDS probe has succeeded; `None` until then (and
/// forever on a non-EC2 host). Never blocks.
#[must_use]
pub fn cloud_shape() -> Option<CloudShape> {
    // Under test only the per-thread stand-in counts: a test binary that
    // happens to run `observability::init` on an EC2 CI runner would
    // otherwise fill the process-wide cell and break every exact key-set
    // assertion.
    #[cfg(test)]
    {
        TEST_CLOUD.with(|cell| cell.borrow().clone())
    }
    #[cfg(not(test))]
    {
        CLOUD.get().cloned()
    }
}

/// Set the cloud shape on the current test thread only (`None` clears it).
#[cfg(test)]
pub fn set_cloud_for_test(shape: Option<CloudShape>) {
    TEST_CLOUD.with(|cell| *cell.borrow_mut() = shape);
}

/// The `host.type` value: the EC2 instance type when the probe found one,
/// else a non-cloud model (macOS `hw.model`), else nothing.
#[must_use]
pub fn host_type() -> Option<String> {
    cloud_shape()
        .map(|cloud| cloud.instance_type)
        .or_else(|| static_shape().model.clone())
}

#[cfg(target_os = "macos")]
fn host_model() -> Option<String> {
    let output = std::process::Command::new("sysctl")
        .args(["-n", "hw.model"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    clean_value(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(target_os = "macos"))]
fn host_model() -> Option<String> {
    None
}

/// A metadata value fit for a resource attribute: trimmed, non-empty, at most
/// 64 bytes, and only `[A-Za-z0-9._,-]` — an instance type, region or Mac
/// model never needs more, and anything else (an HTML error page from a
/// captive portal, say) is not what was asked for.
fn clean_value(raw: &str) -> Option<String> {
    let value = raw.trim();
    let ok = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b','));
    ok.then(|| value.to_string())
}

/// Probe AWS IMDSv2 at `base` once: `PUT /latest/api/token`, then `GET`
/// `meta-data/instance-type` and `meta-data/placement/region` with that token.
/// Any transport error, non-2xx status, unfit body, or the whole exchange
/// exceeding `timeout` yields `None`. No retries.
///
/// The client is built with `.no_proxy()`: reqwest honours `HTTP_PROXY` from
/// the environment by default, and a proxied link-local request would either
/// hang or ask a proxy about *its* metadata.
pub async fn probe_imds(base: &str, timeout: Duration) -> Option<CloudShape> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .connect_timeout(timeout)
        .build()
        .ok()?;
    let base = base.trim_end_matches('/');
    let exchange = async {
        let token = client
            .put(format!("{base}/latest/api/token"))
            .header("X-aws-ec2-metadata-token-ttl-seconds", "60")
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .text()
            .await
            .ok()?;
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        let get = |path: &str| {
            client
                .get(format!("{base}/latest/meta-data/{path}"))
                .header("X-aws-ec2-metadata-token", token)
                .send()
        };
        let instance_type = get("instance-type")
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .text()
            .await
            .ok()?;
        let region = get("placement/region")
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .text()
            .await
            .ok()?;
        Some(CloudShape {
            provider: "aws",
            instance_type: clean_value(&instance_type)?,
            region: clean_value(&region)?,
        })
    };
    tokio::time::timeout(timeout, exchange).await.ok().flatten()
}

/// Spawn the one-shot IMDS probe (at most once per process) on the current
/// tokio runtime; a success fills the cloud cell for the life of the process.
/// Never awaited by the caller, so daemon startup is not delayed. Linux-only
/// at the call site (`observability::init`): fleet EC2 hosts are Linux, and a
/// Mac's `host.type` comes from `hw.model` instead.
pub fn spawn_imds_probe() {
    if PROBE_SPAWNED.set(()).is_err() {
        return;
    }
    tokio::spawn(async {
        match probe_imds(IMDS_BASE, IMDS_TIMEOUT).await {
            Some(shape) => {
                log::info!(
                    "host shape: IMDS reports instance type {} in {}",
                    shape.instance_type,
                    shape.region
                );
                let _ = CLOUD.set(shape);
            }
            None => log::debug!("host shape: IMDS not reachable; no host.type/cloud.* keys"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn arch_maps_to_otel_semconv() {
        assert_eq!(otel_arch("x86_64"), "amd64");
        assert_eq!(otel_arch("aarch64"), "arm64");
        assert_eq!(otel_arch("x86"), "x86");
        assert_eq!(otel_arch("powerpc64"), "ppc64");
        assert_eq!(otel_arch("riscv64"), "riscv64");
    }

    #[test]
    fn static_shape_is_plausible_and_never_zero_ram() {
        let shape = static_shape();
        assert!(shape.cpu_count >= 1);
        assert_ne!(shape.mem_total_bytes, Some(0));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(shape.mem_total_bytes.is_some(), "RAM should read here");
        #[cfg(target_os = "macos")]
        assert!(shape.model.is_some(), "hw.model should read on macOS");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(shape.model, None);
    }

    #[test]
    fn values_are_cleaned() {
        assert_eq!(clean_value(" c7i.2xlarge\n"), Some("c7i.2xlarge".into()));
        assert_eq!(clean_value("Mac14,6"), Some("Mac14,6".into()));
        assert_eq!(clean_value(""), None);
        assert_eq!(clean_value("<html>nope</html>"), None);
        assert_eq!(clean_value(&"a".repeat(65)), None);
    }

    #[test]
    fn host_type_prefers_the_cloud_instance_type() {
        set_cloud_for_test(Some(CloudShape {
            provider: "aws",
            instance_type: "m7g.xlarge".into(),
            region: "us-west-2".into(),
        }));
        assert_eq!(host_type().as_deref(), Some("m7g.xlarge"));
        set_cloud_for_test(None);
        assert_eq!(host_type(), static_shape().model);
    }

    /// A loopback IMDS stand-in. `token_status` is the token PUT's status
    /// line; the request lines it saw (with the token header, if any) are
    /// collected in the returned log.
    async fn fake_imds(token_status: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let mut req = Vec::new();
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8_lossy(&req).to_ascii_lowercase();
                let line = text.lines().next().unwrap_or_default().to_string();
                let token = text
                    .lines()
                    .find_map(|l| l.strip_prefix("x-aws-ec2-metadata-token: "))
                    .unwrap_or("-")
                    .trim()
                    .to_string();
                log.lock().unwrap().push(format!("{line} token={token}"));
                let (status, body) = if line.starts_with("put /latest/api/token") {
                    (token_status, "tok-123")
                } else if token != "tok-123" {
                    ("401 Unauthorized", "")
                } else if line.starts_with("get /latest/meta-data/instance-type") {
                    ("200 OK", "c7i.2xlarge")
                } else if line.starts_with("get /latest/meta-data/placement/region") {
                    ("200 OK", "us-east-1")
                } else {
                    ("404 Not Found", "")
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (base, seen)
    }

    #[tokio::test]
    async fn probe_reads_instance_type_and_region_with_a_token() {
        let (base, seen) = fake_imds("200 OK").await;
        let shape = probe_imds(&base, Duration::from_secs(5)).await;
        assert_eq!(
            shape,
            Some(CloudShape {
                provider: "aws",
                instance_type: "c7i.2xlarge".into(),
                region: "us-east-1".into(),
            })
        );
        let seen = seen.lock().unwrap().clone();
        assert!(seen[0].starts_with("put /latest/api/token"), "{seen:?}");
        assert!(seen[1..].iter().all(|l| l.ends_with("token=tok-123")), "{seen:?}");
    }

    #[tokio::test]
    async fn probe_refused_token_is_none() {
        let (base, seen) = fake_imds("401 Unauthorized").await;
        assert_eq!(probe_imds(&base, Duration::from_secs(5)).await, None);
        assert_eq!(seen.lock().unwrap().len(), 1, "no GET after a refused token");
    }

    #[tokio::test]
    async fn probe_of_a_closed_port_is_none_within_the_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let timeout = Duration::from_millis(500);
        let started = std::time::Instant::now();
        assert_eq!(probe_imds(&base, timeout).await, None);
        assert!(started.elapsed() < timeout * 2, "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn probe_of_a_silent_host_is_none_within_the_timeout() {
        // Accepts and never answers: the total budget, not a per-request
        // one, bounds the probe.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        let timeout = Duration::from_millis(300);
        let started = std::time::Instant::now();
        assert_eq!(probe_imds(&base, timeout).await, None);
        assert!(started.elapsed() < timeout * 2, "{:?}", started.elapsed());
    }
}
