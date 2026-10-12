//! The script's own argument parser, reproduced rather than delegated to clap.
//!
//! Three properties of that loop are contract, and clap reproduces none of
//! them without editing the oracle:
//!
//! * `--help` prints **this script's banner** (`help.txt`), which the retained
//!   suite greps for individual flag names;
//! * an unknown flag prints ``Unknown option '<arg>'`` on stderr followed by
//!   `Use --help for usage`, and exits **1** — not clap's usage block and
//!   exit 2;
//! * `--timeout` validates its own argument (`^[0-9]+$`) and exits 1 with its
//!   own message, and is otherwise carried through as an opaque **string** to
//!   `loom-daemon restart --drain --timeout`, never re-rendered from a parsed
//!   integer.
//!
//! Environment precedence is the shell's, and its ORDER is load-bearing for
//! exactly one variable: `LOOM_DAEMON_UPDATE_FETCH` is tested for the falsy
//! set first and the truthy set second, so a value in neither set leaves
//! `auto` in place.

use super::out;

/// `FETCH_MODE` — artifact-fetch precedence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FetchMode {
    /// Default: prefer a verified artifact when one resolves, else softly fall
    /// back to the source build.
    Auto,
    /// `--fetch` / `LOOM_DAEMON_UPDATE_FETCH=1`: require an artifact, hard-fail
    /// rather than silently building from source.
    Force,
    /// `--no-fetch` / `LOOM_DAEMON_UPDATE_FETCH=0`: always build from source.
    Off,
}

/// Everything the flag loop and the pre-seeded environment decide.
#[derive(Debug)]
pub struct Args {
    pub dry_run: bool,
    pub force: bool,
    pub check_only: bool,
    pub no_restart: bool,
    pub relaunch: bool,
    pub allow_stale: bool,
    pub auto_resolve_safe_abort: bool,
    pub prune_stale: bool,
    pub drain: bool,
    /// Kept as the operator's own string: it is threaded verbatim into
    /// `loom-daemon restart --drain --timeout <SECS>`.
    pub drain_timeout: Option<String>,
    pub force_after_timeout: bool,
    pub restart_now: bool,
    pub fetch_mode: FetchMode,
    pub resolve_json: bool,
    /// `--tag <TAG>` (#10709): resolve exactly this release instead of the
    /// newest one. `None` is today's `latest` resolution, unchanged.
    pub tag: Option<String>,
    /// `--yes` (#11044): install without the fleet floor confirmation.
    /// `LOOM_DAEMON_UPDATE_YES` is read where the check runs.
    pub yes: bool,
    /// `--to-floor` (#11044): install exactly the fleet floor's release.
    /// Resolved into `tag` + `--fetch` once the floor is read.
    pub to_floor: bool,
}

impl Args {
    /// Parse `argv` (already stripped of the program name).
    ///
    /// Never returns on `--help`, an unknown flag, a bad `--timeout` argument,
    /// or the `--drain` / `--restart-now` conflict: each exits with the code
    /// the script exited with.
    pub fn parse(argv: &[String]) -> Self {
        let mut a = Args {
            dry_run: false,
            force: false,
            check_only: false,
            no_restart: false,
            relaunch: super::util::env_truthy("LOOM_DAEMON_UPDATE_RELAUNCH"),
            allow_stale: false,
            auto_resolve_safe_abort: false,
            prune_stale: false,
            drain: super::util::env_truthy("LOOM_DAEMON_UPDATE_DRAIN"),
            drain_timeout: None,
            force_after_timeout: false,
            restart_now: super::util::env_truthy("LOOM_DAEMON_UPDATE_RESTART_NOW"),
            fetch_mode: FetchMode::Auto,
            resolve_json: false,
            tag: None,
            yes: false,
            to_floor: false,
        };
        // Falsy first, then truthy — the shell ran both tests unconditionally
        // and the second only fires for a value the first did not match.
        if super::util::env_falsy("LOOM_DAEMON_UPDATE_FETCH") {
            a.fetch_mode = FetchMode::Off;
        }
        if super::util::env_truthy("LOOM_DAEMON_UPDATE_FETCH") {
            a.fetch_mode = FetchMode::Force;
        }

        let mut i = 0;
        while i < argv.len() {
            match argv[i].as_str() {
                "--help" | "-h" => {
                    show_help();
                    std::process::exit(0);
                }
                "--dry-run" => a.dry_run = true,
                "--force" => a.force = true,
                "--check" => a.check_only = true,
                "--no-restart" => a.no_restart = true,
                "--relaunch" => a.relaunch = true,
                "--drain" => a.drain = true,
                "--timeout" => {
                    let next = argv.get(i + 1);
                    let numeric = next
                        .is_some_and(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()));
                    if !numeric {
                        out::err("--timeout requires a numeric SECS argument");
                        std::process::exit(1);
                    }
                    a.drain_timeout = next.cloned();
                    i += 1;
                }
                "--force-after-timeout" => a.force_after_timeout = true,
                "--restart-now" => a.restart_now = true,
                "--allow-stale" => a.allow_stale = true,
                "--auto-resolve-safe-abort" => a.auto_resolve_safe_abort = true,
                "--fetch" => a.fetch_mode = FetchMode::Force,
                "--no-fetch" => a.fetch_mode = FetchMode::Off,
                "--resolve-json" => a.resolve_json = true,
                "--tag" => {
                    let next = argv
                        .get(i + 1)
                        .filter(|v| !v.is_empty() && !v.starts_with('-'));
                    let Some(tag) = next else {
                        out::err("--tag requires a release TAG argument (e.g. v0.19.831)");
                        std::process::exit(1);
                    };
                    a.tag = Some(tag.clone());
                    i += 1;
                }
                "--prune-stale-entry-points" => a.prune_stale = true,
                "--yes" => a.yes = true,
                "--to-floor" => a.to_floor = true,
                other => {
                    out::err(&format!("Unknown option '{other}'"));
                    out::say_err("Use --help for usage");
                    std::process::exit(1);
                }
            }
            i += 1;
        }

