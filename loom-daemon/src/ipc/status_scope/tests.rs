//! Tests for section-scoped status builds (Issue #10787).

#![allow(clippy::unwrap_used)]

use super::*;
use crate::status_section::StatusSection;
use crate::workspace_registry::REGISTRY_PATH_ENV;

fn runtime_handle() -> tokio::runtime::Handle {
    use std::sync::OnceLock;
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().unwrap())
        .handle()
        .clone()
}

fn credentials() -> CredentialPreflightReport {
    CredentialPreflightReport {
        ok: true,
        mechanism: "test-fixture".to_string(),
        fingerprint: None,
        message: "test fixture".to_string(),
        checked_at: Utc::now(),
        pool: None,
    }
}

/// A registry of `n` repos plus an unregistered daemon workspace (the
/// fallback root), with the registry env pointed at it until drop.
struct ManyRoots {
    _dir: tempfile::TempDir,
    roots: Vec<PathBuf>,
    fallback: PathBuf,
}

impl ManyRoots {
    fn new(n: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = WorkspaceRegistry::default();
        let roots: Vec<PathBuf> = (0..n)
            .map(|i| {
                let root = dir.path().join(format!("repo-{i:02}"));
                std::fs::create_dir_all(root.join(".loom")).unwrap();
                reg.add(&root, None).unwrap();
                // The pool keys registries by canonical path (macOS temp
                // dirs resolve through `/private`).
                root.canonicalize().unwrap()
            })
            .collect();
        let reg_path = dir.path().join("workspaces.json");
        reg.save(&reg_path).unwrap();
        std::env::set_var(REGISTRY_PATH_ENV, &reg_path);
        let fallback = dir.path().join("daemon-workspace");
        std::fs::create_dir_all(fallback.join(".loom")).unwrap();
        Self {
            _dir: dir,
            roots,
            fallback,
        }
    }

    fn pool() -> Arc<WorkspacePool> {
        Arc::new(WorkspacePool::new(
            Arc::new(crate::event_bus::EventBus::new()),
            runtime_handle(),
        ))
    }

    fn build(&self, pool: &Arc<WorkspacePool>, sections: &SectionSet) -> DaemonStatusReport {
        super::super::build_daemon_status_for(
            pool,
            &WorkspaceHealthStates::new(),
            &self.fallback,
            &credentials(),
            sections,
        )
    }

    /// How many registered roots have had their sweep registry provisioned —
    /// the first step of the per-root `registry_lock/list` phase.
    fn walked(&self, pool: &WorkspacePool) -> usize {
        self.roots
            .iter()
            .filter(|root| pool.provisioned_registry_for(root).is_some())
            .count()
    }
}

impl Drop for ManyRoots {
    fn drop(&mut self) {
        std::env::remove_var(REGISTRY_PATH_ENV);
    }
}

const ROOTS: usize = 40;

/// **#10787 AC.** `--section daemon_build,auto_update` must not run the
/// per-root phases at all — not merely produce a small output. The pool's
/// provisioned registries are the witness: the per-root loop's first act on
/// a root is `get_or_provision`, so a root the loop never visited has none.
/// The full build on the same pool afterwards proves the fixture's roots are
/// really walked when a section needs them.
#[test]
#[serial_test::serial]
fn a_daemon_build_and_auto_update_build_walks_no_registered_root() {
    let fixture = ManyRoots::new(ROOTS);
    let pool = ManyRoots::pool();

    let sections = SectionSet::only([StatusSection::DaemonBuild, StatusSection::AutoUpdate]);
    let report = fixture.build(&pool, &sections);
    assert_eq!(fixture.walked(&pool), 0, "a sectioned build walked registered roots");
    assert!(report.per_repo.is_empty());
    assert_eq!(
        report.daemon_build_commit.as_deref(),
        Some(crate::self_update::BUILT_COMMIT),
        "the requested section is still built"
    );
    // Nor the machine-level tail: no registry at all was provisioned (not
    // even the fallback root's, for the pre-flight advisory), and the token
    // pool, headroom (`df` / `vm_stat`), work-finder config, shard posture
    // and forge-call window were never read.
    assert!(pool.is_empty(), "a sectioned build provisioned a registry");
    assert_eq!(report.token_pool_dir, None);
    assert_eq!((report.disk_headroom, report.ram_headroom, report.dynamic_cap), (0, 0, 0));
    assert_eq!(report.work_finder_enabled, None);
    assert!(report.role_runner_shard.is_none() && report.forge_calls.is_none());

    let full = fixture.build(&pool, &SectionSet::all());
    assert_eq!(fixture.walked(&pool), ROOTS, "the full build walks every root");
    assert_eq!(full.per_repo.len(), ROOTS);
    assert!(full.token_pool_dir.is_some() && full.work_finder_enabled.is_some());
    assert!(full.role_runner_shard.is_some() && full.forge_calls.is_some());
}

/// The in-flight union needs every root's registry snapshot but none of the
/// per-repo detail phases, so it walks the roots without building rows.
#[test]
#[serial_test::serial]
fn an_in_flight_build_reads_every_registry_but_builds_no_per_repo_rows() {
    let fixture = ManyRoots::new(ROOTS);
    let pool = ManyRoots::pool();

    let report = fixture.build(&pool, &SectionSet::only([StatusSection::InFlight]));
    assert_eq!(fixture.walked(&pool), ROOTS);
    assert!(report.per_repo.is_empty(), "per-repo rows built for an in_flight-only request");

    let report = fixture.build(&pool, &SectionSet::only([StatusSection::PerRepo]));
    assert_eq!(report.per_repo.len(), ROOTS);
}

