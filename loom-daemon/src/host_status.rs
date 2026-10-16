//! Host status sampler (#12112, slice a of #11809).
//!
//! Samples CPU, memory, disk and network on a fixed interval, filters each raw
//! series with a first-order EWMA, and returns a
//! [`HostStatusRecord`](crate::telemetry::kinds::host_status::HostStatusRecord)
//! for the OTLP gauge export. The latest record is published for
//! [`latest_raw`] readers (role-span attributes, #12113); admission and GPU
//! are later slices.
//!
//! The sampler is split so the arithmetic is testable without an OS:
//! [`read_os`] turns the platform into a [`Readings`], and
//! [`HostStatusSampler::step`] turns successive `Readings` into a record.
//!
//! **Absent is never zero** (the [`crate::host_pressure`] rule): a reading that
//! failed, a counter that reset and the first tick of any rate are `None`, the
//! signal is missing from the record, and the filter keeps its state instead
//! of being fed a fake zero.
//!
//! Platforms: Linux reads every signal. macOS reads load, memory pressure,
//! swap and volume space; CPU utilisation (`iostat` blocks for a second), disk
//! I/O and network counters have no cheap non-blocking source there and stay
//! absent.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use chrono::Utc;

use crate::telemetry::kinds::host_status::{
    HostStatusRecord, SignalScope, SignalSpec, VolumeStatus, SIGNALS,
};

/// The closed label of the only volume sampled in this slice.
pub const WORKTREE_VOLUME: &str = "worktree_root";

/// Sampler configuration. The EWMA time constants live in
/// [`SIGNALS`] (one per signal, stated in each filtered gauge's name), so the
/// defaults here are the whole configuration: an override would make a gauge
/// name lie about its constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostStatusConfig {
    /// Time between samples.
    pub interval: Duration,
}

impl Default for HostStatusConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
        }
    }
}

impl HostStatusConfig {
    /// The time constant of `key`, from [`SIGNALS`].
    #[must_use]
    pub fn tau_secs(key: &str) -> Option<u64> {
        spec(key).map(|s| s.tau_secs)
    }
}

fn spec(key: &str) -> Option<&'static SignalSpec> {
    SIGNALS.iter().find(|s| s.key == key)
}

/// Cumulative CPU jiffies, as `/proc/stat`'s aggregate line gives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuCounters {
    /// Idle plus iowait jiffies.
    pub idle: u64,
    /// All jiffies.
    pub total: u64,
}

/// Cumulative disk counters, summed over whole disks (busiest disk for time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskCounters {
    /// Bytes read since boot, over whole disks.
    pub read_bytes: u64,
    /// Bytes written since boot, over whole disks.
    pub write_bytes: u64,
    /// Per whole disk, milliseconds with I/O in flight since boot.
    pub io_ms_max: u64,
}

/// Cumulative network counters over non-loopback interfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetCounters {
    /// Bytes received since boot.
    pub rx_bytes: u64,
    /// Bytes transmitted since boot.
    pub tx_bytes: u64,
}

/// One tick's raw OS readings. `None` is "could not read", never zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Readings {
    /// Cumulative CPU time.
    pub cpu: Option<CpuCounters>,
    /// 1-minute load average per logical core.
    pub load_per_core: Option<f64>,
    /// Physical memory in use.
    pub mem_used_bytes: Option<u64>,
    /// Memory available without reclaim.
    pub mem_available_bytes: Option<u64>,
    /// Memory pressure grade (0 none, 1 some, 2 full).
    pub mem_pressure_grade: Option<u64>,
    /// Swap in use.
    pub swap_used_bytes: Option<u64>,
    /// Cumulative disk counters.
    pub disk: Option<DiskCounters>,
    /// Cumulative network counters.
    pub net: Option<NetCounters>,
    /// Free bytes of the worktree-root volume.
    pub volume_free_bytes: Option<u64>,
}

/// One EWMA filter. A signal that goes unread for longer than
/// [`RESEED_TAUS`] time constants is re-seeded instead of blended with state
/// that old.
#[derive(Debug, Default, Clone, Copy)]
struct Ewma {
    value: f64,
    at: Option<f64>,
}

/// A reading after this many time constants of silence starts a fresh filter.
const RESEED_TAUS: f64 = 5.0;

impl Ewma {
    /// Feed `x` observed at `t` (seconds on the sampler's clock). Returns the
    /// filtered value, or `None` for an absent or non-finite reading, leaving
    /// the state untouched.
    fn update(&mut self, x: Option<f64>, t: f64, tau: f64) -> Option<f64> {
        let x = x.filter(|v| v.is_finite())?;
        let y = match self.at {
            Some(at) if t - at <= RESEED_TAUS * tau => {
                let alpha = 1.0 - (-(t - at).max(0.0) / tau).exp();
                self.value + alpha * (x - self.value)
            }
            _ => x,
        };
        self.value = y;
        self.at = Some(t);
        Some(y)
    }
}