        if a.drain && a.restart_now {
            out::err(
                "--drain and --restart-now are mutually exclusive (drain vs. immediate restart).",
            );
            std::process::exit(1);
        }
        if let Some(conflict) = to_floor_conflict(&a) {
            out::err(conflict);
            std::process::exit(1);
        }
        if let Some(conflict) = tag_conflict(&a) {
            out::err(conflict);
            std::process::exit(1);
        }

        a
    }
}

/// Why `--tag` cannot be honoured alongside the rest of `a`, if it cannot.
///
/// A pinned tag names a release to FETCH: `--no-fetch` would silently ignore
/// it, and `--resolve-json` answers through the daemon's own resolver, which
/// has no pin. Refusing beats pretending the pin applied.
fn tag_conflict(a: &Args) -> Option<&'static str> {
    a.tag.as_ref()?;
    if a.fetch_mode == FetchMode::Off {
        return Some("--tag names a release to fetch; it cannot be combined with --no-fetch (or LOOM_DAEMON_UPDATE_FETCH=0).");
    }
    if a.resolve_json {
        return Some("--tag cannot be combined with --resolve-json.");
    }
    None
}

/// Why `--to-floor` cannot be honoured alongside the rest of `a`, if it
/// cannot. It becomes `--fetch --tag v<floor>`, so it conflicts with an
/// explicit tag and with everything a tag conflicts with.
fn to_floor_conflict(a: &Args) -> Option<&'static str> {
    if !a.to_floor {
        return None;
    }
    if a.tag.is_some() {
        return Some(
            "--to-floor installs the fleet floor's own release; it cannot be combined with --tag.",
        );
    }
    if a.fetch_mode == FetchMode::Off {
        return Some("--to-floor fetches the floor's release; it cannot be combined with --no-fetch (or LOOM_DAEMON_UPDATE_FETCH=0).");
    }
    if a.resolve_json {
        return Some("--to-floor cannot be combined with --resolve-json.");
    }
    None
}

