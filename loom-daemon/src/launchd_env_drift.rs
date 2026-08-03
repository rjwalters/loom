//! Detect drift between a launchd job's actual live (bootstrapped)
//! `EnvironmentVariables` and what the on-disk plist currently declares
//! (Issue #4995).
//!
//! # Why
//!
//! launchd's `KeepAlive:{SuccessfulExit:true}` relaunch reuses the job spec
//! already held in launchd's own memory since the last `bootstrap` — it does
//! **not** re-read the plist file from disk. A plain `loom-daemon restart`
//! only sends `RestartDaemon` over the daemon's own IPC socket
//! ([`crate::cli`]'s equivalent in the binary crate); it never touches
//! launchd's job registration. So hand-editing the installed plist's
//! `EnvironmentVariables` and then running a plain restart silently keeps the
//! OLD (already-bootstrapped) environment — the relaunched process comes
//! back with exactly what launchd already had, not what the file now says.
//! This was entirely silent before this module: no error anywhere, the env
//! var is simply not there post-restart.
//!
//! The fix (matching the issue's "Detect" option): before scheduling a
//! restart, compare the *live* environment launchd actually holds for the
//! running job (`launchctl print gui/<uid>/<label>`'s `environment = { ... }`
//! block — the ground truth for what a `KeepAlive` respawn will use) against
//! the *on-disk* plist's declared `EnvironmentVariables` (via `plutil
//! -convert json`). Any disagreement, restricted to the `LOOM_*`/token keys
//! `loom-daemon-start.sh` actually manages (mirrors
//! `defaults/scripts/lib/daemon-env-harvest.sh`'s `harvest_plist_env()`
//! allowlist), is surfaced as a loud warning naming the exact remediation —
//! `launchctl bootout` + `bootstrap` (or re-running `loom-daemon-start.sh`,
//! which performs the equivalent render+bootstrap).
//!
//! This is a macOS/launchd-specific gap: the systemd path re-reads
//! `Environment=` fresh via `daemon-reload` on every unit reload, so there is
//! nothing to detect there (see the launchd/systemd contract table in
//! `defaults/docs/daemon-reference.md`).
//!
//! Every public entry point here is advisory-only and best-effort: any
//! failure (not on macOS, launchd job not found — e.g. an unsupervised nohup
//! host, no on-disk plist to compare against, a `launchctl`/`plutil` binary
//! missing, unparseable output) collapses to `None` — "nothing to say" — and
//! must never fail or block the restart itself.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::daemon_install_state::DEFAULT_LAUNCHD_LABEL;
use crate::main_health_gate::run_capture_with_timeout;

/// Bound on the `launchctl print` / `plutil` probes — both are local,
/// in-memory queries and should return near-instantly; this is only a safety
/// net against a wedged/hung binary blocking the whole restart CLI.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The exact `LOOM_*`/token key allowlist `render_launchd_plist`
/// (`loom-daemon-start.sh`) forwards into the plist, mirrored 1:1 from
/// `defaults/scripts/lib/daemon-env-harvest.sh::harvest_plist_env()` so the
/// bash and Rust sides of this drift check can never silently diverge on
/// which keys are "tracked". `LOOM_DAEMON_SUPERVISOR` is excluded because
/// `loom-daemon-start.sh` hardcodes it unconditionally — it can never drift
/// by definition, and comparing it would only ever produce a false positive
/// on hosts where the live job predates some unrelated key being added.
fn is_tracked_key(key: &str) -> bool {
    if key == "LOOM_DAEMON_SUPERVISOR" {
        return false;
    }
    key.starts_with("LOOM_") || matches!(key, "GH_TOKEN" | "GITEA_TOKEN" | "FORGE_TOKEN")
}

