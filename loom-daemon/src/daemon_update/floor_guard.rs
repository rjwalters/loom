//! Confirm before a manual run moves a fleet host off the fleet floor
//! (Issue #11044).
//!
//! Since #10885 a fleet host's version is set by the fleet floor
//! (`loom_min_version`): it moves only when the floor moves. This script still
//! installs the newest release, or a source build, and so a manual run on a
//! fleet host could leave that one host ahead of (or below) the rest of the
//! fleet with nothing said. This module is the "are you sure?" step.
//!
//! # When it speaks
//!
//! Only on a **fleet host** (a fleet store is configured, so the floor is
//! `Set` or `Unknown`, never `NoStore`), and only when the version about to be
//! installed is not what the floor implies ([`warning`]):
//!
//! | Floor | Target | Verdict |
//! |---|---|---|
//! | set | equal to the floor | silent |
//! | set | equal to the installed version (a reinstall) | silent |
//! | set | above the floor, installed below it (catching up) | silent: a floor roll goes there too |
//! | set | above the floor, installed at or above it (or unknown) | **ahead of the fleet** |
//! | set | below the floor | **below the fleet** |
//! | unknown | anything | **floor not known**: the daemon fails closed and moves nothing, so a manual move is flagged |
//!
//! The floor comes from [`crate::fleet_sync::offline_floor`], which reads the
//! config and the on-disk fleet-sync snapshot and never contacts the daemon.
//!
//! # What it does about it ([`decide`])
//!
//! `--dry-run` prints the warning and never prompts. A run the daemon or Loom
//! tooling started ([`INVOKER_ENV`]) prints it and proceeds: those runs are
//! never interactive and must never block. `--yes` / [`YES_ENV`] proceeds. A
//! terminal on stdin gets a `[y/N]` prompt; anything else is refused with exit
//! 1 and the override named, never a read from a stdin that is not a TTY.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use super::args::{Args, FetchMode};
use super::{exit, out, util};
use crate::fleet_sync::offline_floor::{offline_floor, OfflineFloor};
use crate::fleet_sync::FloorKnowledge;

/// `LOOM_DAEMON_UPDATE_YES=1`: the environment form of `--yes`.
pub const YES_ENV: &str = "LOOM_DAEMON_UPDATE_YES";

/// Set by every caller that runs this script without an operator at a
/// terminal: the daemon's own self-update loop (`daemon`), `fleet roll`
/// (`fleet-roll`) and `fleet add-worker` (`add-worker`). Its value names the
/// caller in the log. A marked run never prompts and is never refused here.
pub const INVOKER_ENV: &str = "LOOM_DAEMON_UPDATE_INVOKER";

/// What this run is about to install.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    /// `X.Y.Z`, or empty when it could not be determined.
    pub version: &'a str,
    /// How it is installed, for the message: `release v0.19.953`,
    /// `a source build`.
    pub how: &'a str,
}

/// What to do once a warning has been printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Carry on without asking.
    Proceed,
    /// Ask on the terminal.
    Prompt,
    /// Refuse: exit 1, naming the override.
    Refuse,
}

/// The warning for installing `target` on a host with `floor`, or `None` when
/// the install is what the floor implies (see the module doc's table).
#[must_use]
pub fn warning(floor: &OfflineFloor, target: Target<'_>, installed: &str) -> Option<String> {
    let fleet = floor.store.as_deref().unwrap_or("(unnamed store)");
    let what = if target.version.is_empty() {
        format!("{} (version unknown)", target.how)
    } else {
        format!("{} ({})", target.version, target.how)
    };
    let floor_value = match &floor.knowledge {
        FloorKnowledge::NoStore => return None,
        FloorKnowledge::Unknown(why) => {
            return Some(format!(
                "This host is in fleet {fleet}, but its loom_min_version is not known ({why}). \
                 Installing {what} may move it off the fleet's version; the daemon itself moves a \
                 fleet host only when the floor moves."
            ));
        }
        FloorKnowledge::Set(f) => f.as_str(),
    };
    let snapshot = floor
        .snapshot_at
        .as_deref()
        .map(|at| format!(" (fleet-sync snapshot of {at})"))
        .unwrap_or_default();
    let head =
        format!("This host is in fleet {fleet} with loom_min_version {floor_value}{snapshot}.");
    if target.version.is_empty() {
        return Some(format!(
            "{head} Installing {what} may move it off the floor; the fleet changes version by \
             raising the floor."
        ));
    }
    let to_floor = util::semver_compare(target.version, floor_value);
    if to_floor.is_eq() || (!installed.is_empty() && target.version == installed) {
        return None;
    }
    if to_floor.is_lt() {
        return Some(format!(
            "{head} Installing {what} moves it BELOW the fleet floor; the daemon's own floor check \
             would roll it forward again."
        ));
    }
    let catching_up = !installed.is_empty() && util::semver_compare(installed, floor_value).is_lt();
    if catching_up {
        return None;
    }
    Some(format!(
        "{head} Installing {what} moves it ahead of the fleet; the fleet changes version by \
         raising the floor."
    ))
}

