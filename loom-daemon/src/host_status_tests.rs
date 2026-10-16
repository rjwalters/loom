use super::*;

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn default_config_is_five_seconds_with_stated_time_constants() {
    assert_eq!(HostStatusConfig::default().interval, Duration::from_secs(5));
    // The constants live in the signal table, and every filtered gauge name
    // states its own.
    for s in SIGNALS {
        assert!(
            s.filtered_name.ends_with(&format!("_ewma_{}s", s.tau_secs)),
            "{} must state tau {}",
            s.filtered_name,
            s.tau_secs
        );
    }
    assert_eq!(HostStatusConfig::tau_secs("cpu_utilisation"), Some(30));
    assert_eq!(HostStatusConfig::tau_secs("swap_used_bytes"), Some(60));
    assert_eq!(HostStatusConfig::tau_secs("vol_free_bytes"), Some(300));
    assert_eq!(HostStatusConfig::tau_secs("nope"), None);
}

#[test]
fn ewma_step_response_follows_one_minus_exp() {
    let tau = 30.0;
    let mut f = Ewma::default();
    assert!(approx(f.update(Some(0.0), 0.0, tau).unwrap(), 0.0), "first reading seeds");
    // Step to 1.0; after one time constant the output is 1 - 1/e of the way.
    let y = f.update(Some(1.0), tau, tau).unwrap();
    assert!(approx(y, 1.0 - (-1.0f64).exp()), "got {y}");
    // Five-second ticks converge monotonically toward the step and never overshoot.
    let mut f = Ewma::default();
    f.update(Some(0.0), 0.0, tau);
    let mut last = 0.0;
    for i in 1..=60 {
        let y = f.update(Some(1.0), f64::from(i) * 5.0, tau).unwrap();
        assert!(y > last && y < 1.0 + 1e-12);
        last = y;
    }
    assert!(last > 0.99);
}

#[test]
fn ewma_absent_reading_is_absent_and_keeps_state() {
    let mut f = Ewma::default();
    f.update(Some(10.0), 0.0, 30.0);
    assert_eq!(f.update(None, 5.0, 30.0), None);
    assert_eq!(f.update(Some(f64::NAN), 10.0, 30.0), None);
    // State was not pulled toward zero: the next reading blends with 10.0.
    let y = f.update(Some(10.0), 15.0, 30.0).unwrap();
    assert!(approx(y, 10.0));
}

#[test]
fn ewma_reseeds_after_long_silence() {
    let mut f = Ewma::default();
    f.update(Some(100.0), 0.0, 10.0);
    let y = f.update(Some(1.0), 1000.0, 10.0).unwrap();
    assert!(approx(y, 1.0), "state older than {RESEED_TAUS} taus is dropped");
}

fn readings() -> Readings {
    Readings {
        cpu: Some(CpuCounters {
            idle: 100,
            total: 200,
        }),
        load_per_core: Some(0.5),
        mem_used_bytes: Some(4_000),
        mem_available_bytes: Some(6_000),
        mem_pressure_grade: Some(0),
        swap_used_bytes: Some(0),
        disk: Some(DiskCounters {
            read_bytes: 0,
            write_bytes: 0,
            io_ms_max: 0,
        }),
        net: Some(NetCounters {
            rx_bytes: 0,
            tx_bytes: 0,
        }),
        volume_free_bytes: Some(1_000_000),
        gpu: None,
    }
}

#[test]
fn parse_nvidia_smi_aggregates_max_util_and_summed_memory() {
    let g = parse_nvidia_smi("30, 1000, 16000\n80, 2000, 16000\n").unwrap();
    assert!(approx(g.utilisation, 0.8));
    assert_eq!(g.mem_used_bytes, 3000 * 1024 * 1024);
    assert_eq!(g.mem_total_bytes, 32000 * 1024 * 1024);
}

#[test]
fn parse_nvidia_smi_skips_na_rows_and_empty_is_absent() {
    let g = parse_nvidia_smi("[N/A], 1000, 16000\n10, 500, 8000\n").unwrap();
    assert!(approx(g.utilisation, 0.1));
    assert_eq!(g.mem_total_bytes, 8000 * 1024 * 1024);
    assert_eq!(parse_nvidia_smi(""), None);
    assert_eq!(parse_nvidia_smi("[N/A], [N/A], [N/A]\n"), None);
}

