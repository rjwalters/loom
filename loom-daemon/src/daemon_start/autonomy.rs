//! The FLAGS-OFF / opt-in autonomy contract — the property this port has to
//! preserve most exactly.
//!
//! # What "preserve" means here
//!
//! Everything downstream of this module — the plist's `EnvironmentVariables`,
//! the unit's `Environment=` lines, the `--foreground` exec's inherited
//! environment, the marker's `work_finder=` field — reads the two variables
//! this module exports. So "did the daemon start?" is not a test of the
//! contract; the test is *which two strings were exported, for every
//! combination of flag, pre-exported env and `--from-config`*. That is what
//! `loom-daemon/tests/differential_daemon_start.rs` enumerates against the
//! shell's frozen answers: inverting the default below turns 190 of its 535
//! replayed cases red.
//!
//! # Precedence, in one sentence
//!
//! An already-exported non-empty value always wins; otherwise an explicit flag
//! decides; otherwise the loop is `0` — except under `--from-config`, where a
//! loop with no explicit flag is left **unset** so `.loom/config.json`'s
//! `autonomous` block drives it.
//!
//! `--from-config` **composes** with the flags (#4353) rather than overriding
//! them: `--from-config --work-finder` forces `LOOM_WORK_FINDER=1` and still
//! leaves `LOOM_MAIN_HEALTH_GATE` unset. A plain boolean for the flags collapses
//! "explicitly off" into "not stated" and silently drops the force, which is the
//! bug #4353 fixed; [`super::args::Want`] is the tri-state that prevents it.

use std::path::{Path, PathBuf};

use super::args::Want;
use super::out;
use super::render::Mechanism;

/// `${VAR:-default}` — an empty value counts as unset.
fn export_default(name: &str, default: &str) -> String {
    let current = std::env::var(name).unwrap_or_default();
    let value = if current.is_empty() {
        default.to_string()
    } else {
        current
    };
    std::env::set_var(name, &value);
    value
}

/// Where the informational autonomy line goes.
///
/// Under `--print-plist` / `--print-unit` it is redirected to stderr so an
/// inspection mode emits the plist/unit on stdout and **nothing else** — which
/// is what `--help` promises and what every other advisory on that path already
/// does. A real start is unchanged: the line stays on stdout.
fn autonomy_echo(inspection: bool, msg: &str) {
    if inspection {
        out::say_err(msg);
    } else {
        out::say(msg);
    }
}

