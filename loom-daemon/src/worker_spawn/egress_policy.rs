//! Container egress boundary + spawn-time negative canary (#9989, C6 of epic
//! #9983, scope items 3 and 4).
//!
//! Routing by construction (the managed `gh` launcher, #9987) does not cover
//! what an agent *types*: `curl https://api.github.com/…`, a raw SDK, an
//! alternate `gh`. Inside a worker container Loom owns the network, so under
//! `enforcement.api = required` it denies TCP to `api.github.com` /
//! `uploads.github.com` there — no host or firewall change — and proves it
//! with the policy's `enforcement.negativeCanary` run **inside** the container
//! before the agent starts.
//!
//! # Mechanism (Docker): a netns-holding sidecar
//!
//! `--add-host` only changes name resolution, and giving the worker
//! `NET_ADMIN` would let the agent delete the rules. So the rules live in a
//! short-lived **sidecar** container (`NET_ADMIN`/`NET_RAW`, no other
//! capability, `no-new-privileges`) that owns the network namespace; the
//! worker joins it with `--network container:<sidecar>` and holds no network
//! capability, so it cannot alter them. The sidecar's entrypoint
//! ([`sidecar_script`]) resolves every host over IPv4 **and** IPv6, installs a
//! dedicated `OUTPUT` chain atomically with `iptables-restore --noflush`
//! ([`render_restore`]), and re-resolves every [`REFRESH_SECS`] — the CDN
//! rotates addresses, so a launch-time snapshot is not a boundary. Rule order:
//!
//! 1. TLS-SNI string match for the blocked hosts (catches an address shared
//!    with an allowed host; dropped, never fatal, if the kernel lacks
//!    `xt_string` — the canary then decides);
//! 2. `ACCEPT` for the addresses of the allowed hosts (`github.apiOrigin`,
//!    `github.com` for git transport);
//! 3. `REJECT --reject-with tcp-reset` for the addresses of the blocked hosts.
//!
//! Because the worker shares the sidecar's namespace, `--add-host` /
//! `--hostname` / `-p` / `--dns` are sidecar options (docker refuses them
//! alongside `--network container:`); [`Options::add_host_gateway`] carries the
//! one Loom needs (the credential proxy's `host.docker.internal`).
//!
//! # Canary and abort path
//!
//! [`establish`] runs the canary in a throwaway container joined to the same
//! namespace (same image, no capabilities). The outcome is classified into the
//! existing C1 finding codes through [`checks::assert_runtime`] — no parallel
//! report path: success ⇒ `runtime.bypass-open`; could not run (missing `curl`,
//! timeout, docker failure) ⇒ `runtime.unverifiable`; either aborts the spawn
//! under `required`, because Loom never claims an enforcement it did not
//! prove. A blocked canary is the only source of `runtime.verified`, which is
//! a log line, never a config assertion. Only env/machine-owned policies reach
//! this code ([`WorkerEgress`] is `None` for a repo-origin policy, which may
//! not name a command — see `Origin::may_run_canary`).
//!
//! Bare-metal hosts keep `runtime.unverifiable` (`forge egress doctor`, exit
//! 2): host network policy is 2am#1931, and Loom installs no host firewall rules.
//!
//! With no policy, or `enforcement.api = observe`, [`Boundary::from_egress`] is
//! `None` and every caller is byte-identical to its previous behaviour.

use std::collections::BTreeSet;
use std::net::ToSocketAddrs;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

use crate::forge_egress::checks::{assert_runtime, CanaryOutcome, Observed};
use crate::forge_egress::policy::{dig, dig_str, expected_api_host};
use crate::forge_egress::report::Finding;
use crate::forge_egress::worker_env::WorkerEgress;

/// Hosts whose TCP is denied inside the container.
pub const BLOCKED_HOSTS: [&str; 2] = ["api.github.com", "uploads.github.com"];
/// Git transport stays reachable.
pub const GIT_HOST: &str = "github.com";
/// `enforcement.negativeCanary` when the policy names none.
pub const DEFAULT_CANARY: &str = "curl -sS --max-time 5 https://api.github.com/zen";
/// The dedicated `OUTPUT` chain the sidecar owns.
pub const CHAIN: &str = "LOOM_FORGE_EGRESS";
/// Seconds between re-resolutions inside the sidecar.
pub const REFRESH_SECS: u64 = 30;
/// File the sidecar creates once the first rule set is installed.
pub const READY_FILE: &str = "/tmp/loom-egress-ready";
/// Label on every sidecar, so `docker ps --filter label=…` finds strays.
pub const SIDECAR_LABEL: &str = "loom.egress-sidecar";
/// Env var overriding the sidecar image (default: the worker image). It needs
/// `iptables`, `ip6tables`, `iptables-restore`, `getent` and `awk`.
pub const SIDECAR_IMAGE_ENV: &str = "LOOM_EGRESS_SIDECAR_IMAGE";

