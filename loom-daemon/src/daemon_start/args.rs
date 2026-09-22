//! The flag surface, which is contract.
//!
//! Parsing is hand-rolled rather than delegated to clap for two reasons that
//! are both observable:
//!
//! * The refusal text is asserted. An unknown flag prints `Unknown option
//!   '<arg>'` on stderr, then `Use --help for usage`, then exits **1** — not
//!   clap's usage block, and not clap's exit 2.
//! * `--help` prints the script's own banner (`help.txt`), which the retained
//!   suite greps for `FLAGS-OFF`, `--work-finder`, `--no-systemd`,
//!   `--print-unit` and `LOOM_ALLOW_SESSION_DAEMON_START`. clap would print a
//!   generated summary instead.
//!
//! Parsing is positional and left-to-right, so `--bogus --help` still exits 1
//! (the loop reaches `--bogus` first) while `--help --bogus` exits 0.

/// The tri-state the `--from-config` composition rule (#4353) needs.
///
/// A plain boolean collapses "the operator asked for this loop to be off" and
/// "the operator said nothing, let config drive" into the same `false`, which
/// is the bug #4353 fixed: `--from-config --no-work-finder` silently dropped
/// the `--no-work-finder`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Not passed on the CLI — config drives under `--from-config`.
    Unset,
    /// An explicit `--work-finder` / `--health-gate`.
    On,
    /// An explicit `--no-work-finder` / `--no-health-gate`.
    Off,
}

/// Every flag the script accepted, plus the raw argv it persists.
#[derive(Debug, Clone)]
pub struct Args {
    pub from_config: bool,
    pub foreground: bool,
    pub want_work_finder: Want,
    pub want_health_gate: Want,
    pub no_launchd: bool,
    pub no_systemd: bool,
    pub print_plist: bool,
    pub print_unit: bool,
    pub force_env: bool,
    pub heal_watchdog_only: bool,
    /// `ORIGINAL_ARGS` — the raw invocation, captured before parsing, because
    /// `loom-daemon-update.sh` replays the persisted subset verbatim (#3968).
    pub original: Vec<String>,
}

impl Args {
    fn empty(original: Vec<String>) -> Self {
        Self {
            from_config: false,
            foreground: false,
            want_work_finder: Want::Unset,
            want_health_gate: Want::Unset,
            no_launchd: false,
            no_systemd: false,
            print_plist: false,
            print_unit: false,
            force_env: false,
            heal_watchdog_only: false,
            original,
        }
    }
}

/// What the argument loop decided.
pub enum Parsed {
    /// Carry on with these flags.
    Args(Box<Args>),
    /// `--help` / `-h`: print the banner, exit 0.
    Help,
    /// An unknown option: print the two-line refusal, exit 1.
    Unknown(String),
}

/// The `while [[ $# -gt 0 ]]; do case "$1" in … esac; done` loop.
#[must_use]
pub fn parse(argv: &[String]) -> Parsed {
    let mut args = Args::empty(argv.to_vec());
    for arg in argv {
        match arg.as_str() {
            "--help" | "-h" => return Parsed::Help,
            "--from-config" => args.from_config = true,
            "--foreground" | "--fg" => args.foreground = true,
            "--work-finder" => args.want_work_finder = Want::On,
            "--health-gate" => args.want_health_gate = Want::On,
            "--no-work-finder" => args.want_work_finder = Want::Off,
            "--no-health-gate" => args.want_health_gate = Want::Off,
            "--no-launchd" => args.no_launchd = true,
            "--no-systemd" => args.no_systemd = true,
            "--print-plist" => args.print_plist = true,
            "--print-unit" => args.print_unit = true,
            "--force-env" => args.force_env = true,
            "--heal-watchdog-only" => args.heal_watchdog_only = true,
            other => return Parsed::Unknown(other.to_string()),
        }
    }
    Parsed::Args(Box::new(args))
}

/// The subset of `ORIGINAL_ARGS` persisted to `.loom/.daemon.flags` (#3968).
///
/// Only flags that describe **daemon autonomy state** survive; the script-only
/// ones are filtered so a rebuild-and-restart replays the autonomy contract and
/// nothing else. Note `--heal-watchdog-only` is absent from the filter list in
/// the shell too — it exits long before the flags file is written, so it can
/// never reach here, and adding it to the list would be a difference that only
/// looks like tidying.
#[must_use]
pub fn persisted_flags(original: &[String]) -> Vec<String> {
    original
        .iter()
        .filter(|a| {
            !matches!(
                a.as_str(),
                "--foreground"
                    | "--fg"
                    | "--help"
                    | "-h"
                    | "--no-launchd"
                    | "--no-systemd"
                    | "--print-plist"
                    | "--print-unit"
                    | "--force-env"
            )
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn an_unknown_flag_wins_when_it_comes_first() {
        // Left-to-right, exactly like the shell loop.
        assert!(matches!(parse(&v(&["--bogus", "--help"])), Parsed::Unknown(a) if a == "--bogus"));
        assert!(matches!(parse(&v(&["--help", "--bogus"])), Parsed::Help));
    }

    #[test]
    fn from_config_composes_with_an_explicit_loop_flag() {
        let Parsed::Args(a) = parse(&v(&["--from-config", "--no-work-finder"])) else {
            panic!("expected args")
        };
        assert!(a.from_config);
        assert_eq!(a.want_work_finder, Want::Off);
        assert_eq!(a.want_health_gate, Want::Unset, "the unnamed loop stays config-driven");
    }

    #[test]
    fn the_last_spelling_of_a_loop_flag_wins() {
        let Parsed::Args(a) = parse(&v(&["--work-finder", "--no-work-finder"])) else {
            panic!("expected args")
        };
        assert_eq!(a.want_work_finder, Want::Off);
    }

    #[test]
    fn persisted_flags_keep_only_the_autonomy_ones() {
        let got = persisted_flags(&v(&[
            "--from-config",
            "--foreground",
            "--work-finder",
            "--print-plist",
            "--no-launchd",
            "--no-health-gate",
        ]));
        assert_eq!(got, v(&["--from-config", "--work-finder", "--no-health-gate"]));
    }
}