#[test]
fn gpu_signals_present_only_when_read_and_absent_never_zero() {
    let mut s = HostStatusSampler::new(HostStatusConfig::default());
    let r = s.step(&readings(), 0.0);
    for k in [
        "gpu_utilisation",
        "gpu_mem_used_bytes",
        "gpu_mem_total_bytes",
    ] {
        assert!(!r.raw.contains_key(k) && !r.filtered.contains_key(k), "{k}");
    }
    let mut with = readings();
    with.gpu = Some(GpuReadings {
        utilisation: 0.5,
        mem_used_bytes: 10,
        mem_total_bytes: 100,
    });
    let r = s.step(&with, 5.0);
    assert!(approx(r.raw["gpu_utilisation"], 0.5));
    assert!(r.filtered.contains_key("gpu_mem_used_bytes"));
    // A later tick without a GPU reading drops the keys instead of emitting 0.
    let r = s.step(&readings(), 10.0);
    assert!(!r.raw.contains_key("gpu_utilisation"));
    assert!(!r.filtered.contains_key("gpu_utilisation"));
}

#[test]
fn first_tick_has_no_rates_and_seeds_instantaneous_filters() {
    let mut s = HostStatusSampler::new(HostStatusConfig::default());
    let r = s.step(&readings(), 0.0);
    for rate in [
        "cpu_utilisation",
        "disk_io_busy",
        "net_rx_bytes_per_sec",
        "disk_read_bytes_per_sec",
    ] {
        assert!(!r.raw.contains_key(rate), "{rate} needs two samples");
        assert!(!r.filtered.contains_key(rate));
    }
    assert!(approx(r.raw["cpu_load_per_core"], 0.5));
    assert!(approx(r.filtered["mem_used_bytes"], 4_000.0));
    assert_eq!(r.volumes.len(), 1);
    assert!(!r.volumes[0].raw.contains_key("vol_fill_rate_bytes_per_sec"));
}

#[test]
fn second_tick_derives_rates_from_counter_deltas() {
    let mut s = HostStatusSampler::new(HostStatusConfig::default());
    s.step(&readings(), 0.0);
    let mut b = readings();
    b.cpu = Some(CpuCounters {
        idle: 150,
        total: 300,
    });
    b.disk = Some(DiskCounters {
        read_bytes: 5_000,
        write_bytes: 10_000,
        io_ms_max: 2_500,
    });
    b.net = Some(NetCounters {
        rx_bytes: 500,
        tx_bytes: 50,
    });
    b.volume_free_bytes = Some(999_000);
    let r = s.step(&b, 5.0);
    assert!(approx(r.raw["cpu_utilisation"], 0.5));
    assert!(approx(r.raw["disk_read_bytes_per_sec"], 1_000.0));
    assert!(approx(r.raw["disk_write_bytes_per_sec"], 2_000.0));
    assert!(approx(r.raw["disk_io_busy"], 0.5));
    assert!(approx(r.raw["net_rx_bytes_per_sec"], 100.0));
    assert!(approx(r.raw["net_tx_bytes_per_sec"], 10.0));
    let v = &r.volumes[0];
    assert!(approx(v.raw["vol_fill_rate_bytes_per_sec"], 200.0));
    assert!(approx(v.raw["vol_time_to_full_secs"], 999_000.0 / 200.0));
    assert!(v.filtered.contains_key("vol_time_to_full_secs"));
}

#[test]
fn a_failed_reading_is_absent_never_zero() {
    let mut s = HostStatusSampler::new(HostStatusConfig::default());
    s.step(&readings(), 0.0);
    let r = s.step(&Readings::default(), 5.0);
    assert!(r.raw.is_empty(), "nothing readable: {:?}", r.raw);
    assert!(r.filtered.is_empty());
    assert!(r.volumes.is_empty(), "an unreadable volume is not a zero-byte volume");
    // And it recovers: the next full reading is a first tick for the rates
    // (the counters were lost), instantaneous signals are back.
    let r = s.step(&readings(), 10.0);
    assert!(r.raw.contains_key("mem_used_bytes"));
    assert!(!r.raw.contains_key("cpu_utilisation"));
}