/// What to do about a warning: the pure half of [`enforce`].
#[must_use]
pub fn decide(dry_run: bool, yes: bool, invoker: Option<&str>, interactive: bool) -> Decision {
    if dry_run || invoker.is_some() || yes {
        Decision::Proceed
    } else if interactive {
        Decision::Prompt
    } else {
        Decision::Refuse
    }
}

/// Whether a typed answer is a yes. Only `y` / `yes`, any case: the default
/// is no.
#[must_use]
pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// What a refusal says was changed: `None` when nothing was (the #11064
/// preflight runs before the checkout sync), else what the run already did.
#[must_use]
pub fn nothing_changed(changed: Option<&str>) -> String {
    match changed {
        None => "Nothing was changed.".to_string(),
        Some(what) => format!("The installed daemon was not changed, but {what}."),
    }
}

/// The check, end to end. Returns `true` when it warned and the run may
/// continue, `false` when there was nothing to warn about; exits 1 when the
/// operator declines or no terminal can be asked. `changed` names what this
/// run has already changed, for the refusal ([`nothing_changed`]).
pub fn enforce(
    workspace: &Path,
    target: Target<'_>,
    installed: &str,
    (dry_run, yes): (bool, bool),
    changed: Option<&str>,
) -> bool {
    let floor = offline_floor(workspace);
    let Some(message) = warning(&floor, target, installed) else {
        return false;
    };
    let invoker = util::env_non_empty(INVOKER_ENV);
    let yes = yes || util::env_truthy(YES_ENV);
    let interactive = std::io::stdin().is_terminal();
    match decide(dry_run, yes, invoker.as_deref(), interactive) {
        Decision::Proceed => {
            out::warn(&message);
            if dry_run {
                out::say("[dry-run] A real run would ask for confirmation first (and refuse without a terminal unless --yes or LOOM_DAEMON_UPDATE_YES=1 is given).");
            } else if let Some(by) = invoker {
                out::say(&format!("Started by {by} ({INVOKER_ENV}): not asking, continuing."));
            } else {
                out::say("--yes (or LOOM_DAEMON_UPDATE_YES=1) given: continuing.");
            }
        }
        Decision::Prompt => {
            out::warn(&message);
            eprint!("Continue? [y/N] ");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            let _ = std::io::stdin().lock().read_line(&mut answer);
            if !is_yes(&answer) {
                out::err(&match changed {
                    None => "Not confirmed: nothing was changed.".to_string(),
                    Some(what) => {
                        format!("Not confirmed: the installed daemon was not changed, but {what}.")
                    }
                });
                exit(1);
            }
        }
        Decision::Refuse => {
            out::warn(&message);
            out::err(&format!("Refusing to move a fleet host off the fleet floor without confirmation, and there is no terminal to ask on. {}", nothing_changed(changed)));
            out::say_err("Re-run with --yes (or LOOM_DAEMON_UPDATE_YES=1) to install it anyway, or raise the floor to move the whole fleet.");
            exit(1);
        }
    }
    true
}