const READY_WAIT: Duration = Duration::from_secs(30);
const CANARY_BOUND: Duration = Duration::from_secs(30);

/// What a `required` policy asks the container network to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boundary {
    pub blocked_hosts: Vec<String>,
    /// `github.apiOrigin`'s host first, then [`GIT_HOST`].
    pub allowed_hosts: Vec<String>,
    /// The canary, run inside the container.
    pub canary: String,
    /// The policy document, for the C1 finding classifier.
    policy: Value,
}

impl Boundary {
    /// `Some` only under `enforcement.api = required` for a loaded
    /// env/machine policy; `None` for everything else (inert).
    #[must_use]
    pub fn from_egress(egress: &WorkerEgress) -> Option<Self> {
        if !egress.required {
            return None;
        }
        let text = std::fs::read_to_string(&egress.policy_file).ok()?;
        Self::from_policy(&serde_json::from_str(&text).ok()?)
    }

    /// [`Self::from_egress`] over an already-parsed policy document.
    #[must_use]
    pub fn from_policy(policy: &Value) -> Option<Self> {
        let mut allowed = Vec::new();
        let api = expected_api_host(policy);
        // A port (`host:8443`) is not part of the DNS name.
        let api = api.split(':').next().unwrap_or("").to_string();
        if !api.is_empty() && !BLOCKED_HOSTS.contains(&api.as_str()) {
            allowed.push(api);
        }
        if !allowed.iter().any(|h| h == GIT_HOST) {
            allowed.push(GIT_HOST.to_string());
        }
        let canary = dig(policy, &["enforcement", "negativeCanary"])
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .unwrap_or(DEFAULT_CANARY)
            .to_string();
        Some(Self {
            blocked_hosts: BLOCKED_HOSTS.iter().map(|h| (*h).to_string()).collect(),
            allowed_hosts: allowed,
            canary,
            policy: policy.clone(),
        })
    }

    /// Resolve every host now (IPv4 and IPv6) for the launch-time rule set.
    #[must_use]
    pub fn resolve(&self) -> Resolved {
        let mut out = Resolved::default();
        for (hosts, v4, v6) in [
            (&self.blocked_hosts, &mut out.blocked_v4, &mut out.blocked_v6),
            (&self.allowed_hosts, &mut out.allowed_v4, &mut out.allowed_v6),
        ] {
            for host in hosts {
                for addr in (host.as_str(), 443).to_socket_addrs().into_iter().flatten() {
                    if addr.is_ipv4() {
                        v4.insert(addr.ip().to_string());
                    } else {
                        v6.insert(addr.ip().to_string());
                    }
                }
            }
        }
        out
    }
}

/// Addresses by role and family, sorted and de-duplicated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    pub blocked_v4: BTreeSet<String>,
    pub blocked_v6: BTreeSet<String>,
    pub allowed_v4: BTreeSet<String>,
    pub allowed_v6: BTreeSet<String>,
}

/// IP family of a rule set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

/// The `iptables-restore --noflush` input that atomically replaces
/// [`CHAIN`]. `sni` adds the TLS-SNI string rules. The sidecar's shell
/// ([`sidecar_script`]) emits byte-identical text (tested against this).
#[must_use]
pub fn render_restore(
    family: Family,
    blocked_hosts: &[String],
    allowed: &BTreeSet<String>,
    blocked: &BTreeSet<String>,
    sni: bool,
) -> String {
    let width = if family == Family::V4 { "/32" } else { "/128" };
    let mut out = format!("*filter\n:{CHAIN} - [0:0]\n");
    if sni {
        for host in blocked_hosts {
            out.push_str(&format!(
                "-A {CHAIN} -p tcp --dport 443 -m string --string {host} --algo bm -j REJECT --reject-with tcp-reset\n"
            ));
        }
    }
    for ip in allowed {
        out.push_str(&format!("-A {CHAIN} -d {ip}{width} -j ACCEPT\n"));
    }
    for ip in blocked {
        out.push_str(&format!(
            "-A {CHAIN} -d {ip}{width} -p tcp -j REJECT --reject-with tcp-reset\n"
        ));
    }
    out.push_str("COMMIT\n");
    out
}