/// Resolve the launchd label this host's daemon runs under: `LOOM_LAUNCHD_LABEL`
/// wins (mirrors `resolve_launchd_label()` in `loom-daemon-start.sh`), else the
/// shared default constant.
fn resolve_label() -> String {
    std::env::var("LOOM_LAUNCHD_LABEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_LAUNCHD_LABEL.to_string())
}

/// Parse `launchctl print gui/<uid>/<label>`'s `environment = { ... }` block
/// into key/value pairs, restricted to [`is_tracked_key`]. This block is the
/// currently-*bootstrapped* job spec launchd holds in memory since its last
/// `bootstrap` — exactly what a `KeepAlive` respawn reuses, independent of any
/// later on-disk plist edit.
///
/// Deliberately ignores the sibling `inherited environment = { ... }` and
/// `default environment = { ... }` blocks `launchctl print` also emits — those
/// describe launchd's own ambient/fallback environment, not the job's declared
/// `EnvironmentVariables`, and are not what `loom-daemon-start.sh` renders.
fn parse_launchctl_environment(output: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut in_block = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if !in_block {
            if trimmed == "environment = {" {
                in_block = true;
            }
            continue;
        }
        if trimmed == "}" {
            break;
        }
        if let Some((key, value)) = trimmed.split_once("=>") {
            let key = key.trim();
            let value = value.trim();
            if is_tracked_key(key) {
                map.insert(key.to_string(), value.to_string());
            }
        }
    }
    map
}

/// Parse a `plutil -convert json -o -`-rendered plist's `EnvironmentVariables`
/// dict, restricted to [`is_tracked_key`]. Returns `None` when the JSON has no
/// `EnvironmentVariables` object at all (malformed/foreign plist, or the parse
/// itself failed) — that is deliberately distinct from "present but empty",
/// which is a legitimate (if unusual) plist state and must still be
/// comparable against a live environment.
fn parse_plist_json_env(json: &str) -> Option<BTreeMap<String, String>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let env = value.get("EnvironmentVariables")?.as_object()?;
    Some(
        env.iter()
            .filter_map(|(k, v)| {
                if !is_tracked_key(k) {
                    return None;
                }
                v.as_str().map(|s| (k.clone(), s.to_string()))
            })
            .collect(),
    )
}

/// One tracked key whose live (bootstrapped) value disagrees with the
/// on-disk plist's declared value. `None` on either side means "absent from
/// that source", never an empty string.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DriftedKey {
    key: String,
    live: Option<String>,
    on_disk: Option<String>,
}

/// Diff the live (bootstrapped) launchd environment against the on-disk
/// plist's declared environment. Both inputs are assumed already filtered to
/// the tracked-key set (`is_tracked_key`). Deterministic (sorted by key) so
/// the rendered warning is stable across runs and unit-testable.
fn env_drift(
    live: &BTreeMap<String, String>,
    on_disk: &BTreeMap<String, String>,
) -> Vec<DriftedKey> {
    let mut keys: BTreeSet<&String> = live.keys().collect();
    keys.extend(on_disk.keys());
    keys.into_iter()
        .filter_map(|k| {
            let l = live.get(k);
            let d = on_disk.get(k);
            if l == d {
                None
            } else {
                Some(DriftedKey {
                    key: k.clone(),
                    live: l.cloned(),
                    on_disk: d.cloned(),
                })
            }
        })
        .collect()
}

/// Render a tracked value for display in the warning. Token keys
/// (`GH_TOKEN`/`GITEA_TOKEN`/`FORGE_TOKEN`) are secrets pulled straight from
/// the environment and must never be echoed in full — only the last 4
/// characters (mirrors `credential_preflight::env_fingerprint`'s convention),
/// enough to tell "differs" from "same" without leaking the credential.
/// `LOOM_*` keys are plain config (paths, booleans, small enums) and are safe
/// to show verbatim.
fn display_value(key: &str, value: &Option<String>) -> String {
    match value {
        None => "(unset)".to_string(),
        Some(v) => {
            if matches!(key, "GH_TOKEN" | "GITEA_TOKEN" | "FORGE_TOKEN") {
                let tail: String = v
                    .chars()
                    .rev()
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                if tail.chars().count() < 4 {
                    "***".to_string()
                } else {
                    format!("***{tail}")
                }
            } else {
                v.clone()
            }
        }
    }
}

