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
    }
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

// ---- #12114: admission readers see the filtered record ----

#[test]
fn filtered_readers_smooth_a_one_sample_load_spike() {
    let _slot = TEST_SLOT.lock().unwrap_or_else(PoisonError::into_inner);
    let mut s = HostStatusSampler::new(HostStatusConfig::default());
    // Six calm ticks at 0.5/core, then one 2.0/core spike one tick later (the old raw gate
    // threshold is 0.9/core).
    let mut last = None;
    for i in 0..6 {
        last = Some(s.step(&readings(), f64::from(i) * 5.0));
    }
    let mut spike = readings();
    spike.load_per_core = Some(2.0);
    let record = s.step(&spike, 30.0);
    publish(&record);
    let raw = latest_raw("cpu_load_per_core").unwrap();
    let filtered = filtered_load_per_core().unwrap();
    clear_latest();
    assert!(last.is_some());
    assert!(raw >= 0.9, "a raw read trips the 0.9 gate threshold: {raw}");
    assert!(filtered < 0.9, "the filtered read does not: {filtered}");
    assert!(filtered > 0.5, "but it does move toward the spike: {filtered}");
}

#[test]
fn filtered_readers_are_absent_until_published_and_never_zero() {
    let _slot = TEST_SLOT.lock().unwrap_or_else(PoisonError::into_inner);
    clear_latest();
    assert_eq!(latest_filtered("cpu_load_per_core"), None);
    assert_eq!(filtered_load_per_core(), None);
    assert_eq!(filtered_loadavg_1m(8), None);
    assert_eq!(filtered_cpu_idle_fraction(), None);
}

#[test]
fn filtered_loadavg_and_idle_are_derived_from_the_filtered_signals() {
    let _slot = TEST_SLOT.lock().unwrap_or_else(PoisonError::into_inner);
    let mut s = HostStatusSampler::new(HostStatusConfig::default());
    s.step(&readings(), 0.0);
    let mut b = readings();
    b.cpu = Some(CpuCounters {
        idle: 150,
        total: 300,
    });
    publish(&s.step(&b, 5.0));
    let lpc = filtered_load_per_core().unwrap();
    assert!(approx(filtered_loadavg_1m(8).unwrap(), lpc * 8.0));
    // 50% utilisation over the second tick: idle is its complement.
    assert!(approx(filtered_cpu_idle_fraction().unwrap(), 0.5));
    clear_latest();
}
