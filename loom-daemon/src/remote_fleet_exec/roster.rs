//! Fleet-host membership for the remote-fleet-exec guard (#11568).
//!
//! A host is a fleet host when it is a tailnet address or name (`*.ts.net`,
//! an IPv4 address in Tailscale's `100.64.0.0/10`, or an IPv6 address in its
//! `fd7a:115c:a1e0::/48`), or when it names a host on a roster: the Loom
//! fleet registry (`LOOM_FLEET_PATH`, else `~/.loom/fleet.json` — `ssh_host`,
//! `tailnet_name`, `provider_instance_id`), the local `tailscale status
//! --json` peer list, or the extend-only `guards.remoteFleetHosts` config
//! array (where an operator lists hosts a roster file such as a fleet-gitops
//! `hosts.yml` names). An `ssh` alias is resolved through `ssh -G` first, so
//! `loom-worker-1` → `100.x.y.z` is caught even with no roster at all.
//!
//! Every source is read lazily and only when the guard has already found a
//! non-read-only payload, so an ordinary `ssh host 'git status'` never pays
//! for a roster read or a subprocess.

use std::cell::OnceCell;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Test seam and operator override: read `tailscale status --json` output
/// from this file instead of running the CLI. An empty value or a missing
/// file means "no tailnet peers".
pub const TAILSCALE_STATUS_FILE_ENV: &str = "LOOM_GUARD_TAILSCALE_STATUS_FILE";
/// The extend-only config array of extra fleet host names.
pub const HOSTS_KEY: &str = "guards.remoteFleetHosts";

/// Answers "is this a fleet host?".
pub trait Roster {
    fn is_fleet_host(&self, host: &str) -> bool;
    fn is_fleet_instance(&self, instance_id: &str) -> bool;
}

/// Tailnet address/name test that needs no roster.
#[must_use]
pub fn is_tailnet_literal(host: &str) -> bool {
    let h = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if h.ends_with(".ts.net") {
        return true;
    }
    match h.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            let o = v4.octets();
            o[0] == 100 && (64..=127).contains(&o[1])
        }
        Ok(IpAddr::V6(v6)) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
        Err(_) => false,
    }
}

/// Names compare case-insensitively, and a dotless name matches the first
/// label of a dotted one (`loom-worker-1` ≡ `loom-worker-1.tail1234.ts.net`).
#[must_use]
pub fn names_match(a: &str, b: &str) -> bool {
    let a = a.trim().trim_end_matches('.').to_ascii_lowercase();
    let b = b.trim().trim_end_matches('.').to_ascii_lowercase();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let first = |s: &str| s.split('.').next().unwrap_or("").to_string();
    let a_ip = a.parse::<IpAddr>().is_ok();
    let b_ip = b.parse::<IpAddr>().is_ok();
    if a_ip || b_ip {
        return false;
    }
    (!a.contains('.') || !b.contains('.')) && first(&a) == first(&b)
}

/// The real roster: fleet registry, tailscale peers, config list, `ssh -G`.
pub struct ProcessRoster {
    workspace: PathBuf,
    names: OnceCell<Vec<String>>,
    instances: OnceCell<Vec<String>>,
}

impl ProcessRoster {
    #[must_use]
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            names: OnceCell::new(),
            instances: OnceCell::new(),
        }
    }

    fn registry(&self) -> crate::fleet::FleetRegistry {
        crate::fleet::FleetRegistry::load_default().unwrap_or_default()
    }

    fn names(&self) -> &[String] {
        self.names.get_or_init(|| {
            let mut out = Vec::new();
            for w in self.registry().workers {
                out.push(w.ssh_host.clone());
                out.extend(w.tailnet_name.clone());
            }
            out.extend(tailscale_names(&tailscale_status()));
            let config = crate::config_resolver::resolve_effective_config(&self.workspace);
            if let Some(Value::Array(items)) = crate::config_resolver::get_path(&config, HOSTS_KEY)
            {
                out.extend(items.iter().filter_map(Value::as_str).map(str::to_string));
            }
            out.retain(|n| !n.trim().is_empty());
            out
        })
    }

    fn on_roster(&self, host: &str) -> bool {
        is_tailnet_literal(host) || self.names().iter().any(|n| names_match(n, host))
    }
}

impl Roster for ProcessRoster {
    fn is_fleet_host(&self, host: &str) -> bool {
        if self.on_roster(host) {
            return true;
        }
        match ssh_resolved_hostname(host) {
            Some(resolved) if !resolved.eq_ignore_ascii_case(host) => self.on_roster(&resolved),
            _ => false,
        }
    }

    fn is_fleet_instance(&self, instance_id: &str) -> bool {
        self.instances
            .get_or_init(|| {
                self.registry()
                    .workers
                    .into_iter()
                    .filter_map(|w| w.provider_instance_id)
                    .collect()
            })
            .iter()
            .any(|i| i.eq_ignore_ascii_case(instance_id))
    }
}

/// Every name and address a `tailscale status --json` document lists for
/// `Self` and each `Peer`.
#[must_use]
pub fn tailscale_names(status: &Value) -> Vec<String> {
    let mut nodes: Vec<&Value> = Vec::new();
    if let Some(me) = status.get("Self") {
        nodes.push(me);
    }
    if let Some(Value::Object(peers)) = status.get("Peer") {
        nodes.extend(peers.values());
    }
    let mut out = Vec::new();
    for node in nodes {
        for key in ["HostName", "DNSName"] {
            if let Some(s) = node.get(key).and_then(Value::as_str) {
                out.push(s.to_string());
            }
        }
        if let Some(Value::Array(ips)) = node.get("TailscaleIPs") {
            out.extend(ips.iter().filter_map(Value::as_str).map(str::to_string));
        }
    }
    out
}

fn tailscale_status() -> Value {
    if let Some(path) = std::env::var_os(TAILSCALE_STATUS_FILE_ENV) {
        return std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
    }
    for bin in [
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ] {
        if let Some(out) = run_with_timeout(bin, &["status", "--json"]) {
            if let Ok(v) = serde_json::from_str(&out) {
                return v;
            }
        }
    }
    Value::Null
}

/// `ssh -G <host>`'s resolved `hostname` (no connection is made).
fn ssh_resolved_hostname(host: &str) -> Option<String> {
    if host.starts_with('-') {
        return None;
    }
    let out = run_with_timeout("ssh", &["-G", "--", host])?;
    out.lines()
        .find_map(|l| l.strip_prefix("hostname "))
        .map(|h| h.trim().to_string())
}

fn run_with_timeout(bin: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Drain stdout on a thread so a large document cannot fill the pipe and
    // stall the child past the deadline.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut s)
            .ok()
            .map(|_| s)
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().ok().flatten()?;
                return status.success().then_some(out);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}