#[test]
fn counter_reset_yields_no_rate_for_that_tick() {
    let mut s = HostStatusSampler::new(HostStatusConfig::default());
    let mut a = readings();
    a.net = Some(NetCounters {
        rx_bytes: 1_000_000,
        tx_bytes: 1_000_000,
    });
    a.cpu = Some(CpuCounters {
        idle: 1_000,
        total: 2_000,
    });
    s.step(&a, 0.0);
    let mut b = readings();
    b.net = Some(NetCounters {
        rx_bytes: 10,
        tx_bytes: 10,
    });
    b.cpu = Some(CpuCounters { idle: 5, total: 10 });
    let r = s.step(&b, 5.0);
    assert!(!r.raw.contains_key("net_rx_bytes_per_sec"));
    assert!(!r.raw.contains_key("net_tx_bytes_per_sec"));
    assert!(!r.raw.contains_key("cpu_utilisation"));
    // The reset sample is the new baseline, so the next tick has a rate.
    let mut c = b.clone();
    c.net = Some(NetCounters {
        rx_bytes: 60,
        tx_bytes: 10,
    });
    let r = s.step(&c, 10.0);
    assert!(approx(r.raw["net_rx_bytes_per_sec"], 10.0));
}

#[test]
fn a_volume_that_is_not_filling_has_no_time_to_full() {
    let mut s = HostStatusSampler::new(HostStatusConfig::default());
    s.step(&readings(), 0.0);
    let mut b = readings();
    b.volume_free_bytes = Some(1_000_500); // freeing space
    let r = s.step(&b, 5.0);
    let v = &r.volumes[0];
    assert!(approx(v.raw["vol_fill_rate_bytes_per_sec"], 0.0));
    assert!(!v.raw.contains_key("vol_time_to_full_secs"));
}

#[test]
fn parse_diskstats_sums_whole_disks_only() {
    let text = "\
   8       0 sda 100 0 2000 50 200 0 4000 80 0 700 130 0 0 0 0
   8       1 sda1 90 0 1800 40 190 0 3800 70 0 650 110 0 0 0 0
   7       0 loop0 5 0 40 1 0 0 0 0 0 1 1 0 0 0 0
 259       0 nvme0n1 10 0 100 5 20 0 300 9 0 900 14 0 0 0 0
";
    let whole = |n: &str| matches!(n, "sda" | "nvme0n1" | "loop0");
    let d = parse_diskstats(text, whole).unwrap();
    assert_eq!(d.read_bytes, (2000 + 100) * 512);
    assert_eq!(d.write_bytes, (4000 + 300) * 512);
    assert_eq!(d.io_ms_max, 900);
    assert_eq!(parse_diskstats("", whole), None);
}

#[test]
fn parse_net_dev_skips_loopback() {
    let text = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 999 1 0 0 0 0 0 0 999 1 0 0 0 0 0 0
  eth0: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0
  eth1: 5 1 0 0 0 0 0 0 7 1 0 0 0 0 0 0
";
    let n = parse_net_dev(text).unwrap();
    assert_eq!((n.rx_bytes, n.tx_bytes), (1005, 2007));
    assert_eq!(parse_net_dev("garbage"), None);
}

#[test]
fn volume_free_bytes_reads_an_existing_ancestor_and_none_for_nothing() {
    let dir = std::env::temp_dir()
        .join("loom-host-status-test-nonexistent")
        .join("a")
        .join("b");
    assert!(volume_free_bytes(&dir).is_some(), "walks up to the existing /tmp");
}

/// Serializes stub write + spawn: a concurrent fork that inherits the stub's
/// still-open write fd makes `exec` fail with ETXTBSY.
#[cfg(target_os = "linux")]
static STUB_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(target_os = "linux")]
fn stub_nvidia_smi(name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("loom-nvsmi-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nvidia-smi");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(target_os = "linux")]
#[test]
fn query_nvidia_smi_times_out_and_returns_none() {
    let _guard = STUB_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let stub = stub_nvidia_smi("hang", "exec sleep 30");
    let start = std::time::Instant::now();
    let got = query_nvidia_smi(stub.to_str().unwrap(), std::time::Duration::from_millis(300));
    assert!(matches!(got, Ok(None)), "{got:?}");
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}

#[cfg(target_os = "linux")]
#[test]
fn query_nvidia_smi_parses_stub_output() {
    let _guard = STUB_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let stub = stub_nvidia_smi("ok", "echo '30, 1000, 16000'");
    let g = query_nvidia_smi(stub.to_str().unwrap(), std::time::Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(g.mem_total_bytes, 16000 * 1024 * 1024);
}

#[cfg(target_os = "linux")]
#[test]
fn query_nvidia_smi_nonzero_exit_and_missing_binary() {
    let _guard = STUB_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let stub = stub_nvidia_smi("fail", "echo '30, 1000, 16000'; exit 3");
    let got = query_nvidia_smi(stub.to_str().unwrap(), std::time::Duration::from_secs(5));
    assert!(matches!(got, Ok(None)));
    let err =
        query_nvidia_smi("/nonexistent/nvidia-smi", std::time::Duration::from_secs(1)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}