/// The sidecar's `sh -c` program. Configuration arrives in the environment
/// (`LOOM_EGRESS_BLOCKED`, `LOOM_EGRESS_ALLOWED`, `LOOM_EGRESS_REFRESH`;
/// `LOOM_EGRESS_ONCE=1` exits after the first install — the test hook).
/// Exit 70: rules could not be installed (the spawn is aborted).
#[must_use]
pub fn sidecar_script() -> String {
    format!(
        r#"set -u
CHAIN={CHAIN}
READY={READY_FILE}
resolve() {{ getent "ahosts$1" "$2" 2>/dev/null | awk '{{print $1}}' | sort -u; }}
collect() {{ for h in $2; do resolve "$1" "$h"; done | sort -u; }}
render() {{
  fam=$1 sni=$2 w=/32
  [ "$fam" = v6 ] && w=/128
  printf '*filter\n:%s - [0:0]\n' "$CHAIN"
  if [ "$sni" = 1 ]; then
    for h in $LOOM_EGRESS_BLOCKED; do
      printf -- '-A %s -p tcp --dport 443 -m string --string %s --algo bm -j REJECT --reject-with tcp-reset\n' "$CHAIN" "$h"
    done
  fi
  collect "$fam" "$LOOM_EGRESS_ALLOWED" | while read -r ip; do
    [ -n "$ip" ] && printf -- '-A %s -d %s%s -j ACCEPT\n' "$CHAIN" "$ip" "$w"
  done
  collect "$fam" "$LOOM_EGRESS_BLOCKED" | while read -r ip; do
    [ -n "$ip" ] && printf -- '-A %s -d %s%s -p tcp -j REJECT --reject-with tcp-reset\n' "$CHAIN" "$ip" "$w"
  done
  printf 'COMMIT\n'
}}
apply() {{
  fam=$1 ipt=iptables
  [ "$fam" = v6 ] && ipt=ip6tables
  v4v6=$(render "$fam" 1)
  if ! printf '%s\n' "$v4v6" | "$ipt-restore" --noflush 2>/dev/null; then
    printf '%s\n' "$(render "$fam" 0)" | "$ipt-restore" --noflush || return 1
  fi
  "$ipt" -C OUTPUT -j "$CHAIN" 2>/dev/null || "$ipt" -I OUTPUT 1 -j "$CHAIN" || return 1
}}
has_global_v6() {{ [ -r /proc/net/if_inet6 ] && awk '$6 != "lo"' /proc/net/if_inet6 | grep -q .; }}
apply v4 || {{ echo "loom-egress: cannot install IPv4 rules" >&2; exit 70; }}
if ! apply v6 && has_global_v6; then echo "loom-egress: cannot install IPv6 rules" >&2; exit 70; fi
: > "$READY"
[ "${{LOOM_EGRESS_ONCE:-}}" = 1 ] && exit 0
while :; do
  sleep "${{LOOM_EGRESS_REFRESH:-{REFRESH_SECS}}}"
  apply v4 || true
  apply v6 || true
done
"#
    )
}

/// Options for [`establish`].
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Map `host.docker.internal` in the shared namespace (credential proxy).
    pub add_host_gateway: bool,
    /// Host pid whose exit removes the sidecar (the process that execs the
    /// worker's `docker run`). `None`: the caller removes it.
    pub watch_pid: Option<u32>,
}

/// A live sidecar the worker must join with [`Sidecar::network_args`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    pub name: String,
}

impl Sidecar {
    /// `docker run` arguments that put the worker in the boundary.
    #[must_use]
    pub fn network_args(&self) -> Vec<String> {
        vec![
            "--network".into(),
            format!("container:{}", self.name),
            "--label".into(),
            format!("{SIDECAR_LABEL}={}", self.name),
        ]
    }
}

/// The docker calls [`establish`] makes, injectable so the abort path is
/// testable without a daemon. `Some((exit, output))`; `None` = could not run
/// or exceeded `bound`.
pub trait Docker {
    fn run(&self, args: &[String], bound: Duration) -> Option<(i32, String)>;
}

