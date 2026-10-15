//! Tests for [`super`]: the systemd unit carries no autonomy env (#11951).

use super::*;
use crate::daemon_start::autonomy::{warn_autonomy_downgrade, DowngradeCheck, PriorAutonomy};
use crate::daemon_start::envh::forwarded_env_pairs_from;
use crate::daemon_start::render::{
    launchd_plist, systemd_env_keys, systemd_unit, warn_dropped_env_keys, Mechanism,
};

/// The start script's process env after a bare start resolved its FLAGS-OFF
/// defaults, plus the path keys it exports and one ordinary forwarded key:
/// the shape that reached the worker units on 2026-10-14.
const POLLUTED_ENV: &str = "HOME=/home/u\n\
    LOOM_WORKSPACE=/home/u/loom-workspaces/loom\n\
    LOOM_PID_FILE=/home/u/.loom/.daemon.pid\n\
    LOOM_GUARD_DECISION_LOG=1\n\
    LOOM_FORCE_SCOPE=protected\n\
    LOOM_WORK_FINDER=0\n\
    LOOM_MAIN_HEALTH_GATE=0\n\
    LOOM_SAFEHOUSE_ENABLED=1\n\
    GH_TOKEN=redacted\n";

fn unit_from(env: &str) -> String {
    systemd_unit("/b", "/w", "/l", "/p", "/h", &forwarded_env_pairs_from(env))
}

fn plist_from(env: &str) -> String {
    launchd_plist("lbl", "/b", "/w", "/l", "/p", "/h", &forwarded_env_pairs_from(env))
}

#[test]
fn a_polluted_env_yields_a_unit_without_autonomy_keys() {
    let u = unit_from(POLLUTED_ENV);
    let keys = systemd_env_keys(&u);
    for k in UNRENDERED_KEYS {
        assert!(!keys.iter().any(|x| x == k), "unit carries {k}:\n{u}");
        assert!(!u.contains(k), "unit mentions {k}:\n{u}");
    }
    // Everything else the shell forwarded still lands in the unit.
    for k in [
        "PATH",
        "HOME",
        "LOOM_DAEMON_SUPERVISOR",
        "LOOM_WORKSPACE",
        "LOOM_PID_FILE",
        "LOOM_SAFEHOUSE_ENABLED",
        "GH_TOKEN",
    ] {
        assert!(keys.iter().any(|x| x == k), "unit lost {k}:\n{u}");
    }
}

#[test]
fn an_explicit_on_is_not_rendered_either() {
    // `--work-finder` exports `1`; the unit still does not carry it, which is
    // why the start refuses when nothing else turns the loop on.
    let u = unit_from("LOOM_WORK_FINDER=1\nLOOM_MAIN_HEALTH_GATE=1\n");
    assert!(!u.contains("LOOM_WORK_FINDER"));
    assert!(!u.contains("LOOM_MAIN_HEALTH_GATE"));
}

#[test]
fn the_plist_is_unchanged() {
    // launchd is out of scope: the plist carries every autonomy key, in the
    // order the env listed it, exactly as before #11951.
    let p = plist_from(POLLUTED_ENV);
    let expected_env = "        <key>PATH</key>\n        <string>/p</string>\n\
        <key>HOME</key>\n        <string>/h</string>\n\
        <key>LOOM_DAEMON_SUPERVISOR</key>\n        <string>launchd</string>\n\
        <key>LOOM_WORKSPACE</key>\n        <string>/home/u/loom-workspaces/loom</string>\n\
        <key>LOOM_PID_FILE</key>\n        <string>/home/u/.loom/.daemon.pid</string>\n\
        <key>LOOM_GUARD_DECISION_LOG</key>\n        <string>1</string>\n\
        <key>LOOM_FORCE_SCOPE</key>\n        <string>protected</string>\n\
        <key>LOOM_WORK_FINDER</key>\n        <string>0</string>\n\
        <key>LOOM_MAIN_HEALTH_GATE</key>\n        <string>0</string>\n\
        <key>LOOM_SAFEHOUSE_ENABLED</key>\n        <string>1</string>\n\
        <key>GH_TOKEN</key>\n        <string>redacted</string>\n";
    let expected_env = expected_env.replace("\n<key>", "\n        <key>");
    let want =
        format!("    <key>EnvironmentVariables</key>\n    <dict>\n{expected_env}    </dict>\n");
    assert!(p.contains(&want), "plist env block changed:\n{p}");
}