#[test]
fn only_status_requests_carry_sections() {
    assert_eq!(requested_sections(&Request::DaemonStatus), Some(SectionSet::all()));
    let request = Request::DaemonStatusSections {
        sections: vec![StatusSection::AutoUpdate, StatusSection::DaemonBuild],
    };
    assert_eq!(
        requested_sections(&request),
        Some(SectionSet::only([StatusSection::DaemonBuild, StatusSection::AutoUpdate]))
    );
    // An all-sections request is the full build, the same key as `DaemonStatus`.
    let every = Request::DaemonStatusSections {
        sections: StatusSection::all().to_vec(),
    };
    assert_eq!(requested_sections(&every), Some(SectionSet::all()));
    assert_eq!(requested_sections(&Request::Ping), None);
}

/// Each machine-level input is read only for a section that reports it.
#[test]
#[serial_test::serial]
fn machine_inputs_follow_the_sections_that_report_them() {
    let fixture = ManyRoots::new(2);
    let pool = ManyRoots::pool();

    let report = fixture.build(&pool, &SectionSet::only([StatusSection::WorkFinder]));
    assert!(report.work_finder_enabled.is_some());
    assert_eq!(report.token_pool_dir, None, "work_finder does not read the token pool");

    let report = fixture.build(&pool, &SectionSet::only([StatusSection::TokenUsage]));
    assert!(report.token_pool_dir.is_some(), "the CLI's token probe needs the pool dir");
    assert_eq!(report.work_finder_enabled, None);

    let report = fixture.build(&pool, &SectionSet::only([StatusSection::CapacityBound]));
    let full = fixture.build(&pool, &SectionSet::all());
    assert_eq!(report.dynamic_cap, full.dynamic_cap);
    assert_eq!(report.capacity_bound, full.capacity_bound);
    assert_eq!(report.configured_max, full.configured_max);
}

/// **#10879 review nit.** A sectioned `drain` build with a drain active and
/// a sweep in flight: `drain` walks the roots for the in-flight count, so
/// the roll projection names the sweep it is waiting on rather than a
/// silent zero, without building any per-repo row.
#[test]
#[serial_test::serial]
fn a_sectioned_drain_build_counts_the_sweeps_an_active_drain_waits_on() {
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let spawn_bin = root.join(".loom").join("scripts").join("spawn-claude.sh");
    std::fs::create_dir_all(spawn_bin.parent().unwrap()).unwrap();
    std::fs::write(&spawn_bin, "#!/usr/bin/env bash\nexit 0\n").unwrap();
    std::fs::set_permissions(&spawn_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = SweepRegistryConfig::new(root.clone());
    config.spawn_bin = Some(spawn_bin);
    config.skip_label_flip = true;
    config.journal_path = Some(root.join("test-sweeps-journal.json"));
    let registry = Arc::new(Mutex::new(SweepRegistry::new(config)));
    std::env::set_var(REGISTRY_PATH_ENV, root.join("no-such-workspaces.json"));
    let pool = ManyRoots::pool();
    pool.seed(root.clone(), registry.clone());
    registry
        .lock()
        .unwrap()
        .dispatch(&crate::types::SweepKind::Issue(10861), None, None, None, None)
        .expect("dispatch");

    let drain = DrainState::new();
    let deadline = match drain.begin(std::time::Duration::from_secs(300), false, false) {
        super::super::DrainBegin::Started { deadline, .. } => deadline,
        other => panic!("expected Started, got {other:?}"),
    };
    let report = build_daemon_status_with_drain(
        &pool,
        &WorkspaceHealthStates::new(),
        &root,
        &credentials(),
        &drain,
        &SectionSet::only([StatusSection::Drain]),
    );
    std::env::remove_var(REGISTRY_PATH_ENV);

    assert!(report.draining);
    assert_eq!(report.drain_deadline, Some(deadline));
    let roll = report.drain_roll.expect("an active drain reports its roll");
    assert_eq!(roll.in_flight, 1, "the roll projection lost the sweep it is waiting on");
    assert!(report.per_repo.is_empty(), "a drain-only build built per-repo rows");
}

/// #10861: the overlay alone — what each request applies to its copy of a
/// shared build — carries the whole drain block, and clears it again.
#[test]
fn overlay_drain_tracks_the_live_drain_state() {
    let drain = DrainState::new();
    let mut report = DaemonStatusReport::default();
    overlay_drain(&mut report, &drain);
    assert!(!report.draining && report.drain_deadline.is_none() && report.drain_roll.is_none());

    let deadline = match drain.begin(std::time::Duration::from_secs(300), false, false) {
        super::super::DrainBegin::Started { deadline, .. } => deadline,
        other => panic!("expected Started, got {other:?}"),
    };
    overlay_drain(&mut report, &drain);
    assert!(report.draining);
    assert_eq!(report.drain_deadline, Some(deadline));
    assert_eq!(report.drain_roll.as_ref().map(|roll| roll.in_flight), Some(0));

    assert!(drain.abort());
    overlay_drain(&mut report, &drain);
    assert!(!report.draining && report.drain_roll.is_none());
}