/// `show_help()` — the script's leading comment block, verbatim.
///
/// The shell recovered it with an `awk` pass over `"$0"` executed as the very
/// first statement of the run (#7794), because reading it lazily raced a
/// same-path truncate+rewrite of the script and handed back a torn banner. The
/// banner is compiled in here, which is the permanent fix that note named: no
/// file is read at all, so there is no window left to narrow.
pub fn show_help() {
    print!("{}", include_str!("help.txt"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_help_banner_is_the_scripts_own_leading_comment_block() {
        let help = include_str!("help.txt");
        assert!(help.starts_with("loom-daemon-update.sh - Self-update"));
        // The flags the retained suite greps for, one assertion each so a
        // silent truncation of the banner names which line went missing.
        for flag in [
            "--check",
            "--dry-run",
            "--force",
            "--no-restart",
            "--relaunch",
            "--drain",
            "--timeout",
            "--force-after-timeout",
            "--restart-now",
            "--allow-stale",
            "--auto-resolve-safe-abort",
            "--fetch",
            "--no-fetch",
            "--resolve-json",
            "--tag",
            "--prune-stale-entry-points",
            "--yes",
            "--to-floor",
            "LOOM_DAEMON_UPDATE_YES",
            "Fleet floor check",
        ] {
            assert!(help.contains(flag), "help.txt lost {flag}");
        }
        // Exit codes 0-8 are branched on by the dispatcher, the daemon's own
        // auto-update tick and the retained suites. Anchored at a line start
        // with the banner's real two-space indent: a bare `contains("3")`
        // would be satisfied by any stray digit in 393 lines of prose.
        for code in 0..=8 {
            // 2 is not a code this script emits; the banner skips it too.
            if code == 2 {
                continue;
            }
            let line = format!("\n  {code}  ");
            assert!(help.contains(&line), "help.txt lost the exit-code-{code} line");
        }
    }

    #[test]
    fn flags_set_exactly_what_they_name() {
        let a = Args::parse(&argv(&["--check"]));
        assert!(a.check_only && !a.dry_run && !a.force);
        let a = Args::parse(&argv(&["--drain", "--timeout", "5", "--force-after-timeout"]));
        assert!(a.drain && a.force_after_timeout);
        assert_eq!(a.drain_timeout.as_deref(), Some("5"));
        let a = Args::parse(&argv(&["--fetch"]));
        assert_eq!(a.fetch_mode, FetchMode::Force);
        let a = Args::parse(&argv(&["--no-fetch"]));
        assert_eq!(a.fetch_mode, FetchMode::Off);
        assert_eq!(Args::parse(&argv(&["--fetch"])).tag, None, "no --tag: unpinned");
        let a = Args::parse(&argv(&["--fetch", "--tag", "v0.19.831"]));
        assert_eq!(a.fetch_mode, FetchMode::Force);
        assert_eq!(a.tag.as_deref(), Some("v0.19.831"));
    }

    #[test]
    fn a_pinned_tag_conflicts_with_no_fetch_and_resolve_json_only() {
        // `--fetch` on the argv so an ambient LOOM_DAEMON_UPDATE_FETCH=0
        // cannot turn this parse into a conflict that exits the test process;
        // every other mode is set on the struct directly.
        let mut a = Args::parse(&argv(&["--fetch", "--tag", "v1.2.3"]));
        assert!(tag_conflict(&a).is_none(), "--fetch");
        a.fetch_mode = FetchMode::Auto;
        assert!(tag_conflict(&a).is_none(), "auto mode");
        a.fetch_mode = FetchMode::Off;
        assert!(tag_conflict(&a).is_some(), "--no-fetch would ignore the pin");
        a.fetch_mode = FetchMode::Force;
        a.resolve_json = true;
        assert!(tag_conflict(&a).is_some(), "--resolve-json has no pin");
        a.tag = None;
        assert!(tag_conflict(&a).is_none(), "no tag, no conflict");
    }

    #[test]
    fn yes_and_to_floor_parse_and_to_floor_conflicts_like_a_tag() {
        let a = Args::parse(&argv(&["--yes"]));
        assert!(a.yes && !a.to_floor);
        let a = Args::parse(&argv(&["--fetch", "--to-floor"]));
        assert!(a.to_floor && !a.yes);
        assert!(to_floor_conflict(&a).is_none());
        let mut b = Args::parse(&argv(&["--fetch"]));
        b.to_floor = true;
        b.tag = Some("v1.2.3".to_string());
        assert!(to_floor_conflict(&b).is_some(), "--tag");
        b.tag = None;
        b.fetch_mode = FetchMode::Off;
        assert!(to_floor_conflict(&b).is_some(), "--no-fetch");
        b.fetch_mode = FetchMode::Force;
        b.resolve_json = true;
        assert!(to_floor_conflict(&b).is_some(), "--resolve-json");
    }

    #[test]
    fn a_later_fetch_flag_wins_over_an_earlier_one() {
        // The shell's loop assigned on each match with no precedence rule, so
        // last-one-wins is the behaviour, not first-one-wins.
        assert_eq!(Args::parse(&argv(&["--fetch", "--no-fetch"])).fetch_mode, FetchMode::Off);
        assert_eq!(Args::parse(&argv(&["--no-fetch", "--fetch"])).fetch_mode, FetchMode::Force);
    }
}