#[test]
fn an_installed_units_autonomy_lines_are_purged_not_carried_forward() {
    let dir = tempfile::tempdir().expect("tempdir");
    let old = dir.path().join("loom-daemon.service");
    // A pre-#11951 unit, written the way the old renderer wrote it.
    let mut installed = systemd_unit("/b", "/w", "/l", "/p", "/h", &[]);
    installed = installed.replace(
        "Environment=LOOM_DAEMON_SUPERVISOR=systemd\n",
        "Environment=LOOM_DAEMON_SUPERVISOR=systemd\n\
         Environment=LOOM_WORK_FINDER=0\nEnvironment=LOOM_FORCE_SCOPE=protected\n\
         Environment=LOOM_SAFEHOUSE_ENABLED=1\n",
    );
    std::fs::write(&old, &installed).expect("write");
    let new = systemd_unit("/b", "/w", "/l", "/p", "/h", &[]);
    for force_env in [false, true] {
        let merged =
            warn_dropped_env_keys(&old, "/tmp/new", &new, Mechanism::Systemd, force_env).new_text;
        assert!(!merged.contains("LOOM_WORK_FINDER"), "carried forward:\n{merged}");
        assert!(!merged.contains("LOOM_FORCE_SCOPE"), "carried forward:\n{merged}");
        // An ordinary key keeps the #5344 carry unless --force-env.
        assert_eq!(merged.contains("Environment=LOOM_SAFEHOUSE_ENABLED=1"), !force_env);
    }
}

#[test]
fn the_plist_merge_still_carries_autonomy_forward() {
    let dir = tempfile::tempdir().expect("tempdir");
    let old = dir.path().join("old.plist");
    std::fs::write(&old, plist_from("LOOM_WORK_FINDER=1\n")).expect("write");
    let new = plist_from("");
    let merged = warn_dropped_env_keys(&old, "/tmp/new", &new, Mechanism::Launchd, false).new_text;
    assert!(merged.contains("<key>LOOM_WORK_FINDER</key>\n        <string>1</string>"));
}

#[test]
fn environment_lines_parse_like_systemd() {
    assert_eq!(
        parse_environment(r#"A=1 "B=two words" 'C=x y' D=a\ b NOVALUE"#),
        vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "two words".to_string()),
            ("C".to_string(), "x y".to_string()),
            ("D".to_string(), "a b".to_string()),
        ]
    );
}

fn write_dropin(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir).expect("mkdir");
    std::fs::write(dir.join(name), body).expect("write");
}

#[test]
fn dropins_apply_in_name_order_and_an_empty_assignment_resets() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let d = tmp.path().join("loom-daemon.service.d");
    assert_eq!(dropin_value(&d, "LOOM_WORK_FINDER"), None, "no directory");
    write_dropin(&d, "10-layout.conf", "[Service]\nEnvironment=LOOM_WORKSPACE=/w\n");
    write_dropin(
        &d,
        "15-autonomy.conf",
        "[Service]\nEnvironment=LOOM_WORK_FINDER=1 LOOM_FORCE_SCOPE=protected\n",
    );
    assert_eq!(dropin_value(&d, "LOOM_WORK_FINDER").as_deref(), Some("1"));
    write_dropin(&d, "20-off.conf", "[Service]\nEnvironment=\"LOOM_WORK_FINDER=0\"\n");
    assert_eq!(dropin_value(&d, "LOOM_WORK_FINDER").as_deref(), Some("0"));
    write_dropin(&d, "30-reset.conf", "[Service]\nEnvironment=\n");
    assert_eq!(dropin_value(&d, "LOOM_WORK_FINDER"), None);
    write_dropin(&d, "99-ignored.txt", "Environment=LOOM_WORK_FINDER=1\n");
    assert_eq!(dropin_value(&d, "LOOM_WORK_FINDER"), None, "only *.conf counts");
}

fn layer(dropin: [Option<&str>; 2], config: [Option<bool>; 2]) -> Layer {
    Layer {
        dropin_dir: PathBuf::from("/h/.config/systemd/user/loom-daemon.service.d"),
        dropin: dropin.map(|v| v.map(ToString::to_string)),
        config,
    }
}

#[test]
fn the_layer_reads_dropin_before_config() {
    let l = layer([Some("0"), None], [Some(true), Some(true)]);
    assert!(!l.on_after_render("LOOM_WORK_FINDER"), "a drop-in 0 beats config");
    assert!(l.on_after_render("LOOM_MAIN_HEALTH_GATE"), "config decides with no drop-in");
    assert!(l.decides("LOOM_WORK_FINDER"));
    let none = layer([None, None], [None, Some(false)]);
    assert!(!none.on_after_render("LOOM_WORK_FINDER"), "both loops default off");
    assert!(!none.decides("LOOM_WORK_FINDER"));
    assert!(!none.decides("LOOM_MAIN_HEALTH_GATE"));
}