/// Real `docker`, bounded by coreutils `timeout`.
pub struct RealDocker;

impl Docker for RealDocker {
    fn run(&self, args: &[String], bound: Duration) -> Option<(i32, String)> {
        let out = Command::new("timeout")
            .arg("--kill-after=5")
            .arg(bound.as_secs().max(1).to_string())
            .arg("docker")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        let code = out.status.code()?;
        if code == 124 || code == 137 {
            return None;
        }
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Some((code, text))
    }
}

/// Classify the canary's exit status. `docker run`'s own failures (125) and
/// "command not found"/"cannot execute" (126/127 — an image without `curl`)
/// are not a verdict: they are [`CanaryOutcome::NotRun`], never `Blocked`.
#[must_use]
pub fn classify_canary(result: Option<(i32, String)>) -> CanaryOutcome {
    match result {
        None => CanaryOutcome::NotRun("did not complete within the bound"),
        Some((0, _)) => CanaryOutcome::Open,
        Some((125, _)) => CanaryOutcome::NotRun("docker could not start the canary container"),
        Some((126 | 127, _)) => {
            CanaryOutcome::NotRun("the canary's tool is missing from the image")
        }
        Some(_) => CanaryOutcome::Blocked,
    }
}

/// The C1 finding(s) for a canary outcome, through the one classifier
/// ([`assert_runtime`]). Empty = the boundary held (`runtime.verified`).
#[must_use]
pub fn canary_findings(boundary: &Boundary, outcome: CanaryOutcome) -> Vec<Finding> {
    let mut policy = boundary.policy.clone();
    policy["enforcement"]["negativeCanary"] = json!(boundary.canary);
    let obs = Observed {
        canary: Some(outcome),
        ..Observed::default()
    };
    assert_runtime(&policy, &obs)
}

/// The sidecar `docker run` argv.
#[must_use]
pub fn sidecar_args(
    name: &str,
    image: &str,
    boundary: &Boundary,
    opts: &Options,
    resolve_secs: u64,
) -> Vec<String> {
    let mut a: Vec<String> = [
        "run",
        "-d",
        "--rm",
        "--name",
        name,
        "--cap-drop",
        "ALL",
        "--cap-add",
        "NET_ADMIN",
        "--cap-add",
        "NET_RAW",
        "--security-opt",
        "no-new-privileges",
        "--label",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    a.push(format!("{SIDECAR_LABEL}={name}"));
    if opts.add_host_gateway {
        a.push("--add-host".into());
        a.push("host.docker.internal:host-gateway".into());
    }
    for (k, v) in [
        ("LOOM_EGRESS_BLOCKED", boundary.blocked_hosts.join(" ")),
        ("LOOM_EGRESS_ALLOWED", boundary.allowed_hosts.join(" ")),
        ("LOOM_EGRESS_REFRESH", resolve_secs.to_string()),
    ] {
        a.push("-e".into());
        a.push(format!("{k}={v}"));
    }
    a.extend([
        "--entrypoint".into(),
        "sh".into(),
        image.into(),
        "-c".into(),
        sidecar_script(),
    ]);
    a
}

/// A sidecar name unique per launch.
fn sidecar_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("loom-egress-{}-{nanos:08x}", std::process::id())
}

fn refusal(code: &'static str, observed: String) -> Box<Finding> {
    Box::new(
        Finding::new(code, "direct GitHub API egress is blocked for this workload")
            .expected("the container egress boundary installed and its canary blocked")
            .observed(observed)
            .source("container egress sidecar (#9989)")
            .remedy(
                "the worker image (or LOOM_EGRESS_SIDECAR_IMAGE) needs iptables, ip6tables, \
                 getent and the canary's tool; docker/worker/README.md \"Forge egress boundary\"",
            )
            .incomplete(),
    )
}