/// The previous tick's counters and the instant they were read.
#[derive(Debug, Clone, Copy)]
struct Prev {
    t: f64,
    cpu: Option<CpuCounters>,
    disk: Option<DiskCounters>,
    net: Option<NetCounters>,
    volume_free_bytes: Option<u64>,
}

/// Stateful sampler: the filters and the previous counters.
#[derive(Debug, Default)]
pub struct HostStatusSampler {
    config: HostStatusConfig,
    host_filters: HashMap<&'static str, Ewma>,
    volume_filters: HashMap<&'static str, Ewma>,
    prev: Option<Prev>,
}

/// The most recent record the collector tick produced (#12113).
static LATEST: Mutex<Option<HostStatusRecord>> = Mutex::new(None);

/// Store `record` as the latest, for [`latest_raw`] readers such as the
/// role-span host attributes.
pub fn publish(record: &HostStatusRecord) {
    *LATEST.lock().unwrap_or_else(PoisonError::into_inner) = Some(record.clone());
}

/// The latest record's raw value for `key`. `None` when no record has been
/// published yet or the record has no such signal (absent is never zero).
#[must_use]
pub fn latest_raw(key: &str) -> Option<f64> {
    LATEST
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|r| r.raw.get(key).copied())
}

/// The latest record's filtered (EWMA) value for `key`: the one smoothed source
/// admission-side readers use (#12114). `None` when no record has been
/// published yet or the filter has not been fed this signal (absent is never
/// zero), so a caller keeps its unreadable-sample path for it.
#[must_use]
pub fn latest_filtered(key: &str) -> Option<f64> {
    LATEST
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|r| r.filtered.get(key).copied())
}

/// Filtered 1-minute load per logical core, the admission-side load reading.
#[must_use]
pub fn filtered_load_per_core() -> Option<f64> {
    latest_filtered("cpu_load_per_core")
}

/// Filtered 1-minute load average (`load_per_core` times `ncpu`), for the
/// callers that compare a raw load to a per-core threshold themselves.
#[must_use]
pub fn filtered_loadavg_1m(ncpu: usize) -> Option<f64> {
    filtered_load_per_core().map(|lpc| lpc * ncpu as f64)
}

/// Filtered CPU idle fraction (`1 - cpu_utilisation`), clamped to `0..=1`.
#[must_use]
pub fn filtered_cpu_idle_fraction() -> Option<f64> {
    latest_filtered("cpu_utilisation").map(|u| (1.0 - u).clamp(0.0, 1.0))
}

/// Serialises the tests that publish to the one global slot.
#[cfg(test)]
pub static TEST_SLOT: Mutex<()> = Mutex::new(());

