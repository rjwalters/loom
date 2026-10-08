//! Tests for the `status --section` list (Issue #10787).

#![allow(clippy::unwrap_used)]

use super::*;
use clap::ValueEnum;

/// The serde (wire) name, the clap (`--section`) name and [`StatusSection::as_str`]
/// are one string per section, and `FromStr` inverts it.
#[test]
fn wire_cli_and_display_names_agree_for_every_section() {
    for section in StatusSection::all() {
        let wire = serde_json::to_value(section).unwrap();
        assert_eq!(wire, serde_json::json!(section.as_str()), "serde name of {section:?}");
        let cli = section.to_possible_value().unwrap();
        assert_eq!(cli.get_name(), section.as_str(), "clap name of {section:?}");
        assert_eq!(section.as_str().parse::<StatusSection>().unwrap(), *section);
        assert!(!section.json_keys().is_empty(), "{section:?} selects no key");
    }
}

/// An unknown name is rejected with the full list of valid names.
#[test]
fn an_unknown_section_names_every_valid_one() {
    let err = "daemon_biuld".parse::<StatusSection>().unwrap_err();
    assert!(
        err.starts_with("unknown status section 'daemon_biuld'; valid sections: "),
        "{err}"
    );
    for section in StatusSection::all() {
        assert!(err.contains(section.as_str()), "{err} omits {section}");
    }
}

/// No payload key belongs to two sections, so a selection is unambiguous.
#[test]
fn no_payload_key_belongs_to_two_sections() {
    let mut seen = std::collections::BTreeMap::new();
    for section in StatusSection::all() {
        for key in section.json_keys() {
            if let Some(prev) = seen.insert(*key, *section) {
                panic!("key {key} is in both {prev:?} and {section:?}");
            }
        }
    }
}

/// The fleet tooling's cheap query needs none of the `O(roots)` phases, the
/// CPU sample, or the token probe.
#[test]
fn daemon_build_and_auto_update_need_no_expensive_phase() {
    let set = SectionSet::only([StatusSection::DaemonBuild, StatusSection::AutoUpdate]);
    assert!(!set.walks_roots());
    assert!(!set.walks_root_detail());
    assert!(!set.needs_cpu_sample());
    assert!(!set.needs_token_probe());
    assert!(!set.needs_token_pool());
    assert!(!set.needs_host_headroom());
    assert!(!set.needs_work_finder_config());
    assert_eq!(set.when(StatusSection::ForgeCalls, || 1), None);
    assert_eq!(set.when(StatusSection::DaemonBuild, || 1), Some(1));
}

/// `machine_caps` relies on this: whenever the headroom terms are measured
/// the configured ceiling is resolved too, so `dynamic_cap` is their true min.
#[test]
fn host_headroom_implies_the_work_finder_config() {
    for section in StatusSection::all() {
        let set = SectionSet::only([*section]);
        assert!(
            !set.needs_host_headroom() || set.needs_work_finder_config(),
            "{section} measures headroom without the configured ceiling"
        );
    }
    let all = SectionSet::all();
    assert!(all.needs_token_pool() && all.needs_host_headroom() && all.needs_work_finder_config());
}

/// The per-repo rows need the whole per-root walk; the in-flight union needs
/// only the registry part of it.
#[test]
fn root_phases_follow_the_sections_that_read_them() {
    let in_flight = SectionSet::only([StatusSection::InFlight]);
    assert!(in_flight.walks_roots());
    assert!(!in_flight.walks_root_detail());
    for heavy in [
        StatusSection::PerRepo,
        StatusSection::Worktrees,
        StatusSection::Pipeline,
    ] {
        let set = SectionSet::only([heavy]);
        assert!(set.walks_roots() && set.walks_root_detail(), "{heavy:?}");
    }
    let all = SectionSet::all();
    assert!(all.walks_roots() && all.walks_root_detail());
    assert!(all.needs_cpu_sample() && all.needs_token_probe());
}

#[test]
fn retain_keys_is_a_no_op_for_the_full_set_and_filters_otherwise() {
    let full = serde_json::json!({
        "in_flight": [], "in_flight_count": 0, "daemon_build": {"stale": false}, "x": 1,
    });
    let mut value = full.clone();
    SectionSet::all().retain_keys(&mut value);
    assert_eq!(value, full);

    let mut value = full.clone();
    SectionSet::only([StatusSection::InFlight]).retain_keys(&mut value);
    assert_eq!(value, serde_json::json!({"in_flight": [], "in_flight_count": 0}));
}

#[test]
fn a_set_lists_its_sections_once_in_payload_order() {
    let set = SectionSet::only([
        StatusSection::AutoUpdate,
        StatusSection::DaemonBuild,
        StatusSection::AutoUpdate,
    ]);
    assert_eq!(
        set.sections().unwrap(),
        vec![StatusSection::DaemonBuild, StatusSection::AutoUpdate]
    );
    assert!(SectionSet::all().sections().is_none());
    assert!(SectionSet::all().is_all() && !set.is_all());
}

/// #10861 keys shared builds by this set: equal requests must be equal and
/// hash equally whatever their order or duplication, and naming every
/// section must be the full set, not a second spelling of it.
#[test]
fn the_set_is_normalized_so_it_can_key_a_shared_build() {
    use std::collections::HashSet;

    let a = SectionSet::only([StatusSection::AutoUpdate, StatusSection::DaemonBuild]);
    let b = SectionSet::only([
        StatusSection::DaemonBuild,
        StatusSection::AutoUpdate,
        StatusSection::DaemonBuild,
    ]);
    let every = SectionSet::only(StatusSection::all().iter().copied());
    assert_eq!(a, b);
    assert_eq!(every, SectionSet::all());
    assert!(every.is_all() && every.sections().is_none());

    let keys: HashSet<SectionSet> = [a, b, every, SectionSet::all()].into_iter().collect();
    assert_eq!(keys.len(), 2, "one key per distinct build");
    assert_ne!(SectionSet::only([]), SectionSet::all(), "an empty selection is not a full one");
}