/// `--to-floor`: turn the floor into `--fetch --tag v<floor>`, or refuse
/// (exit 1) when this host has no floor to install.
pub fn apply_to_floor(a: &mut Args, workspace: &Path) {
    let floor = offline_floor(workspace);
    match floor.knowledge {
        FloorKnowledge::Set(f) => {
            out::say(&format!(
                "--to-floor: fleet {} has loom_min_version {f}; installing release v{f}.",
                floor.store.as_deref().unwrap_or("(unnamed store)")
            ));
            a.tag = Some(format!("v{f}"));
            a.fetch_mode = FetchMode::Force;
        }
        FloorKnowledge::NoStore => {
            out::err("--to-floor: this host reads no fleet store (fleet.repo / LOOM_FLEET_REPO is not set), so it has no floor to install.");
            exit(1);
        }
        FloorKnowledge::Unknown(why) => {
            out::err(&format!(
                "--to-floor: this host is in fleet {}, but its loom_min_version is not known ({why}).",
                floor.store.as_deref().unwrap_or("(unnamed store)")
            ));
            exit(1);
        }
    }
}

/// What the installed daemon's `--version` said, as `--to-floor` needs it
/// (#11064): no binary at all is a bootstrap, but a binary whose version
/// cannot be read may be newer than the floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstalledVersion {
    /// No installed binary resolved, or none that can be executed.
    Absent,
    /// An executable binary at this path gave no readable version.
    Unreadable(String),
    /// `X.Y.Z`.
    Known(String),
}

impl InstalledVersion {
    /// The version, or `""` when there is none.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            InstalledVersion::Known(v) => v,
            _ => "",
        }
    }
}

/// What `--to-floor` does about the installed version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToFloor {
    /// Install the floor's release.
    Install,
    /// Nothing to install (exit 0): the message says why.
    Met(String),
    /// Refuse (exit 1): installing the floor could move the host down.
    Refuse(String),
}