/// Install the boundary for one worker and prove it, or return the finding
/// that aborts the spawn.
///
/// `Ok(None)` when `egress` is not `required` (inert). `Ok(Some(sidecar))`
/// when the canary was **blocked**: the worker joins it with
/// [`Sidecar::network_args`]. On `Err` the sidecar is already removed.
///
/// # Errors
/// `runtime.bypass-open` when the canary reached the API; `runtime.unverifiable`
/// when the boundary could not be installed or the canary could not run.
pub fn establish(
    egress: &WorkerEgress,
    image: &str,
    opts: &Options,
) -> Result<Option<Sidecar>, Box<Finding>> {
    let Some(boundary) = Boundary::from_egress(egress) else {
        return Ok(None);
    };
    let sidecar_image = std::env::var(SIDECAR_IMAGE_ENV)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| image.to_string());
    establish_with(&RealDocker, &boundary, image, &sidecar_image, opts, READY_WAIT)
}

/// [`establish`] against an injected [`Docker`].
///
/// # Errors
/// As [`establish`].
pub fn establish_with(
    docker: &dyn Docker,
    boundary: &Boundary,
    image: &str,
    sidecar_image: &str,
    opts: &Options,
    ready_wait: Duration,
) -> Result<Option<Sidecar>, Box<Finding>> {
    let name = sidecar_name();
    let sidecar = Sidecar { name: name.clone() };
    let remove = || {
        docker.run(&["rm".into(), "-f".into(), name.clone()], Duration::from_secs(20));
    };
    let started = docker.run(
        &sidecar_args(&name, sidecar_image, boundary, opts, REFRESH_SECS),
        Duration::from_secs(60),
    );
    if !matches!(started, Some((0, _))) {
        remove();
        let why = started
            .map_or_else(|| "timed out".to_string(), |(c, o)| format!("exit {c}: {}", tail(&o)));
        return Err(refusal("runtime.unverifiable", format!("sidecar did not start ({why})")));
    }
    // Ready = the rules are installed. The sidecar exits 70 if they cannot be.
    let deadline = std::time::Instant::now() + ready_wait;
    loop {
        let probe = docker.run(
            &[
                "exec".into(),
                name.clone(),
                "test".into(),
                "-f".into(),
                READY_FILE.into(),
            ],
            Duration::from_secs(10),
        );
        if matches!(probe, Some((0, _))) {
            break;
        }
        let gone = !matches!(
            docker.run(
                &["inspect".into(), "-f".into(), "{{.State.Running}}".into(), name.clone()],
                Duration::from_secs(10),
            ),
            Some((0, ref o)) if o.trim() == "true"
        );
        if gone || std::time::Instant::now() >= deadline {
            let logs = docker
                .run(
                    &["logs".into(), "--tail".into(), "5".into(), name.clone()],
                    Duration::from_secs(10),
                )
                .map(|(_, o)| tail(&o))
                .unwrap_or_default();
            remove();
            return Err(refusal(
                "runtime.unverifiable",
                format!("the egress rules were not installed ({logs})"),
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let canary = docker.run(
        &[
            "run".into(),
            "--rm".into(),
            "--network".into(),
            format!("container:{name}"),
            "--cap-drop".into(),
            "ALL".into(),
            "--entrypoint".into(),
            "sh".into(),
            image.into(),
            "-c".into(),
            boundary.canary.clone(),
        ],
        CANARY_BOUND,
    );
    let outcome = classify_canary(canary);
    if let Some(finding) = canary_findings(boundary, outcome).into_iter().next() {
        remove();
        return Err(Box::new(finding));
    }
    if let Some(pid) = opts.watch_pid {
        reap_when_gone(pid, &name);
    }
    Ok(Some(sidecar))
}

fn tail(text: &str) -> String {
    let t = text.trim();
    t.chars()
        .rev()
        .take(300)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Detached host-side reaper: removes the sidecar once `pid` (the worker's
/// `docker run` client) is gone. Best effort — the label finds any stray.
fn reap_when_gone(pid: u32, name: &str) {
    use std::os::unix::process::CommandExt;
    let _ = Command::new("sh")
        .arg("-c")
        .arg("while kill -0 \"$1\" 2>/dev/null; do sleep 2; done; docker rm -f \"$2\" >/dev/null 2>&1")
        .arg("sh")
        .arg(pid.to_string())
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
}

/// The log line for a canary that was blocked — the only `runtime.verified`.
#[must_use]
pub fn verified_message(sidecar: &Sidecar) -> String {
    format!(
        "forge-egress: runtime.verified: the container canary was blocked (sidecar {}); \
         verified by the canary, not by configuration",
        sidecar.name
    )
}

#[cfg(test)]
#[path = "egress_policy_tests.rs"]
mod tests;
