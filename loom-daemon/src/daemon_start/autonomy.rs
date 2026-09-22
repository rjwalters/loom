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
}

/// `check_autonomy_downgrade_key` + `warn_autonomy_downgrade`.
///
/// Returns `true` when a downgrade was DETECTED (the caller decides whether to
/// refuse — `--print-plist`/`--print-unit` stay warn-only, a real start exits 1).
#[must_use]
pub fn warn_autonomy_downgrade(c: &DowngradeCheck) -> bool {
    // `--from-config` hands control to config deliberately — not a silent
    // default — so it is exempt from this check entirely.
    if c.from_config {
        return false;
    }
    let mut detected = false;
    detected |= check_key(
        "LOOM_WORK_FINDER",
        c.work_finder.0,
        c.want_work_finder,
        c.work_finder.1,
        c.prior,
        c.intent_marker,
    );
    detected |= check_key(
        "LOOM_MAIN_HEALTH_GATE",
        c.health_gate.0,
        c.want_health_gate,
        c.health_gate.1,
        c.prior,
        c.intent_marker,
    );
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
    out::err("loop on, or the autonomy-desired marker is present. A genuinely fresh start");
    out::err("with no prior signal, #3911, is unaffected and still defaults FLAGS-OFF.)");
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
    let marker_present = intent_marker.exists();

    // #5437: on the nohup fallback tier no plist/unit is ever rendered, so the
    // marker's own persisted field is the ONLY signal that can tell a prior
    // BARE start from a prior AUTONOMOUS one. Without it every bare restart
    // following any prior start looked like a downgrade.
    if old_val.is_empty() && marker_present {
        let field = match key {
            "LOOM_WORK_FINDER" => Some("work_finder"),
            "LOOM_MAIN_HEALTH_GATE" => Some("health_gate"),
            _ => None,
        };
        if let Some(field) = field {
            old_val = marker_field(intent_marker, field).unwrap_or_default();
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
            intent_marker.display()
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
        }));
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
