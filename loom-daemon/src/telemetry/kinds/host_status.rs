//! `host.status` (#12112, slice a of #11809): a fixed-interval sample of this
//! host's CPU, memory, disk and network, raw and EWMA-filtered.
//!
//! `host.health` is a five-minute snapshot and the role spans carry host load
//! only while a role is running, so an idle host or a dead daemon reported
//! nothing. This record is the host's own view, sampled by
//! [`crate::host_status`] every few seconds and exported as OTLP gauges.
//!
//! **Absent is never zero.** A signal that could not be read this tick (no
//! `/proc` source, a counter that reset, the first tick of a rate) is missing
//! from its map, and the mapping produces no data point for it.
//!
//! **OTLP only.** Every signal in [`SIGNALS`] names its gauges, so the
//! exported vocabulary is one table: `system.*` semconv names where the
//! semantics match, `loom.host.*` names otherwise. A filtered series states
//! its time constant in its name, which is why the constants are fixed
//! ([`crate::host_status::HostStatusConfig`]) rather than per-host overrides.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Whether a signal is one per host or one per volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalScope {
    /// One series for the host.
    Host,
    /// One series per volume, labelled `volume`.
    Volume,
}

/// One exported signal: its map key, filter time constant and gauge names.
#[derive(Debug, Clone, Copy)]
pub struct SignalSpec {
    /// Key in [`HostStatusRecord::raw`] / [`HostStatusRecord::filtered`] (and
    /// in a volume's maps).
    pub key: &'static str,
    /// Host-wide or per volume.
    pub scope: SignalScope,
    /// The first-order EWMA time constant, seconds. The filtered gauge's name
    /// states it.
    pub tau_secs: u64,
    /// The raw gauge name, or `None` when another record already exports it.
    pub raw_name: Option<&'static str>,
    /// The filtered gauge name, ending `_ewma_<tau_secs>s`.
    pub filtered_name: &'static str,
    /// OTLP unit.
    pub unit: &'static str,
    /// Gauge description.
    pub description: &'static str,
    /// A fixed `state` label on the raw gauge (`system.memory.usage` etc.).
    pub raw_state: Option<&'static str>,
}

macro_rules! signal {
    ($key:literal, $scope:ident, $tau:literal, $raw:expr, $filtered:literal, $unit:literal,
     $desc:literal, $state:expr) => {
        SignalSpec {
            key: $key,
            scope: SignalScope::$scope,
            tau_secs: $tau,
            raw_name: $raw,
            filtered_name: $filtered,
            unit: $unit,
            description: $desc,
            raw_state: $state,
        }
    };
}

/// Every signal `host.status` exports, in a stable order.
pub const SIGNALS: &[SignalSpec] =
    &[
        signal!(
            "cpu_utilisation",
            Host,
            30,
            Some("system.cpu.utilization"),
            "loom.host.cpu_utilisation_ewma_30s",
            "1",
            "Fraction of CPU time spent non-idle (0..1).",
            None
        ),
        signal!(
            "cpu_load_per_core",
            Host,
            30,
            Some("loom.host.cpu_load_per_core"),
            "loom.host.cpu_load_per_core_ewma_30s",
            "{thread}",
            "1-minute load average per logical core.",
            None
        ),
        signal!(
            "mem_used_bytes",
            Host,
            30,
            Some("system.memory.usage"),
            "loom.host.mem_used_bytes_ewma_30s",
            "By",
            "Physical memory in use (total minus available).",
            Some("used")
        ),
        signal!(
            "mem_available_bytes",
            Host,
            30,
            None,
            "loom.host.mem_available_bytes_ewma_30s",
            "By",
            "Memory available without reclaim.",
            None
        ),
        signal!(
            "mem_pressure_grade",
            Host,
            30,
            Some("loom.host.mem_pressure_grade"),
            "loom.host.mem_pressure_grade_ewma_30s",
            "1",
            "Memory pressure grade: 0 none, 1 some, 2 full.",
            None
        ),
        signal!(
            "swap_used_bytes",
            Host,
            60,
            Some("system.paging.usage"),
            "loom.host.swap_used_bytes_ewma_60s",
            "By",
            "Swap in use.",
            Some("used")
        ),
        signal!(
            "disk_io_busy",
            Host,
            30,
            Some("loom.host.disk_io_busy"),
            "loom.host.disk_io_busy_ewma_30s",
            "1",
            "Fraction of time the busiest whole disk had I/O in flight (0..1).",
            None
        ),
        signal!(
            "disk_read_bytes_per_sec",
            Host,
            30,
            Some("loom.host.disk_read_bytes_per_sec"),
            "loom.host.disk_read_bytes_per_sec_ewma_30s",
            "By/s",
            "Disk read throughput over whole disks.",
            None
        ),
        signal!(
            "disk_write_bytes_per_sec",
            Host,
            30,
            Some("loom.host.disk_write_bytes_per_sec"),
            "loom.host.disk_write_bytes_per_sec_ewma_30s",
            "By/s",
            "Disk write throughput over whole disks.",
            None
        ),
        signal!(
            "net_rx_bytes_per_sec",
            Host,
            30,
            Some("loom.host.net_rx_bytes_per_sec"),
            "loom.host.net_rx_bytes_per_sec_ewma_30s",
            "By/s",
            "Network receive throughput over non-loopback interfaces.",
            None
        ),
        signal!(
            "net_tx_bytes_per_sec",
            Host,
            30,
            Some("loom.host.net_tx_bytes_per_sec"),
            "loom.host.net_tx_bytes_per_sec_ewma_30s",
            "By/s",
            "Network transmit throughput over non-loopback interfaces.",
            None
        ),
        signal!(
            "vol_free_bytes",
            Volume,
            300,
            Some("system.filesystem.usage"),
            "loom.host.disk_free_bytes_ewma_300s",
            "By",
            "Free bytes on the volume.",
            Some("free")
        ),
        signal!(
            "vol_fill_rate_bytes_per_sec",
            Volume,
            300,
            Some("loom.host.disk_fill_rate_bytes_per_sec"),
            "loom.host.disk_fill_rate_bytes_per_sec_ewma_300s",
            "By/s",
            "Rate the volume is filling (free space falling); present only while it fills.",
            None
        ),
        signal!("vol_time_to_full_secs", Volume, 300, Some("loom.host.disk_time_to_full_seconds"),
        "loom.host.disk_time_to_full_seconds_ewma_300s", "s",
        "Seconds until the volume is full at the current fill rate; present only while it fills.",
        None),
    ];

/// One volume's raw and filtered signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeStatus {
    /// A closed label for the volume (`worktree_root`), never a path.
    pub volume: String,
    /// Raw values by [`SignalSpec::key`]; a missing key is an unread signal.
    #[serde(default)]
    pub raw: BTreeMap<String, f64>,
    /// EWMA-filtered values by key.
    #[serde(default)]
    pub filtered: BTreeMap<String, f64>,
}

/// `host.status`: one sample of the host's resource signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostStatusRecord {
    /// When the sample was taken.
    pub captured_at: DateTime<Utc>,
    /// The sampling interval, seconds.
    pub interval_secs: u64,
    /// Raw host-wide values by [`SignalSpec::key`].
    #[serde(default)]
    pub raw: BTreeMap<String, f64>,
    /// EWMA-filtered host-wide values by key.
    #[serde(default)]
    pub filtered: BTreeMap<String, f64>,
    /// Per-volume signals.
    #[serde(default)]
    pub volumes: Vec<VolumeStatus>,
}