/// Render the loud, never-silent warning (Issue #4995's hard acceptance
/// criterion) naming the exact drifted keys and the bootout/bootstrap
/// remediation. Pure so it's unit-testable without a real launchd host.
fn env_drift_warning(label: &str, uid: u32, plist_path: &Path, drifted: &[DriftedKey]) -> String {
    let domain = format!("gui/{uid}");
    let target = format!("{domain}/{label}");
    let mut lines = vec![format!(
        "WARNING: launchd job \"{label}\" is running with a DIFFERENT environment than the \
         on-disk plist now declares. `loom-daemon restart` will NOT pick this up — a KeepAlive \
         relaunch reuses the environment already bootstrapped into launchd, it never re-reads \
         the plist file. Drifted key(s):"
    )];
    for d in drifted {
        lines.push(format!(
            "  {}: running={}  on-disk={}",
            d.key,
            display_value(&d.key, &d.live),
            display_value(&d.key, &d.on_disk),
        ));
    }
    lines.push(format!(
        "To make the on-disk value live, re-bootstrap the job:\n\
         \x20 launchctl bootout {target} && launchctl bootstrap {domain} {}\n\
         \x20 (or re-run loom-daemon-start.sh, which performs the equivalent render+bootstrap).",
        plist_path.display(),
    ));
    lines.join("\n")
}

/// Best-effort pre-restart check (Issue #4995): compare the live launchd
/// job's bootstrapped environment against the on-disk plist. Returns
/// `Some(warning)` only when a real, tracked-key drift is found; returns
/// `None` for every "nothing to say" outcome — not on macOS, no launchd job
/// registered under this label (unsupervised host), no on-disk plist to
/// compare against, or any probe/parse failure. **Never** fails the caller —
/// this is advisory-only and must not block or delay the actual restart.
pub fn check_launchd_env_drift() -> Option<String> {
    check_launchd_env_drift_with(
        resolve_label,
        probe_launchctl_print,
        probe_plist_json,
        plist_path_for,
    )
}

/// [`check_launchd_env_drift`]'s implementation, parameterized over its I/O
/// boundaries so the orchestration (label resolution -> probe -> compare ->
/// render) is unit-testable against fixed fixtures without a real launchd
/// host, `launchctl`, or `plutil` binary.
fn check_launchd_env_drift_with(
    label_fn: impl Fn() -> String,
    launchctl_print: impl Fn(&str) -> Option<String>,
    plutil_json: impl Fn(&Path) -> Option<String>,
    plist_path_fn: impl Fn(&str) -> Option<PathBuf>,
) -> Option<String> {
    if !cfg!(target_os = "macos") {
        // Launchd-specific gap (#4995) — systemd re-reads `Environment=` fresh
        // via `daemon-reload` on every unit reload, so there is nothing to
        // detect on that path.
        return None;
    }

    let label = label_fn();
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}/{label}");
    let print_output = launchctl_print(&target)?;
    let live = parse_launchctl_environment(&print_output);

    let plist_path = plist_path_fn(&label)?;
    let plist_json = plutil_json(&plist_path)?;
    let on_disk = parse_plist_json_env(&plist_json)?;

    let drifted = env_drift(&live, &on_disk);
    if drifted.is_empty() {
        return None;
    }
    Some(env_drift_warning(&label, uid, &plist_path, &drifted))
}

/// Real `launchctl print <target>` probe. `None` on any failure (job not
/// registered — e.g. an unsupervised nohup host — timeout, or missing
/// binary): all indistinguishable "nothing to compare against" outcomes for
/// this advisory-only check.
fn probe_launchctl_print(target: &str) -> Option<String> {
    run_capture_with_timeout("launchctl", &["print", target], &std::env::temp_dir(), PROBE_TIMEOUT)
        .ok()
}

/// Real `plutil -convert json -o - <plist>` probe. `None` on any failure
/// (missing file, unparseable plist, missing binary, timeout).
fn probe_plist_json(plist_path: &Path) -> Option<String> {
    let path_str = plist_path.to_str()?;
    run_capture_with_timeout(
        "plutil",
        &["-convert", "json", "-o", "-", path_str],
        &std::env::temp_dir(),
        PROBE_TIMEOUT,
    )
    .ok()
}

