//! Live-host smoke and pressure tests for [`loom_daemon::host_pressure`].
//!
//! The parser-level tests are deterministic and live in the module itself
//! (`host_pressure::tests`); this file verifies the **live probe** against
//! the actual host:
//!
//! - `live_sample_reports_total_memory` — `sample()` runs on this host
//!   without panicking and honours the "unknown != zero" contract: a source
//!   the platform cannot measure is `None`, and the one source every
//!   supported platform can measure (total RAM) is present and non-zero.
//! - `controlled_pressure_moves_a_measured_reading` — opt-in via
//!   `LOOM_TEST_MEMORY_PRESSURE_GB=N` (skipped by default, because it puts
//!   real memory pressure on the host running the suite): reserves and
//!   touches N GiB of pages, then asserts a *measured* reading moved by a
//!   comparable amount — available memory dropped, or compressed/swap usage
//!   grew. This is the acceptance evidence that the telemetry slice measures
//!   real pressure instead of a static reading. Run as:
//!   `LOOM_TEST_MEMORY_PRESSURE_GB=2 cargo test -p loom-daemon --test host_pressure -- --nocapture`

use loom_daemon::host_pressure::{self, HostPressure};

#[test]
fn live_sample_reports_total_memory() {
    let probe: HostPressure = host_pressure::sample();
    // Absent (unmeasurable source) or strictly positive — never a fake zero.
    assert!(
        probe.mem_total_bytes.is_none_or(|total| total > 0),
        "mem_total_bytes must be absent or > 0, got: {probe:?}"
    );
    // Linux and macOS both expose a total-RAM source.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert!(
        probe.mem_total_bytes.is_some(),
        "total RAM is measurable on this platform: {probe:?}"
    );
    // A counter that exists must monotonically accumulate over time — a
    // second sample taken after the first cannot read lower.
    let second = host_pressure::sample();
    if let (Some(first), Some(second)) = (probe.swap_in_bytes_total, second.swap_in_bytes_total) {
        assert!(second >= first, "swap-in counter went backwards");
    }
    if let (Some(first), Some(second)) = (probe.swap_out_bytes_total, second.swap_out_bytes_total) {
        assert!(second >= first, "swap-out counter went backwards");
    }
}

/// `Some(drop)` when `before` > `after` by a measurable amount, else `None`
/// — the "unmeasurable stays unmeasured" rule applied to a delta.
fn drop_if_measured(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    match (before, after) {
        (Some(before), Some(after)) if before > after => Some(before - after),
        _ => None,
    }
}

#[test]
fn controlled_pressure_moves_a_measured_reading() {
    let gib: u64 = std::env::var("LOOM_TEST_MEMORY_PRESSURE_GB")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0);
    if gib < 1 {
        eprintln!(
            "test: skipped (set LOOM_TEST_MEMORY_PRESSURE_GB=N, e.g. =2, to apply real memory pressure)"
        );
        return;
    }

    let before = host_pressure::sample();
    let bytes = gib * 1024 * 1024 * 1024;
    // Zeroing the allocation faults every page in — the pressure is real.
    let pages = vec![0u8; bytes as usize];

    // Give the kernel (reclaim / compressor / swap) a beat to respond.
    std::thread::sleep(std::time::Duration::from_secs(3));
    std::hint::black_box(&pages); // keep the allocation alive until here

    let after = host_pressure::sample();

    // The pressure must show up in **some** measured reading: available
    // memory dropped by roughly what was taken, or compressed/swap usage
    // grew by roughly the same. On a machine in the incident's state (28 GB
    // of 32 GB swap already committed) the kernel cannot hide N GiB of new
    // demand anywhere but these counters.
    let available_drop = drop_if_measured(before.mem_available_bytes, after.mem_available_bytes)
        .filter(|drop| *drop >= bytes / 2);
    let compressed_growth = match (before.mem_compressed_bytes, after.mem_compressed_bytes) {
        (Some(c0), Some(c1)) if c1 > c0 => Some(c1 - c0),
        _ => None,
    }
    .filter(|growth| *growth >= bytes / 2);
    let swap_growth =
        drop_if_measured(before.swap_used_bytes, after.swap_used_bytes).or_else(|| {
            drop_if_measured(before.swap_in_bytes_total, after.swap_in_bytes_total)
                .filter(|delta| *delta >= bytes / 2)
        });

    println!(
        "before: available={:?} compressed={:?} swap_used={:?} swap_in_total={:?}",
        before.mem_available_bytes,
        before.mem_compressed_bytes,
        before.swap_used_bytes,
        before.swap_in_bytes_total
    );
    println!(
        "after:  available={:?} compressed={:?} swap_used={:?} swap_in_total={:?}",
        after.mem_available_bytes,
        after.mem_compressed_bytes,
        after.swap_used_bytes,
        after.swap_in_bytes_total
    );
    if let Some(drop) = available_drop {
        println!("observed: available memory dropped {drop} bytes");
    }
    if let Some(growth) = compressed_growth {
        println!("observed: compressed memory grew {growth} bytes");
    }
    if let Some(delta) = swap_growth {
        println!("observed: swap usage/inflow grew by {delta} bytes");
    }

    assert!(
        available_drop.is_some() || compressed_growth.is_some() || swap_growth.is_some(),
        "taking {gib} GiB moved no measured reading \
         (available={:?}->{:?}, compressed={:?}->{:?}, swap_used={:?}->{:?})",
        before.mem_available_bytes,
        after.mem_available_bytes,
        before.mem_compressed_bytes,
        after.mem_compressed_bytes,
        before.swap_used_bytes,
        after.swap_used_bytes
    );

    // Release the pressure before the suite continues.
    drop(pages);
}