/// `resolve_autonomy_env()` — the exports, and the banner line that reports
/// them.
///
/// Called from exactly one of two places per invocation: the inspection
/// short-circuit (which runs *before* the already-running guard, #6387) or the
/// real start path. Both must resolve byte-identical environment, which is why
/// this is one function rather than two blocks that look alike.
pub fn resolve_autonomy_env(
    repo_root: &Path,
    from_config: bool,
    want_work_finder: Want,
    want_health_gate: Want,
    inspection: bool,
) {
    let p = out::palette();

    export_default("LOOM_WORKSPACE", &repo_root.display().to_string());

    // Guard-hook autonomy defaults (#3898): a headless sweep under
    // `--dangerously-skip-permissions` has no human to answer a guard ASK, so
    // an ASK is a silent stall. Both are env-overridable — an already-exported
    // value wins — and both are inherited by every child the daemon spawns,
    // which is the KNOWN CONSEQUENCE #5388 documents.
    export_default("LOOM_GUARD_DECISION_LOG", "1");
    export_default("LOOM_FORCE_SCOPE", "protected");

    if from_config {
        let mut forced: Vec<String> = Vec::new();
        match want_work_finder {
            Want::On => {
                forced.push(format!("work_finder={}", export_default("LOOM_WORK_FINDER", "1")))
            }
            Want::Off => {
                forced.push(format!("work_finder={}", export_default("LOOM_WORK_FINDER", "0")))
            }
            Want::Unset => {}
        }
        match want_health_gate {
            Want::On => forced
                .push(format!("main_health_gate={}", export_default("LOOM_MAIN_HEALTH_GATE", "1"))),
            Want::Off => forced
                .push(format!("main_health_gate={}", export_default("LOOM_MAIN_HEALTH_GATE", "0"))),
            Want::Unset => {}
        }
        if forced.is_empty() {
            autonomy_echo(
                inspection,
                &format!(
                    "{}Autonomous mode: driven by .loom/config.json -> autonomous (env not forced){}",
                    p.bold, p.nc
                ),
            );
        } else {
            autonomy_echo(
                inspection,
                &format!(
                    "{}Autonomous mode: config-driven; forced: {}{}",
                    p.bold,
                    // `"$(IFS=', '; echo "${FORCED_DESC[*]}")"`. `${arr[*]}`
                    // joins with the FIRST character of `$IFS` and ignores the
                    // rest, so a two-character `', '` separator produced
                    // `a=1,b=2` — not `a=1, b=2`. Caught by
                    // `tests/differential_daemon_start.rs`; the retained suite
                    // was green on `", "` because no assertion read this line
                    // with both loops forced.
                    forced.join(","),
                    p.nc
                ),
            );
        }
        return;
    }

    let wf = export_default(
        "LOOM_WORK_FINDER",
        if want_work_finder == Want::On {
            "1"
        } else {
            "0"
        },
    );
    let hg = export_default(
        "LOOM_MAIN_HEALTH_GATE",
        if want_health_gate == Want::On {
            "1"
        } else {
            "0"
        },
    );
    if wf == "0" && hg == "0" {
        autonomy_echo(
            inspection,
            &format!(
                "{}Reliability daemon:{} work_finder=off main_health_gate=off (both loops OFF; opt in with --work-finder / --health-gate / --from-config)",
                p.bold, p.nc
            ),
        );
    } else {
        autonomy_echo(
            inspection,
            &format!("{}Autonomous mode:{} work_finder={wf} main_health_gate={hg}", p.bold, p.nc),
        );
    }
}

/// Which installed file (if any) carries the PRIOR autonomy values, and how to
/// read it.
pub struct PriorAutonomy {
    pub file: Option<PathBuf>,
    pub mechanism: Option<Mechanism>,
}

impl PriorAutonomy {
    fn value(&self, key: &str) -> Option<String> {
        let (file, mech) = (self.file.as_ref()?, self.mechanism?);
        let text = std::fs::read_to_string(file).ok()?;
        match mech {
            Mechanism::Launchd => super::render::plist_env_value(&text, key),
            Mechanism::Systemd => super::render::systemd_env_value(&text, key),
        }
    }
}

/// Everything the downgrade check consults, per autonomy loop.
///
/// One struct rather than nine positional parameters: the four `&str`s are two
/// pairs of `(resolved, pre-exported)` values, and a call site that transposed
/// a pair would compile and silently check the wrong thing.
pub struct DowngradeCheck<'a> {
    pub from_config: bool,
    /// The value this invocation resolved, and what the calling shell had
    /// already exported — `("0", "")` is the silent default the check exists
    /// for, `("0", "0")` is an operator stating it explicitly.
    pub work_finder: (&'a str, &'a str),
    pub health_gate: (&'a str, &'a str),
    pub want_work_finder: Want,
    pub want_health_gate: Want,
    pub prior: &'a PriorAutonomy,
    pub intent_marker: &'a Path,
    /// The resolved config's `autonomous.workFinder.enabled` for the repo root
    /// this start serves (#11934): `Some(true)` means the host's own config
    /// asks for the work finder, so exporting the FLAGS-OFF `0` over it (env
    /// beats config) is a downgrade even with no prior plist/unit value and no
    /// marker to say so. `None`/`Some(false)` adds nothing.
    pub config_work_finder: Option<bool>,
    /// `Some` on the systemd tier (#11951): what decides each loop once the
    /// unit is re-rendered without autonomy env. The start's own resolved
    /// values are never written there, so a loop a drop-in sets, or the
    /// config turns on, is out of this start's reach and is not checked; and
    /// a loop this start asks for that nothing will turn on is refused.
    /// `None` on launchd and nohup, which still carry the start's env.
    pub systemd: Option<&'a super::systemd_autonomy::Layer>,
}