#[test]
fn an_unmet_ask_is_refused_and_a_met_one_is_not() {
    let nothing = layer([None, None], [None, None]);
    assert!(unmet_intent(&nothing, [(Want::On, ""), (Want::Unset, "")]));
    assert!(unmet_intent(&nothing, [(Want::Unset, "1"), (Want::Unset, "")]), "exported 1");
    assert!(unmet_intent(&nothing, [(Want::Unset, ""), (Want::On, "")]), "--health-gate");
    assert!(!unmet_intent(&nothing, [(Want::Unset, ""), (Want::Unset, "")]), "a bare start");
    assert!(!unmet_intent(&nothing, [(Want::Off, "1"), (Want::Unset, "0")]), "asks for off");
    let wf_on = layer([Some("1"), None], [None, None]);
    assert!(!unmet_intent(&wf_on, [(Want::On, ""), (Want::Unset, "")]));
    let cfg_on = layer([None, None], [Some(true), None]);
    assert!(!unmet_intent(&cfg_on, [(Want::On, ""), (Want::Unset, "")]));
    let dropin_off = layer([Some("0"), None], [Some(true), None]);
    assert!(unmet_intent(&dropin_off, [(Want::On, ""), (Want::Unset, "")]));
}

fn check(
    l: &Layer,
    marker: &Path,
    prior: &PriorAutonomy,
    from_config: bool,
    want: Want,
    wf: (&str, &str),
) -> bool {
    warn_autonomy_downgrade(&DowngradeCheck {
        from_config,
        work_finder: wf,
        health_gate: ("0", ""),
        want_work_finder: want,
        want_health_gate: Want::Unset,
        prior,
        intent_marker: marker,
        config_work_finder: l.config[0],
        systemd: Some(l),
    })
}

#[test]
fn the_guard_reads_the_dropin_not_the_base_unit_or_a_stale_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let unit = tmp.path().join("loom-daemon.service");
    let marker = tmp.path().join("autonomy-desired");
    // The worker-4/5 shape: a stale marker and base unit both say 0 or 1,
    // and the spec's drop-in turns the work finder on.
    std::fs::write(&unit, "[Service]\nEnvironment=LOOM_WORK_FINDER=1\n").expect("unit");
    std::fs::write(&marker, "work_finder=0\nhealth_gate=0\n").expect("marker");
    let prior = PriorAutonomy {
        file: Some(unit.clone()),
        mechanism: Some(Mechanism::Systemd),
    };
    let spec = layer([Some("1"), Some("0")], [None, None]);
    assert!(!check(&spec, &marker, &prior, false, Want::Unset, ("0", "")), "a plain start");

    // No drop-in and no config: purging the base unit's `1` is a downgrade.
    let bare = layer([None, Some("0")], [None, None]);
    assert!(check(&bare, &marker, &prior, false, Want::Unset, ("0", "")));
    assert!(!check(&bare, &marker, &prior, false, Want::Off, ("0", "")), "--no-work-finder");
    assert!(!check(&bare, &marker, &prior, true, Want::Unset, ("", "")), "--from-config");

    // The config enables it: the start's `0` is never written, so it is not
    // the #11934 config downgrade either.
    let cfg = layer([None, Some("0")], [Some(true), None]);
    assert!(!check(&cfg, &marker, &prior, false, Want::Unset, ("0", "")));

    // `--from-config --work-finder` with nothing to honour it is still refused.
    assert!(check(&bare, &marker, &prior, true, Want::On, ("1", "")));
}

#[test]
fn the_marker_records_what_the_daemon_runs_with() {
    let spec = layer([Some("1"), None], [None, Some(true)]);
    // No readable /proc entry for this pid on any host.
    let pid = Some("999999999");
    assert_eq!(effective_marker_value(pid, &spec, "LOOM_WORK_FINDER"), "1");
    assert_eq!(effective_marker_value(pid, &spec, "LOOM_MAIN_HEALTH_GATE"), "1");
    let nothing = layer([None, None], [None, None]);
    assert_eq!(effective_marker_value(None, &nothing, "LOOM_WORK_FINDER"), "0");
}

#[cfg(target_os = "linux")]
#[test]
fn the_marker_prefers_the_running_daemons_own_env() {
    // This test process stands in for the daemon: its exec-time env has PATH.
    let pid = std::process::id().to_string();
    assert!(proc_env_value(&pid, "PATH").is_some_and(|v| !v.is_empty()));
    assert_eq!(proc_env_value("not-a-pid", "PATH"), None);
    let dropin_off = layer([Some("0"), None], [None, None]);
    let want = std::env::var("PATH").unwrap_or_default();
    assert_eq!(effective_marker_value(Some(&pid), &dropin_off, "PATH"), want);
}
