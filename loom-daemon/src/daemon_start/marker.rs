//! The autonomy-desired intent marker (#4011).
//!
//! Its LIFETIME is operator INTENT, not process liveness: only an
//! operator-initiated `loom-daemon-stop.sh` removes it, and it is deliberately
//! preserved across the internal stop `loom-daemon-update.sh` performs. The
//! watchdog reads it to decide whether a missing daemon is a silent failure
//! (marker present ⇒ report) or a deliberate stop (marker absent ⇒ stay quiet).

use std::path::Path;

/// Everything `write_intent_marker` records, so the watchdog can probe reality
/// without re-deriving any of it.
pub struct IntentMarker<'a> {
    pub repo_root: &'a Path,
    pub pid_file: &'a Path,
    pub heartbeat_file: &'a Path,
    pub heartbeat_interval_secs: &'a str,
    pub use_launchd: bool,
    pub launchd_label: &'a str,
    pub use_systemd: bool,
    pub systemd_unit: &'a str,
    pub socket_path: &'a Path,
}

/// `write_intent_marker <use_launchd> <label> [use_systemd] [unit]`.
///
/// `work_finder` / `health_gate` (#5437) persist THIS invocation's actual
/// resolved values. On the nohup fallback tier — which never renders a
/// plist/unit — they are the only durable record of "was the daemon most
/// recently started autonomously?", and without them every bare restart
/// following any prior start looked like a downgrade.
///
/// Written under `umask 077`: the marker records paths and a label, and the
/// shell chose 0600 deliberately.
pub fn write(loom_dir: &Path, marker_path: &Path, m: &IntentMarker) {
    let _ = std::fs::create_dir_all(loom_dir);
    let started_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let work_finder = std::env::var("LOOM_WORK_FINDER").unwrap_or_default();
    let health_gate = std::env::var("LOOM_MAIN_HEALTH_GATE").unwrap_or_default();
    // Read BEFORE the truncating write below: the prior values are the carry.
    let config = crate::config_resolver::resolve_effective_config(m.repo_root);
    let hang_recover = hang_recover_fields(
        |k| std::env::var(k).ok().filter(|v| !v.is_empty()),
        |k| config_scalar(&config, k),
        |k| crate::watchdog::marker::get_nonempty(marker_path, k),
    );

    let body = format!(
        "# loom autonomy-desired marker (issue #4011)\n\
         # Presence ⇒ a loom-daemon is EXPECTED to be running on this host. Written by\n\
         # loom-daemon-start.sh on a successful start; removed ONLY by an\n\
         # operator-initiated loom-daemon-stop.sh (preserved across update.sh restarts).\n\
         # Do not hand-edit — delete via loom-daemon-stop.sh so the watchdog stays quiet.\n\
         started_at={started_at}\n\
         repo_root={repo_root}\n\
         pid_file={pid_file}\n\
         heartbeat_file={heartbeat_file}\n\
         heartbeat_interval_secs={heartbeat_interval_secs}\n\
         use_launchd={use_launchd}\n\
         launchd_label={launchd_label}\n\
         use_systemd={use_systemd}\n\
         systemd_unit={systemd_unit}\n\
         socket_path={socket_path}\n\
         work_finder={work_finder}\n\
         health_gate={health_gate}\n\
         {hang_recover}",
        repo_root = m.repo_root.display(),
        pid_file = m.pid_file.display(),
        heartbeat_file = m.heartbeat_file.display(),
        heartbeat_interval_secs = m.heartbeat_interval_secs,
        use_launchd = m.use_launchd,
        launchd_label = m.launchd_label,
        use_systemd = m.use_systemd,
        systemd_unit = m.systemd_unit,
        socket_path = m.socket_path.display(),
    );
    write_private(marker_path, &body);
}