/// Resolve the on-disk plist path `loom-daemon-start.sh` renders to for a
/// given label: `~/Library/LaunchAgents/<label>.plist`. `None` when there is
/// no home directory (should not happen in practice) or the file does not
/// exist — the latter means there is nothing on disk to compare the live
/// environment against, so this is a "nothing to say" outcome, not a defect.
fn plist_path_for(label: &str) -> Option<PathBuf> {
    let path = dirs::home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{label}.plist"));
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real capture of `launchctl print gui/<uid>/com.rjwalters.loom-daemon`
    /// on a running host — used verbatim as the parser fixture so the test
    /// pins the actual on-the-wire format, not an idealized approximation.
    const SAMPLE_LAUNCHCTL_PRINT: &str = r#"gui/501/com.rjwalters.loom-daemon = {
	active count = 1
	path = /Users/example/Library/LaunchAgents/com.rjwalters.loom-daemon.plist
	type = LaunchAgent
	state = running

	program = /Users/example/.local/bin/loom-daemon
	arguments = {
		/Users/example/.local/bin/loom-daemon
	}

	working directory = /Users/example/GitHub/loom

	stdout path = /Users/example/GitHub/loom/.loom/logs/daemon-start.log
	stderr path = /Users/example/GitHub/loom/.loom/logs/daemon-start.log
	inherited environment = {
		DISPLAY => /var/run/com.apple.launchd.jkuOheOR7V/org.xquartz:0
		SSH_AUTH_SOCK => /var/run/com.apple.launchd.5Guo3B5jqI/Listeners
	}

	default environment = {
		PATH => /usr/bin:/bin:/usr/sbin:/sbin
	}

	environment = {
		OSLogRateLimit => 64
		LOOM_PID_FILE => /Users/example/GitHub/loom/.loom/.daemon.pid
		LOOM_GUARD_DECISION_LOG => 1
		LOOM_FORCE_SCOPE => protected
		PATH => /Users/example/.local/bin:/Users/example/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
		LOOM_WORKSPACE => /Users/example/GitHub/loom
		LOOM_DAEMON_SUPERVISOR => launchd
		HOME => /Users/example
		XPC_SERVICE_NAME => com.rjwalters.loom-daemon
	}

	domain = gui/501 [100020]
	asid = 100020
"#;

    #[test]
    fn parses_only_the_environment_block_restricted_to_tracked_keys() {
        let env = parse_launchctl_environment(SAMPLE_LAUNCHCTL_PRINT);
        // Tracked keys present in the `environment = { ... }` block:
        assert_eq!(
            env.get("LOOM_PID_FILE").unwrap(),
            "/Users/example/GitHub/loom/.loom/.daemon.pid"
        );
        assert_eq!(env.get("LOOM_GUARD_DECISION_LOG").unwrap(), "1");
        assert_eq!(env.get("LOOM_FORCE_SCOPE").unwrap(), "protected");
        assert_eq!(env.get("LOOM_WORKSPACE").unwrap(), "/Users/example/GitHub/loom");
        // Untracked keys in the SAME block are excluded (PATH/HOME/OSLogRateLimit/XPC_SERVICE_NAME).
        assert!(!env.contains_key("PATH"));
        assert!(!env.contains_key("HOME"));
        assert!(!env.contains_key("OSLogRateLimit"));
        assert!(!env.contains_key("XPC_SERVICE_NAME"));
        // LOOM_DAEMON_SUPERVISOR is explicitly excluded even though it matches LOOM_*.
        assert!(!env.contains_key("LOOM_DAEMON_SUPERVISOR"));
        // The sibling `inherited environment` / `default environment` blocks
        // must never leak in, even though they contain no tracked keys here —
        // guards against a naive "any => line" parser.
        assert!(!env.contains_key("DISPLAY"));
        assert_eq!(env.len(), 4);
    }

    #[test]
    fn plist_json_env_is_restricted_to_tracked_keys() {
        let json = r#"{
            "Label": "com.rjwalters.loom-daemon",
            "EnvironmentVariables": {
                "PATH": "/usr/bin:/bin",
                "LOOM_WORKSPACE": "/Users/example/GitHub/loom",
                "LOOM_RUNTIME_JUDGE": "codex",
                "LOOM_DAEMON_SUPERVISOR": "launchd",
                "GH_TOKEN": "ghp_abcdef1234567890"
            }
        }"#;
        let env = parse_plist_json_env(json).expect("EnvironmentVariables present");
        assert_eq!(env.get("LOOM_WORKSPACE").unwrap(), "/Users/example/GitHub/loom");
        assert_eq!(env.get("LOOM_RUNTIME_JUDGE").unwrap(), "codex");
        assert_eq!(env.get("GH_TOKEN").unwrap(), "ghp_abcdef1234567890");
        assert!(!env.contains_key("PATH"));
        assert!(!env.contains_key("LOOM_DAEMON_SUPERVISOR"));
        assert_eq!(env.len(), 3);
    }

    #[test]
    fn plist_json_with_no_environment_variables_key_returns_none() {
        assert!(parse_plist_json_env(r#"{"Label": "x"}"#).is_none());
        assert!(parse_plist_json_env("not json at all").is_none());
    }

    #[test]
    fn plist_json_with_empty_environment_variables_is_some_empty_map() {
        // Distinct from "no EnvironmentVariables key at all" — a legitimate,
        // if unusual, plist state that must still be comparable.
        let env = parse_plist_json_env(r#"{"EnvironmentVariables": {}}"#).unwrap();
        assert!(env.is_empty());
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn no_drift_when_maps_agree() {
        let live = map(&[
            ("LOOM_WORKSPACE", "/repo"),
            ("LOOM_RUNTIME_JUDGE", "claude"),
        ]);
        let on_disk = live.clone();
        assert!(env_drift(&live, &on_disk).is_empty());
    }

    #[test]
    fn detects_a_changed_value() {
        // The exact #4995 incident shape: a hand-edited plist adds/changes a
        // LOOM_* key that the running (bootstrapped) job never picked up.
        let live = map(&[("LOOM_RUNTIME_JUDGE", "claude")]);
        let on_disk = map(&[("LOOM_RUNTIME_JUDGE", "codex")]);
        let drifted = env_drift(&live, &on_disk);
        assert_eq!(drifted.len(), 1);
        assert_eq!(drifted[0].key, "LOOM_RUNTIME_JUDGE");
        assert_eq!(drifted[0].live.as_deref(), Some("claude"));
        assert_eq!(drifted[0].on_disk.as_deref(), Some("codex"));
    }

    #[test]
    fn detects_an_added_and_a_removed_key() {
        let live = map(&[("LOOM_ONLY_LIVE", "1")]);
        let on_disk = map(&[("LOOM_ONLY_ON_DISK", "2")]);
        let drifted = env_drift(&live, &on_disk);
        assert_eq!(drifted.len(), 2);
        let added = drifted
            .iter()
            .find(|d| d.key == "LOOM_ONLY_ON_DISK")
            .unwrap();
        assert_eq!(added.live, None);
        assert_eq!(added.on_disk.as_deref(), Some("2"));
        let removed = drifted.iter().find(|d| d.key == "LOOM_ONLY_LIVE").unwrap();
        assert_eq!(removed.live.as_deref(), Some("1"));
        assert_eq!(removed.on_disk, None);
    }

    #[test]
    fn token_values_are_redacted_but_still_distinguishable() {
        let full = Some("ghp_abcdef1234567890".to_string());
        let rendered = display_value("GH_TOKEN", &full);
        assert!(!rendered.contains("abcdef1234567890"), "must not leak the token: {rendered}");
        assert!(rendered.ends_with("7890"));
    }

    #[test]
    fn loom_star_values_are_shown_verbatim() {
        assert_eq!(display_value("LOOM_WORKSPACE", &Some("/repo".to_string())), "/repo");
    }

    #[test]
    fn unset_value_renders_as_unset_not_empty_string() {
        assert_eq!(display_value("LOOM_WORKSPACE", &None), "(unset)");
    }

    #[test]
    fn warning_names_the_exact_remediation_and_never_leaks_a_raw_token() {
        let drifted = vec![
            DriftedKey {
                key: "LOOM_RUNTIME_JUDGE".to_string(),
                live: Some("claude".to_string()),
                on_disk: Some("codex".to_string()),
            },
            DriftedKey {
                key: "GH_TOKEN".to_string(),
                live: Some("ghp_oldsecretvalue".to_string()),
                on_disk: Some("ghp_newsecretvalue".to_string()),
            },
        ];
        let warning = env_drift_warning(
            "com.rjwalters.loom-daemon",
            501,
            Path::new("/Users/example/Library/LaunchAgents/com.rjwalters.loom-daemon.plist"),
            &drifted,
        );
        assert!(warning.starts_with("WARNING:"));
        assert!(warning.contains("LOOM_RUNTIME_JUDGE"));
        assert!(warning.contains("running=claude"));
        assert!(warning.contains("on-disk=codex"));
        assert!(warning.contains("launchctl bootout gui/501/com.rjwalters.loom-daemon"));
        assert!(warning.contains("launchctl bootstrap gui/501"));
        assert!(warning.contains("com.rjwalters.loom-daemon.plist"));
        assert!(!warning.contains("oldsecretvalue"));
        assert!(!warning.contains("newsecretvalue"));
    }

    #[test]
    fn orchestration_returns_none_on_non_macos() {
        if !cfg!(target_os = "macos") {
            assert!(check_launchd_env_drift().is_none());
        }
    }

    #[test]
    fn orchestration_returns_none_when_launchctl_print_fails() {
        // Simulates an unsupervised host: no job registered under this label.
        let result = check_launchd_env_drift_with(
            || "com.rjwalters.loom-daemon".to_string(),
            |_target| None,
            |_path| Some(r#"{"EnvironmentVariables": {}}"#.to_string()),
            |label| Some(PathBuf::from(format!("/tmp/{label}.plist"))),
        );
        assert!(result.is_none());
    }

    #[test]
    fn orchestration_returns_none_when_no_on_disk_plist() {
        let result = check_launchd_env_drift_with(
            || "com.rjwalters.loom-daemon".to_string(),
            |_target| Some("environment = {\n}\n".to_string()),
            |_path| Some(r#"{"EnvironmentVariables": {}}"#.to_string()),
            |_label| None,
        );
        assert!(result.is_none());
    }

    #[test]
    fn orchestration_returns_none_when_live_and_on_disk_agree() {
        let result = check_launchd_env_drift_with(
            || "com.rjwalters.loom-daemon".to_string(),
            |_target| Some("environment = {\n\tLOOM_WORKSPACE => /repo\n}\n".to_string()),
            |_path| Some(r#"{"EnvironmentVariables": {"LOOM_WORKSPACE": "/repo"}}"#.to_string()),
            |label| Some(PathBuf::from(format!("/tmp/{label}.plist"))),
        );
        assert!(result.is_none());
    }

    #[test]
    fn orchestration_surfaces_a_real_drift_end_to_end() {
        let result = check_launchd_env_drift_with(
            || "com.rjwalters.loom-daemon".to_string(),
            |_target| Some("environment = {\n\tLOOM_RUNTIME_JUDGE => claude\n}\n".to_string()),
            |_path| {
                Some(r#"{"EnvironmentVariables": {"LOOM_RUNTIME_JUDGE": "codex"}}"#.to_string())
            },
            |label| Some(PathBuf::from(format!("/tmp/{label}.plist"))),
        );
        let warning = result.expect("a real drift must never be silent");
        assert!(warning.starts_with("WARNING:"));
        assert!(warning.contains("LOOM_RUNTIME_JUDGE"));
        assert!(warning.contains("bootout"));
        assert!(warning.contains("bootstrap"));
    }
}