/// Drop the published record (tests share the one global slot).
#[cfg(test)]
pub fn clear_latest() {
    *LATEST.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

/// `now - prev` over a cumulative counter, `None` when it went backwards (a
/// reboot or a reset) so the tick has no rate rather than a nonsense one.
fn delta(now: u64, prev: u64) -> Option<u64> {
    now.checked_sub(prev)
}

impl HostStatusSampler {
    /// A sampler with `config`.
    #[must_use]
    pub fn new(config: HostStatusConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// The configured sampling interval.
    #[must_use]
    pub fn interval(&self) -> Duration {
        self.config.interval
    }

    /// Fold one tick's `readings`, taken at `t` seconds on the sampler's
    /// clock, into a record. The first tick has no rates and seeds the
    /// filters with the instantaneous readings.
    pub fn step(&mut self, readings: &Readings, t: f64) -> HostStatusRecord {
        let prev = self.prev;
        let dt = prev.map(|p| t - p.t).filter(|dt| *dt > 0.0);
        let per_sec = |d: Option<u64>| dt.zip(d).map(|(dt, d)| d as f64 / dt);

        let mut raw: BTreeMap<&'static str, f64> = BTreeMap::new();
        let mut put = |key: &'static str, v: Option<f64>| {
            if let Some(v) = v.filter(|v| v.is_finite()) {
                raw.insert(key, v);
            }
        };

        if let (Some(now), Some(p)) = (readings.cpu, prev.and_then(|p| p.cpu)) {
            let total = delta(now.total, p.total);
            let idle = delta(now.idle, p.idle);
            if let (Some(total), Some(idle)) = (total, idle) {
                if total > 0 {
                    put(
                        "cpu_utilisation",
                        Some((1.0 - idle as f64 / total as f64).clamp(0.0, 1.0)),
                    );
                }
            }
        }
        put("cpu_load_per_core", readings.load_per_core);
        put("mem_used_bytes", readings.mem_used_bytes.map(|v| v as f64));
        put("mem_available_bytes", readings.mem_available_bytes.map(|v| v as f64));
        put("mem_pressure_grade", readings.mem_pressure_grade.map(|v| v as f64));
        put("swap_used_bytes", readings.swap_used_bytes.map(|v| v as f64));
        if let (Some(now), Some(p)) = (readings.disk, prev.and_then(|p| p.disk)) {
            put("disk_read_bytes_per_sec", per_sec(delta(now.read_bytes, p.read_bytes)));
            put("disk_write_bytes_per_sec", per_sec(delta(now.write_bytes, p.write_bytes)));
            put(
                "disk_io_busy",
                per_sec(delta(now.io_ms_max, p.io_ms_max))
                    .map(|ms_per_s| (ms_per_s / 1000.0).clamp(0.0, 1.0)),
            );
        }
        if let (Some(now), Some(p)) = (readings.net, prev.and_then(|p| p.net)) {
            put("net_rx_bytes_per_sec", per_sec(delta(now.rx_bytes, p.rx_bytes)));
            put("net_tx_bytes_per_sec", per_sec(delta(now.tx_bytes, p.tx_bytes)));
        }

        // Volume: fill rate is the fall in free bytes (positive while
        // filling); a volume that is not filling has no time-to-full, not an
        // infinite one.
        let mut vol_raw: BTreeMap<&'static str, f64> = BTreeMap::new();
        if let Some(free) = readings.volume_free_bytes {
            vol_raw.insert("vol_free_bytes", free as f64);
            if let (Some(dt), Some(pf)) = (dt, prev.and_then(|p| p.volume_free_bytes)) {
                let rate = (pf as f64 - free as f64) / dt;
                vol_raw.insert("vol_fill_rate_bytes_per_sec", rate.max(0.0));
            }
        }
        if let Some(ttf) = time_to_full(&vol_raw, "vol_free_bytes", "vol_fill_rate_bytes_per_sec") {
            vol_raw.insert("vol_time_to_full_secs", ttf);
        }

        let mut filtered = BTreeMap::new();
        for s in SIGNALS.iter().filter(|s| s.scope == SignalScope::Host) {
            let f = self.host_filters.entry(s.key).or_default();
            if let Some(y) = f.update(raw.get(s.key).copied(), t, s.tau_secs as f64) {
                filtered.insert(s.key.to_string(), y);
            }
        }
        let mut vol_filtered: BTreeMap<String, f64> = BTreeMap::new();
        for s in SIGNALS.iter().filter(|s| s.scope == SignalScope::Volume) {
            if s.key == "vol_time_to_full_secs" {
                continue; // derived below from the filtered free space and rate
            }
            let f = self.volume_filters.entry(s.key).or_default();
            if let Some(y) = f.update(vol_raw.get(s.key).copied(), t, s.tau_secs as f64) {
                vol_filtered.insert(s.key.to_string(), y);
            }
        }
        if let Some(ttf) = time_to_full_owned(&vol_filtered) {
            vol_filtered.insert("vol_time_to_full_secs".to_string(), ttf);
        }

        self.prev = Some(Prev {
            t,
            cpu: readings.cpu,
            disk: readings.disk,
            net: readings.net,
            volume_free_bytes: readings.volume_free_bytes,
        });

        let volumes = if vol_raw.is_empty() {
            Vec::new()
        } else {
            vec![VolumeStatus {
                volume: WORKTREE_VOLUME.to_string(),
                raw: vol_raw
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                filtered: vol_filtered,
            }]
        };
        HostStatusRecord {
            captured_at: Utc::now(),
            interval_secs: self.config.interval.as_secs(),
            raw: raw.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            filtered,
            volumes,
        }
    }
}

fn time_to_full(m: &BTreeMap<&'static str, f64>, free: &str, rate: &str) -> Option<f64> {
    let (free, rate) = (*m.get(free)?, *m.get(rate)?);
    (rate > 0.0).then(|| free / rate)
}

fn time_to_full_owned(m: &BTreeMap<String, f64>) -> Option<f64> {
    let (free, rate) = (*m.get("vol_free_bytes")?, *m.get("vol_fill_rate_bytes_per_sec")?);
    (rate > 0.0).then(|| free / rate)
}

/// Read every signal the platform can give. Blocking: a handful of small
/// `/proc` reads and one `statvfs` on Linux; run it under `spawn_blocking`.
#[must_use]
pub fn read_os(workspace_root: &Path) -> Readings {
    let pressure = crate::host_pressure::sample();
    let mem_used = pressure
        .mem_total_bytes
        .zip(pressure.mem_available_bytes)
        .and_then(|(total, avail)| total.checked_sub(avail));
    Readings {
        cpu: read_cpu(),
        load_per_core: crate::cpu_headroom::load_per_core(),
        mem_used_bytes: mem_used,
        mem_available_bytes: pressure.mem_available_bytes,
        mem_pressure_grade: pressure
            .memory_pressure
            .map(crate::host_pressure::MemoryPressure::grade),
        swap_used_bytes: pressure.swap_used_bytes,
        disk: read_disk(),
        net: read_net(),
        volume_free_bytes: volume_free_bytes(&workspace_root.join(".loom").join("worktrees")),
    }
}

/// Free bytes (`f_bavail * f_frsize`) of the volume holding `path`, walking up
/// to the nearest existing ancestor. `None` when `statvfs` fails.
#[must_use]
pub fn volume_free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let probe = path.ancestors().find(|p| p.exists())?;
    let c = std::ffi::CString::new(probe.as_os_str().as_bytes()).ok()?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a valid out
    // pointer; `statvfs` fully initialises it when it returns 0.
    let rc = unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: `statvfs` returned 0, so the struct is initialised.
    let st = unsafe { st.assume_init() };
    #[allow(clippy::unnecessary_cast)]
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[cfg(target_os = "linux")]
fn read_cpu() -> Option<CpuCounters> {
    let s = crate::cpu_headroom::sample_proc_stat_cpu()?;
    Some(CpuCounters {
        idle: s.idle,
        total: s.total,
    })
}

#[cfg(not(target_os = "linux"))]
fn read_cpu() -> Option<CpuCounters> {
    None
}

#[cfg(target_os = "linux")]
fn read_disk() -> Option<DiskCounters> {
    let text = std::fs::read_to_string("/proc/diskstats").ok()?;
    parse_diskstats(&text, |name| Path::new("/sys/block").join(name).exists())
}

#[cfg(not(target_os = "linux"))]
fn read_disk() -> Option<DiskCounters> {
    None
}

#[cfg(target_os = "linux")]
fn read_net() -> Option<NetCounters> {
    let text = std::fs::read_to_string("/proc/net/dev").ok()?;
    parse_net_dev(&text)
}

#[cfg(not(target_os = "linux"))]
fn read_net() -> Option<NetCounters> {
    None
}

/// Sum `/proc/diskstats` over whole disks (`is_whole_disk` rejects partitions;
/// loop and ram devices are skipped by name). Sectors are 512 bytes. `None`
/// when no whole disk is listed.
#[must_use]
pub fn parse_diskstats(text: &str, is_whole_disk: impl Fn(&str) -> bool) -> Option<DiskCounters> {
    let mut out = DiskCounters {
        read_bytes: 0,
        write_bytes: 0,
        io_ms_max: 0,
    };
    let mut any = false;
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let Some(&name) = f.get(2) else { continue };
        if f.len() < 13
            || name.starts_with("loop")
            || name.starts_with("ram")
            || !is_whole_disk(name)
        {
            continue;
        }
        let (Ok(rd), Ok(wr), Ok(ms)) =
            (f[5].parse::<u64>(), f[9].parse::<u64>(), f[12].parse::<u64>())
        else {
            continue;
        };
        out.read_bytes = out.read_bytes.saturating_add(rd.saturating_mul(512));
        out.write_bytes = out.write_bytes.saturating_add(wr.saturating_mul(512));
        out.io_ms_max = out.io_ms_max.max(ms);
        any = true;
    }
    any.then_some(out)
}

/// Sum `/proc/net/dev` over non-loopback interfaces. `None` when none is
/// listed or none parses.
#[must_use]
pub fn parse_net_dev(text: &str) -> Option<NetCounters> {
    let mut out = NetCounters {
        rx_bytes: 0,
        tx_bytes: 0,
    };
    let mut any = false;
    for line in text.lines() {
        let Some((iface, rest)) = line.split_once(':') else {
            continue;
        };
        if iface.trim() == "lo" {
            continue;
        }
        let f: Vec<&str> = rest.split_whitespace().collect();
        let (Some(rx), Some(tx)) = (
            f.first().and_then(|v| v.parse::<u64>().ok()),
            f.get(8).and_then(|v| v.parse::<u64>().ok()),
        ) else {
            continue;
        };
        out.rx_bytes = out.rx_bytes.saturating_add(rx);
        out.tx_bytes = out.tx_bytes.saturating_add(tx);
        any = true;
    }
    any.then_some(out)
}

#[cfg(test)]
#[path = "host_status_tests.rs"]
mod tests;