/// `check_autonomy_downgrade_key` + `warn_autonomy_downgrade`.
///
/// Returns `true` when a downgrade was DETECTED (the caller decides whether to
/// refuse — `--print-plist`/`--print-unit` stay warn-only, a real start exits 1).
#[must_use]
pub fn warn_autonomy_downgrade(c: &DowngradeCheck) -> bool {
    let mut detected = false;
    // #11951: checked before the `--from-config` exemption, because
    // `--from-config --work-finder` still asks for the loop on.
    if let Some(layer) = c.systemd {
        detected |= super::systemd_autonomy::unmet_intent(
            layer,
            [
                (c.want_work_finder, c.work_finder.1),
                (c.want_health_gate, c.health_gate.1),
            ],
        );
    }
    // `--from-config` hands control to config deliberately — not a silent
    // default — so it is exempt from the downgrade check entirely.
    if c.from_config {
        return detected;
    }
    // On systemd the start's `0` is never rendered, so it cannot switch off a
    // loop the config enables (#11951); the config check is launchd/nohup only.
    if c.systemd.is_none() {
        detected |= check_config_work_finder(c);
    }
    let out_of_reach = |key: &str| c.systemd.is_some_and(|l| l.decides(key));
    if !out_of_reach("LOOM_WORK_FINDER") {
        detected |= check_key(
            "LOOM_WORK_FINDER",
            c.work_finder.0,
            c.want_work_finder,
            c.work_finder.1,
            c.prior,
            c.intent_marker,
        );
    }
    if !out_of_reach("LOOM_MAIN_HEALTH_GATE") {
        detected |= check_key(
            "LOOM_MAIN_HEALTH_GATE",
            c.health_gate.0,
            c.want_health_gate,
            c.health_gate.1,
            c.prior,
            c.intent_marker,
        );
    }
    detected
}

/// The refusal text a real start prints before exiting 1 (#5409 AC1).
pub fn print_downgrade_refusal() {
    out::err("");
    out::err("ERROR: refusing to start -- this would silently downgrade autonomy (see the");
    out::err("WARNING(s) above). Pass an explicit --work-finder / --no-work-finder (and/or");
    out::err("--health-gate / --no-health-gate) to state the desired value for THIS");
    out::err("invocation, or --from-config to drive from .loom/config.json -> autonomous.");
    out::err("(This refusal fires only on a DETECTED downgrade -- prior plist/unit had the");
    out::err("loop on, the autonomy-desired marker (or its operator-stop record) is present,");
    out::err("or this host's config enables the work finder. A genuinely fresh start with no");
    out::err("prior signal, #3911, is unaffected and still defaults FLAGS-OFF.)");
}

/// `autonomous.workFinder.enabled: true` in the resolved config, about to be
/// overridden by this start's own FLAGS-OFF `LOOM_WORK_FINDER=0` (#11934).
///
/// The daemon resolves the work finder env > config > default, so the `0` a
/// plain start exports does not merely fail to enable the loop: it switches
/// off a loop the host's config turned on. A config-driven host (its unit
/// rendered by `--from-config`, or by `fleet add-worker`, carries no
/// `LOOM_WORK_FINDER` line at all) gives [`check_key`] nothing to compare, and
/// an operator stop moves the marker aside, so without this check a plain
/// start after `restart --pause --then-exit` brought the daemon back with the
/// work finder silently off.
fn check_config_work_finder(c: &DowngradeCheck) -> bool {
    let (new_val, pre_exported) = c.work_finder;
    if c.config_work_finder != Some(true)
        || new_val != "0"
        || c.want_work_finder == Want::Off
        || !pre_exported.is_empty()
    {
        return false;
    }
    out::warn("");
    out::warn("WARNING: autonomy downgrade -- LOOM_WORK_FINDER renders 0 this start, but this");
    out::warn("  host's config sets autonomous.workFinder.enabled: true. The daemon reads env");
    out::warn("  before config, so this plain start would switch the work finder OFF over");
    out::warn("  the config that enables it (#11934).");
    out::warn("  Remediation: pass --from-config (let .loom/config.json -> autonomous decide),");
    out::warn("  --work-finder to force it on, or --no-work-finder to confirm you want it off.");
    true
}