/// #7855: the opt-in hang-recovery settings, as marker lines.
///
/// The scheduled watchdog job's environment carries only paths, so the marker
/// is the one place a start-time setting can reach it. Each field resolves
/// **env > config > prior marker > empty (off)**: the start's
/// `LOOM_WATCHDOG_HANG_RECOVER*` value when recognisable, else the matching
/// `autonomous.watchdogHangRecover.*` key from the start's effective config,
/// else the PRIOR marker's value — so a self-update, a #5391 watchdog
/// recovery, or a bare restart that did not re-export the variable does not
/// silently turn an opted-in host back off (nor an opted-out one on). An
/// explicit `0/false/no` (or config `false`) turns it off. Only an operator
/// stop, which deletes the marker, drops the carry.
fn hang_recover_fields(
    env_lookup: impl Fn(&str) -> Option<String>,
    config_lookup: impl Fn(&str) -> Option<String>,
    prior: impl Fn(&str) -> Option<String>,
) -> String {
    use crate::watchdog::{env, hang_recover};
    let mut out = String::new();
    for (env_key, marker_key, config_key) in hang_recover::SETTING_KEYS {
        let is_enable = env_key == hang_recover::ENV_ENABLE;
        let normalise = |v: String| -> Option<String> {
            if is_enable {
                if env::is_true(&v) {
                    Some("true".to_string())
                } else if env::is_false(&v) {
                    Some("false".to_string())
                } else {
                    None
                }
            } else {
                (!v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())).then_some(v)
            }
        };
        let value = env_lookup(env_key)
            .and_then(normalise)
            .or_else(|| config_lookup(config_key).and_then(normalise))
            .or_else(|| prior(marker_key).and_then(normalise))
            .unwrap_or_default();
        out.push_str(&format!("{marker_key}={value}\n"));
    }
    out
}

