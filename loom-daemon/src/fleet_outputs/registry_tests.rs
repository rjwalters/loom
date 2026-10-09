//! `singleton_registry`: every one-host fleet job in the source tree, whether
//! a named singleton job (`arm_singleton_job` / `record_owned_singleton_job`)
//! or elected by the ETA authority, has a registry row or an explicit,
//! reasoned exemption.

use super::*;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Files (relative to `src`) that own a named singleton job, i.e. call
/// `arm_singleton_job(` or `record_owned_singleton_job(`. A new owning file
/// must be added here *and* its job registered.
const OWNING_FILES: &[&str] = &[
    "ci_telemetry/mod.rs",
    "fleet_alert/output_feed.rs",
    "intake_reconcile/singleton.rs",
    "observability/captain_gauges.rs",
    "observability/eta_fleet_refresh.rs",
    "observability/eta_nightly_folds.rs",
];

/// Every `.rs` under `src` as (relative path with `/`, contents), skipping
/// `fleet_captain.rs` (the definition and its throwaway test jobs) and this
/// module.
fn sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&src, &mut files);
    files
        .into_iter()
        .map(|p| {
            let rel = p.strip_prefix(&src).unwrap();
            let rel = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            (rel, std::fs::read_to_string(&p).unwrap())
        })
        .filter(|(rel, _)| rel != "fleet_captain.rs" && !rel.starts_with("fleet_outputs/"))
        .collect()
}

/// A call that owns a named singleton job.
const OWNS: &str = r"\b(?:arm|record_owned)_singleton_job\(";

fn re(s: &str) -> regex::Regex {
    regex::Regex::new(s).unwrap()
}

/// Singleton job names in source: every `*JOB_NAME` const (not only
/// `SINGLETON_JOB_NAME`: `eta::retire_filing::JOB_NAME` gates on the captain
/// too), the `captain_gauges` `*_JOB` consts, and string literals at call
/// sites.
fn discovered_jobs(srcs: &[(String, String)]) -> BTreeSet<String> {
    let konst = re(r#"const (?:[A-Z]+_)*JOB_NAME: &str = "([^"]+)""#);
    let gauge = re(r#"const [A-Z_]+_JOB: &str = "([^"]+)""#);
    let literal = re(r#"\b(?:arm|record_owned)_singleton_job\(\s*"([^"]+)""#);
    let mut found = BTreeSet::new();
    for (rel, text) in srcs {
        let mut grab =
            |r: &regex::Regex| found.extend(r.captures_iter(text).map(|c| c[1].to_string()));
        grab(&konst);
        grab(&literal);
        if rel == "observability/captain_gauges.rs" {
            grab(&gauge);
        }
    }
    found
}

#[test]
fn singleton_registry_owning_files_are_known() {
    let call = re(OWNS);
    let found: BTreeSet<String> = sources()
        .into_iter()
        .filter(|(_, t)| call.is_match(t))
        .map(|(rel, _)| rel)
        .collect();
    let listed: BTreeSet<String> = OWNING_FILES.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(
        found, listed,
        "singleton job owning call sites changed: register the new job in \
         fleet_outputs::SINGLETON_OUTPUTS (or EXEMPT) and update OWNING_FILES"
    );
}

/// `eta::job_owner` (#10918) hands the ETA jobs to the explicit authority;
/// whoever consults it outside tests must record the job by name, which the
/// registry then covers.
#[test]
fn singleton_registry_job_owner_callers_record_a_named_job() {
    let owner = re(r"\bjob_owner::resolve(_with)?\(");
    let owns = re(OWNS);
    let unrecorded: Vec<String> = sources()
        .into_iter()
        .filter(|(rel, t)| {
            !rel.contains("/tests/") && !rel.ends_with("_tests.rs") && owner.is_match(t)
        })
        .filter(|(_, t)| !owns.is_match(t))
        .map(|(rel, _)| rel)
        .collect();
    assert!(unrecorded.is_empty(), "job_owner callers owning no named job: {unrecorded:?}");
}

#[test]
fn singleton_registry_covers_every_singleton_job() {
    let jobs = discovered_jobs(&sources());
    assert!(jobs.len() >= 8, "scan found too few jobs: {jobs:?}");
    assert!(jobs.contains(crate::eta::retire_filing::JOB_NAME), "{jobs:?}");
    let known: BTreeSet<&str> = SINGLETON_OUTPUTS
        .iter()
        .filter(|o| o.gate == Gate::SingletonJob)
        .map(|o| o.job)
        .chain(EXEMPT.iter().map(|(j, _)| *j))
        .collect();
    let missing: Vec<_> = jobs
        .iter()
        .filter(|j| !known.contains(j.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "singleton jobs with no SINGLETON_OUTPUTS row or EXEMPT entry: {missing:?}"
    );
    let stale: Vec<_> = known.iter().filter(|j| !jobs.contains(**j)).collect();
    assert!(stale.is_empty(), "registry rows for jobs no longer in source: {stale:?}");
}

#[test]
fn singleton_registry_covers_every_authority_site() {
    let call = re(r"\bauthority::resolve(_with)?\(");
    let found: BTreeSet<String> = sources()
        .into_iter()
        .filter(|(rel, t)| rel != "eta/authority.rs" && call.is_match(t))
        .map(|(rel, _)| rel)
        .collect();
    let listed: BTreeSet<String> = AUTHORITY_SITES
        .iter()
        .map(|(f, _, _)| (*f).to_string())
        .collect();
    assert_eq!(
        found, listed,
        "eta::authority::resolve call sites changed: map each to a Gate::EtaAuthority row (or None + reason) \
         in fleet_outputs::AUTHORITY_SITES"
    );
    let rows: BTreeSet<&str> = SINGLETON_OUTPUTS
        .iter()
        .filter(|o| o.gate == Gate::EtaAuthority)
        .map(|o| o.record_kind)
        .collect();
    let gated: BTreeSet<&str> = AUTHORITY_SITES.iter().filter_map(|(_, k, _)| *k).collect();
    assert_eq!(
        rows, gated,
        "every authority row needs a gating site and every gated kind a row"
    );
    for (file, _, reason) in AUTHORITY_SITES {
        assert!(!reason.trim().is_empty(), "authority site {file} needs a reason");
    }
}

#[test]
fn singleton_registry_rows_are_well_formed() {
    let mut seen = BTreeSet::new();
    for o in SINGLETON_OUTPUTS {
        assert!(o.cadence > Duration::ZERO, "{} cadence is zero", o.record_kind);
        assert!(seen.insert((o.job, o.record_kind)), "duplicate row {}:{}", o.job, o.record_kind);
        assert_eq!(
            o.gate == Gate::EtaAuthority,
            o.job == ETA_AUTHORITY,
            "{}: gate/job mismatch",
            o.record_kind
        );
    }
    for o in SINGLETON_OUTPUTS {
        assert!(
            !o.enabled_by.is_empty() && o.enabled_by.iter().all(|k| !k.trim().is_empty()),
            "{}: every registered job is config-gated; name its toggles",
            o.record_kind
        );
    }
    for (job, reason) in EXEMPT {
        assert!(!reason.trim().is_empty(), "exempt {job} needs a reason");
        assert!(
            SINGLETON_OUTPUTS.iter().all(|o| o.job != *job),
            "{job} is both exempt and registered"
        );
    }
}