fn check_key(
    key: &str,
    new_val: &str,
    want: Want,
    pre_exported: &str,
    prior: &PriorAutonomy,
    intent_marker: &Path,
) -> bool {
    if new_val != "0" {
        return false;
    }
    // An explicit `--no-work-finder` is not silent — it is precisely the
    // escape hatch #5409 asks for.
    if want == Want::Off {
        return false;
    }
    // An operator-exported `LOOM_WORK_FINDER=0` is also explicit.
    if !pre_exported.is_empty() {
        return false;
    }

    let mut old_val = prior.value(key).unwrap_or_default();
    // #11934: an operator stop (`restart --drain/--pause --then-exit`, #9588)
    // MOVES the marker aside into `<marker>.stopped`, verbatim. The explicit
    // start that follows is exactly when this check matters, so the record
    // stands in for the marker: without it the prior autonomy was invisible
    // and a plain start came back FLAGS-OFF with no warning.
    let marker_file = prior_marker_file(intent_marker);
    let marker_present = marker_file.is_some();

    // #5437: on the nohup fallback tier no plist/unit is ever rendered, so the
    // marker's own persisted field is the ONLY signal that can tell a prior
    // BARE start from a prior AUTONOMOUS one. Without it every bare restart
    // following any prior start looked like a downgrade.
    if let (true, Some(file)) = (old_val.is_empty(), marker_file.as_deref()) {
        let field = match key {
            "LOOM_WORK_FINDER" => Some("work_finder"),
            "LOOM_MAIN_HEALTH_GATE" => Some("health_gate"),
            _ => None,
        };
        if let Some(field) = field {
            old_val = marker_field(file, field).unwrap_or_default();
        }
    }

    if old_val == "1" {
        out::warn("");
        out::warn(&format!("WARNING: autonomy downgrade -- {key}: 1 -> 0"));
        out::warn(&format!(
            "  The previously installed daemon had {key}=1 (autonomous); this plain start"
        ));
        out::warn(
            "  would render it OFF -- matching the FLAGS-OFF-by-default contract for a start",
        );
        out::warn(
            "  with no explicit flags (#3911), but SILENTLY from an operator's point of view.",
        );
        out::warn(
            "  Remediation: pass --from-config (drive from .loom/config.json -> autonomous),",
        );
        out::warn("  --work-finder / --health-gate to keep autonomy on, or --no-work-finder /");
        out::warn("  --no-health-gate to confirm you want it off.");
        return true;
    }

    if old_val.is_empty() && marker_present {
        out::warn("");
        out::warn(&format!(
            "WARNING: autonomy downgrade -- {key} renders 0 this start, and no prior plist/unit"
        ));
        out::warn(&format!(
            "  value could be read -- but the autonomy-desired marker ({}) is",
            marker_file.as_deref().unwrap_or(intent_marker).display()
        ));
        out::warn("  present, meaning this host previously ran loom-daemon autonomously.");
        out::warn(
            "  Remediation: pass --from-config (drive from .loom/config.json -> autonomous),",
        );
        out::warn("  --work-finder / --health-gate to keep autonomy on, or --no-work-finder /");
        out::warn("  --no-health-gate to confirm you want it off.");
        return true;
    }
    false
}

/// The file carrying the prior marker's fields: the marker itself, else the
/// operator-stop record it was moved aside into (#9588, #11934), else `None`.
///
/// A record counts only when it actually holds a moved-aside marker. A stop
/// recorded with no marker in place (an unsupervised daemon) carries just its
/// own `operator_stop_*` fields, which say nothing about prior autonomy, so it
/// must not turn the next plain start into a conservative refusal.
fn prior_marker_file(intent_marker: &Path) -> Option<PathBuf> {
    if intent_marker.exists() {
        return Some(intent_marker.to_path_buf());
    }
    let record = crate::operator_stop::record_path(intent_marker);
    let text = std::fs::read_to_string(&record).ok()?;
    text.lines()
        .map(str::trim)
        .any(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("operator_stop_"))
        .then_some(record)
}