/// A scalar config value as text (`true`, `5`, `"1800"` → `1800`); objects,
/// arrays and `null` read as unset.
fn config_scalar(config: &serde_json::Value, dotted: &str) -> Option<String> {
    match crate::config_resolver::get_path(config, dotted)? {
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// `( umask 077; cat > file )` — create at 0600 rather than the process umask.
fn write_private(path: &Path, body: &str) {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
        {
            let _ = f.write_all(body.as_bytes());
            return;
        }
    }
    let _ = std::fs::write(path, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_records_the_resolved_autonomy_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        write(
            dir.path(),
            &marker,
            &IntentMarker {
                repo_root: Path::new("/repo"),
                pid_file: Path::new("/repo/.loom/.daemon.pid"),
                heartbeat_file: Path::new("/h/daemon.heartbeat"),
                heartbeat_interval_secs: "60",
                use_launchd: false,
                launchd_label: "",
                use_systemd: true,
                systemd_unit: "loom-daemon.service",
                socket_path: Path::new("/h/loom-daemon.sock"),
            },
        );
        let text = std::fs::read_to_string(&marker).expect("read");
        assert!(text.contains("use_launchd=false\n"));
        assert!(text.contains("use_systemd=true\n"));
        assert!(text.contains("systemd_unit=loom-daemon.service\n"));
        assert!(text.contains("heartbeat_interval_secs=60\n"));
        // #5437's two fields must always be PRESENT, even when empty, because
        // the downgrade check distinguishes "no field" from "field says 0".
        assert!(text.contains("\nwork_finder="));
        assert!(text.contains("\nhealth_gate="));
    }

    /// `(env, config, prior marker)` lookups that each answer one key.
    fn one(key: &'static str, val: &'static str) -> impl Fn(&str) -> Option<String> {
        move |k| (k == key).then(|| val.to_string())
    }
    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn hang_recover_defaults_to_empty_meaning_off() {
        let got = hang_recover_fields(none, none, none);
        assert!(got.contains("watchdog_hang_recover=\n"), "{got}");
        let s = crate::watchdog::hang_recover::Settings::resolve(none, |k| {
            got.lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")).map(str::to_string))
                .filter(|v| !v.is_empty())
        });
        assert!(!s.enabled, "an unset start must leave the watchdog report-only");
    }

    #[test]
    fn hang_recover_is_persisted_from_the_start_env_and_carried_otherwise() {
        let on = hang_recover_fields(one("LOOM_WATCHDOG_HANG_RECOVER", "1"), none, none);
        assert!(on.contains("watchdog_hang_recover=true\n"), "{on}");
        // A later start without the variable carries the prior value...
        let carried = hang_recover_fields(none, none, one("watchdog_hang_recover", "true"));
        assert!(carried.contains("watchdog_hang_recover=true\n"), "{carried}");
        // ...and an explicit 0 turns it off.
        let off = hang_recover_fields(
            one("LOOM_WATCHDOG_HANG_RECOVER", "0"),
            none,
            one("watchdog_hang_recover", "true"),
        );
        assert!(off.contains("watchdog_hang_recover=false\n"), "{off}");
        // Malformed numbers are dropped, not persisted.
        let n =
            hang_recover_fields(one("LOOM_WATCHDOG_HANG_RECOVER_COOLDOWN_SECS", "30m"), none, none);
        assert!(n.contains("watchdog_hang_recover_cooldown_secs=\n"), "{n}");
    }

    #[test]
    fn hang_recover_precedence_is_env_then_config_then_prior_marker() {
        let cfg_on = one("autonomous.watchdogHangRecover.enabled", "true");
        // Config alone opts in.
        let got = hang_recover_fields(none, &cfg_on, none);
        assert!(got.contains("watchdog_hang_recover=true\n"), "{got}");
        // Env beats config.
        let got = hang_recover_fields(one("LOOM_WATCHDOG_HANG_RECOVER", "no"), &cfg_on, none);
        assert!(got.contains("watchdog_hang_recover=false\n"), "{got}");
        // An explicit config `false` beats the prior marker's carry.
        let got = hang_recover_fields(
            none,
            one("autonomous.watchdogHangRecover.enabled", "false"),
            one("watchdog_hang_recover", "true"),
        );
        assert!(got.contains("watchdog_hang_recover=false\n"), "{got}");
        // Tunables follow the same order.
        let got = hang_recover_fields(
            none,
            one("autonomous.watchdogHangRecover.cooldownSecs", "3600"),
            one("watchdog_hang_recover_cooldown_secs", "2400"),
        );
        assert!(got.contains("watchdog_hang_recover_cooldown_secs=3600\n"), "{got}");
    }

    fn intent(repo_root: &Path) -> IntentMarker<'_> {
        IntentMarker {
            repo_root,
            pid_file: Path::new("/p"),
            heartbeat_file: Path::new("/h"),
            heartbeat_interval_secs: "60",
            use_launchd: true,
            launchd_label: "l",
            use_systemd: false,
            systemd_unit: "",
            socket_path: Path::new("/s"),
        }
    }

    #[test]
    fn the_written_marker_reaches_the_watchdog_reader() {
        // End to end through the real files: what start writes is what the
        // scheduled watchdog resolves (#7855 — no key the job never receives).
        if std::env::var("LOOM_WATCHDOG_HANG_RECOVER").is_ok() {
            return; // this process's own env would (correctly) win
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        let read = |marker: &Path| {
            crate::watchdog::hang_recover::Settings::resolve(none, |k| {
                crate::watchdog::marker::get_nonempty(marker, k)
            })
        };

        // 1. A repo config opts in; the watchdog sees it via the marker.
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".loom")).expect("mkdir");
        std::fs::write(
            repo.join(".loom/config.json"),
            r#"{"autonomous":{"watchdogHangRecover":{"enabled":true,"confirmations":5}}}"#,
        )
        .expect("config");
        write(dir.path(), &marker, &intent(&repo));
        let s = read(&marker);
        assert!(s.enabled);
        assert_eq!((s.source, s.confirmations), ("marker", 5));

        // 2. A later start from a repo with no such key carries the opt-in.
        let bare = dir.path().join("bare");
        std::fs::create_dir_all(&bare).expect("mkdir");
        write(dir.path(), &marker, &intent(&bare));
        assert!(read(&marker).enabled, "the prior marker's opt-in is carried");
    }

    #[cfg(unix)]
    #[test]
    fn the_marker_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        write(
            dir.path(),
            &marker,
            &IntentMarker {
                repo_root: Path::new("/r"),
                pid_file: Path::new("/p"),
                heartbeat_file: Path::new("/h"),
                heartbeat_interval_secs: "60",
                use_launchd: true,
                launchd_label: "l",
                use_systemd: false,
                systemd_unit: "",
                socket_path: Path::new("/s"),
            },
        );
        let mode = std::fs::metadata(&marker)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "umask 077 in the shell");
    }
}