/// `--to-floor` against the installed version: it never moves a host down,
/// a host at the floor has nothing to do (unless `--force`), and a binary
/// whose version cannot be read is refused rather than overwritten (#11064).
#[must_use]
pub fn to_floor_decision(floor_tag: &str, installed: &InstalledVersion, force: bool) -> ToFloor {
    let floor = floor_tag.trim_start_matches('v');
    let installed = match installed {
        InstalledVersion::Absent => return ToFloor::Install,
        InstalledVersion::Unreadable(bin) => {
            return ToFloor::Refuse(format!(
                "--to-floor: the installed daemon {bin} reports no readable version, so installing the fleet floor {floor} could move this host DOWN. Refusing; nothing was changed. Check `{bin} --version`, or run without --to-floor."
            ));
        }
        InstalledVersion::Known(v) => v.as_str(),
    };
    match util::semver_compare(installed, floor) {
        std::cmp::Ordering::Greater => ToFloor::Met(format!(
            "Installed {installed} is above the fleet floor {floor}; --to-floor never moves a host down. Nothing to do."
        )),
        std::cmp::Ordering::Equal if !force => ToFloor::Met(format!(
            "Installed {installed} is already the fleet floor's release. Nothing to do."
        )),
        _ => ToFloor::Install,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(floor: &str) -> OfflineFloor {
        OfflineFloor {
            knowledge: FloorKnowledge::Set(floor.to_string()),
            store: Some("2AMLogic/fleet-gitops".to_string()),
            snapshot_at: None,
        }
    }

    fn release(version: &str) -> Target<'_> {
        Target {
            version,
            how: "release",
        }
    }

    #[test]
    fn a_non_fleet_host_is_never_warned() {
        let none = OfflineFloor {
            knowledge: FloorKnowledge::NoStore,
            store: None,
            snapshot_at: None,
        };
        assert_eq!(warning(&none, release("9.9.9"), "0.0.1"), None);
    }

    #[test]
    fn the_issue_example_moves_a_host_ahead_of_the_fleet() {
        let w = warning(&set("0.19.950"), release("0.19.953"), "0.19.950").unwrap_or_default();
        assert!(w.contains("fleet 2AMLogic/fleet-gitops with loom_min_version 0.19.950"), "{w}");
        assert!(w.contains("Installing 0.19.953"), "{w}");
        assert!(w.contains("ahead of the fleet"), "{w}");
        // Installed version unknown: fail towards asking.
        assert!(warning(&set("0.19.950"), release("0.19.953"), "").is_some());
    }

    #[test]
    fn the_floor_itself_a_reinstall_and_catching_up_are_silent() {
        assert_eq!(warning(&set("0.19.950"), release("0.19.950"), "0.19.953"), None);
        assert_eq!(warning(&set("0.19.950"), release("0.19.953"), "0.19.953"), None);
        // A lagging host going to a release above the floor: a floor roll
        // would go to the newest release at or above it too.
        assert_eq!(warning(&set("0.19.950"), release("0.19.953"), "0.19.940"), None);
    }

    #[test]
    fn below_the_floor_and_unknown_cases_warn() {
        let w = warning(&set("0.19.950"), release("0.19.940"), "0.19.950").unwrap_or_default();
        assert!(w.contains("BELOW the fleet floor"), "{w}");
        let unknown = OfflineFloor {
            knowledge: FloorKnowledge::Unknown("no snapshot".to_string()),
            store: Some("o/fleet".to_string()),
            snapshot_at: None,
        };
        let w = warning(&unknown, release("0.19.950"), "0.19.950").unwrap_or_default();
        assert!(w.contains("not known (no snapshot)"), "{w}");
        let w = warning(&set("0.19.950"), release(""), "0.19.950").unwrap_or_default();
        assert!(w.contains("version unknown"), "{w}");
    }

    #[test]
    fn only_an_unmarked_interactive_run_prompts_and_a_non_tty_is_refused() {
        assert_eq!(decide(false, false, None, true), Decision::Prompt);
        assert_eq!(decide(false, false, None, false), Decision::Refuse);
        // A daemon-started run never prompts, terminal or not.
        assert_eq!(decide(false, false, Some("daemon"), true), Decision::Proceed);
        assert_eq!(decide(false, false, Some("daemon"), false), Decision::Proceed);
        assert_eq!(decide(false, true, None, false), Decision::Proceed);
        assert_eq!(decide(true, false, None, true), Decision::Proceed, "--dry-run never prompts");
    }

    #[test]
    fn to_floor_installs_only_for_a_host_below_the_floor() {
        let known = |v: &str| InstalledVersion::Known(v.to_string());
        let met = |d: ToFloor| matches!(d, ToFloor::Met(_));
        assert_eq!(to_floor_decision("v0.19.950", &known("0.19.940"), false), ToFloor::Install);
        assert!(met(to_floor_decision("v0.19.950", &known("0.19.950"), false)));
        assert_eq!(
            to_floor_decision("v0.19.950", &known("0.19.950"), true),
            ToFloor::Install,
            "--force reinstalls"
        );
        assert!(met(to_floor_decision("v0.19.950", &known("0.19.953"), true)), "never down");
    }

    #[test]
    fn to_floor_bootstraps_a_missing_binary_but_refuses_an_unreadable_one() {
        assert_eq!(
            to_floor_decision("v0.19.950", &InstalledVersion::Absent, false),
            ToFloor::Install,
            "no installed binary: bootstrap"
        );
        let d = to_floor_decision(
            "v0.19.950",
            &InstalledVersion::Unreadable("/x/loom-daemon".to_string()),
            true,
        );
        let ToFloor::Refuse(why) = d else {
            panic!("an unreadable version must refuse, even with --force: {d:?}");
        };
        assert!(why.contains("/x/loom-daemon") && why.contains("no readable version"), "{why}");
        assert!(why.contains("DOWN"), "{why}");
    }

    #[test]
    fn a_refusal_says_exactly_what_changed() {
        assert_eq!(nothing_changed(None), "Nothing was changed.");
        let what = nothing_changed(Some("the source checkout was fast-forwarded to origin/main"));
        assert!(
            what.starts_with("The installed daemon was not changed, but the source"),
            "{what}"
        );
    }

    #[test]
    fn only_y_or_yes_confirms() {
        for yes in ["y", "Y", "yes", " YES\n"] {
            assert!(is_yes(yes), "{yes:?}");
        }
        for no in ["", "\n", "n", "no", "yep", "sure"] {
            assert!(!is_yes(no), "{no:?}");
        }
    }
}