/// `grep -E "^<field>=" "$INTENT_MARKER" | head -n1 | cut -d= -f2-`.
fn marker_field(marker: &Path, field: &str) -> Option<String> {
    let text = std::fs::read_to_string(marker).ok()?;
    let prefix = format!("{field}=");
    text.lines()
        .find(|l| l.starts_with(&prefix))
        .map(|l| l[prefix.len()..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_prior() -> PriorAutonomy {
        PriorAutonomy {
            file: None,
            mechanism: None,
        }
    }

    #[test]
    fn a_genuinely_fresh_start_never_reports_a_downgrade() {
        // #3911's FLAGS-OFF default with no prior signal is correct and stays
        // completely unchanged — this check only closes the SILENT part of a
        // transition FROM autonomous.
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        assert!(!warn_autonomy_downgrade(&DowngradeCheck {
            from_config: false,
            work_finder: ("0", ""),
            health_gate: ("0", ""),
            want_work_finder: Want::Unset,
            want_health_gate: Want::Unset,
            prior: &no_prior(),
            intent_marker: &marker,
            config_work_finder: None,
            systemd: None,
        }));
    }

    #[test]
    fn an_explicit_off_is_not_silent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        std::fs::write(&marker, "work_finder=1\nhealth_gate=1\n").expect("write");
        assert!(
            !warn_autonomy_downgrade(&DowngradeCheck {
                from_config: false,
                work_finder: ("0", ""),
                health_gate: ("0", ""),
                want_work_finder: Want::Off,
                want_health_gate: Want::Off,
                prior: &no_prior(),
                intent_marker: &marker,
                config_work_finder: None,
                systemd: None,
            }),
            "--no-work-finder / --no-health-gate state the value explicitly"
        );
    }

    #[test]
    fn a_pre_exported_zero_is_also_explicit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        std::fs::write(&marker, "work_finder=1\n").expect("write");
        assert!(!warn_autonomy_downgrade(&DowngradeCheck {
            from_config: false,
            work_finder: ("0", "0"),
            health_gate: ("0", "0"),
            want_work_finder: Want::Unset,
            want_health_gate: Want::Unset,
            prior: &no_prior(),
            intent_marker: &marker,
            config_work_finder: None,
            systemd: None,
        }));
    }

    #[test]
    fn a_prior_bare_start_on_the_nohup_tier_stays_silent() {
        // #5437: marker present, no plist/unit, and the marker records the
        // prior value as 0 — no transition, so no warning. Before that field
        // existed this case was indistinguishable from a prior autonomous run.
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        std::fs::write(&marker, "work_finder=0\nhealth_gate=0\n").expect("write");
        assert!(!warn_autonomy_downgrade(&DowngradeCheck {
            from_config: false,
            work_finder: ("0", ""),
            health_gate: ("0", ""),
            want_work_finder: Want::Unset,
            want_health_gate: Want::Unset,
            prior: &no_prior(),
            intent_marker: &marker,
            config_work_finder: None,
            systemd: None,
        }));
    }

    #[test]
    fn a_prior_autonomous_start_on_the_nohup_tier_is_detected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        std::fs::write(&marker, "work_finder=1\nhealth_gate=0\n").expect("write");
        assert!(warn_autonomy_downgrade(&DowngradeCheck {
            from_config: false,
            work_finder: ("0", ""),
            health_gate: ("0", ""),
            want_work_finder: Want::Unset,
            want_health_gate: Want::Unset,
            prior: &no_prior(),
            intent_marker: &marker,
            config_work_finder: None,
            systemd: None,
        }));
    }

    #[test]
    fn a_marker_with_no_recorded_value_still_refuses_conservatively() {
        // The pre-#5437 marker format: presence alone is recorded intent.
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        std::fs::write(&marker, "# old format\nuse_launchd=false\n").expect("write");
        assert!(warn_autonomy_downgrade(&DowngradeCheck {
            from_config: false,
            work_finder: ("0", ""),
            health_gate: ("0", ""),
            want_work_finder: Want::Unset,
            want_health_gate: Want::Unset,
            prior: &no_prior(),
            intent_marker: &marker,
            config_work_finder: None,
            systemd: None,
        }));
    }

    #[test]
    fn from_config_is_exempt_entirely() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        std::fs::write(&marker, "work_finder=1\n").expect("write");
        assert!(!warn_autonomy_downgrade(&DowngradeCheck {
            from_config: true,
            work_finder: ("0", ""),
            health_gate: ("0", ""),
            want_work_finder: Want::Unset,
            want_health_gate: Want::Unset,
            prior: &no_prior(),
            intent_marker: &marker,
            config_work_finder: None,
            systemd: None,
        }));
    }

    fn bare(marker: &Path, config_work_finder: Option<bool>) -> bool {
        warn_autonomy_downgrade(&DowngradeCheck {
            from_config: false,
            work_finder: ("0", ""),
            health_gate: ("0", ""),
            want_work_finder: Want::Unset,
            want_health_gate: Want::Unset,
            prior: &no_prior(),
            intent_marker: marker,
            config_work_finder,
            systemd: None,
        })
    }

    #[test]
    fn an_operator_stop_record_stands_in_for_the_moved_aside_marker() {
        // #11934: `restart --pause --then-exit` records an operator stop,
        // which MOVES the marker aside. The explicit start that follows must
        // still see the prior autonomous run, or it comes back FLAGS-OFF.
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        std::fs::write(&marker, "work_finder=1\nhealth_gate=0\n").expect("write");
        crate::operator_stop::record(&marker, "pause-then-exit").expect("record");
        assert!(!marker.exists(), "the stop moved the marker aside");
        assert!(bare(&marker, None), "the record's work_finder=1 is a prior autonomous run");
    }

    #[test]
    fn an_operator_stop_record_of_a_bare_run_stays_silent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        std::fs::write(&marker, "work_finder=0\nhealth_gate=0\n").expect("write");
        crate::operator_stop::record(&marker, "drain-then-exit").expect("record");
        assert!(!bare(&marker, None), "a recorded bare run is no transition");
    }

    #[test]
    fn a_stop_record_with_no_moved_aside_marker_is_no_signal() {
        // An unsupervised daemon's stop has no marker to move aside; its record
        // holds only the stop fields and must not refuse the next plain start.
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        crate::operator_stop::record(&marker, "then-exit").expect("record");
        assert!(crate::operator_stop::is_recorded(&marker));
        assert!(!bare(&marker, None));
    }

    #[test]
    fn config_enabling_the_work_finder_makes_a_plain_start_a_downgrade() {
        // #11934: the unit of a config-driven host carries no LOOM_WORK_FINDER
        // and the marker may be gone; the config itself is the prior signal.
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        assert!(bare(&marker, Some(true)));
        assert!(!bare(&marker, Some(false)), "config off: FLAGS-OFF agrees with it");
        assert!(!bare(&marker, None), "no config: a genuinely fresh start (#3911)");
    }

    #[test]
    fn config_enabled_but_explicit_or_config_driven_starts_are_not_downgrades() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("autonomy-desired");
        let with = |from_config: bool, wf: (&str, &str), want: Want| {
            warn_autonomy_downgrade(&DowngradeCheck {
                from_config,
                work_finder: wf,
                health_gate: ("0", ""),
                want_work_finder: want,
                want_health_gate: Want::Unset,
                prior: &no_prior(),
                intent_marker: &marker,
                config_work_finder: Some(true),
                systemd: None,
            })
        };
        assert!(!with(true, ("", ""), Want::Unset), "--from-config hands control to config");
        assert!(!with(false, ("0", ""), Want::Off), "--no-work-finder is explicit");
        assert!(!with(false, ("0", "0"), Want::Unset), "an exported 0 is explicit");
        assert!(!with(false, ("1", ""), Want::On), "--work-finder keeps it on");
    }

    #[test]
    fn marker_field_keeps_every_later_equals_sign() {
        // `cut -d= -f2-`, not `-f2`.
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("m");
        std::fs::write(&marker, "work_finder=a=b\n").expect("write");
        assert_eq!(marker_field(&marker, "work_finder").as_deref(), Some("a=b"));
    }
}
