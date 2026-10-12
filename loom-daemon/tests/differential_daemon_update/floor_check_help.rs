//! The `--help` text #11044 added for the fleet floor confirmation, spelled
//! out independently of `help.txt` so [`super::Divergence::HelpDocumentsFleetFloorCheck`]
//! verifies the EXACT intended text rather than "whatever the port prints".
//! A sibling of the main file, which is at the file-size ratchet's threshold.

use super::{classify, insert_before_unique, Answer, Divergence};

/// #11044: the "Fleet floor check" section, inserted before `Usage:`.
const FLOOR_CHECK_SECTION: &str = r#"Fleet floor check (Issue #11044): on a fleet host (a fleet store is
configured: `fleet.repo` / LOOM_FLEET_REPO) the version is set by the fleet
floor, `loom_min_version`, and the host moves only when the floor moves
(#10885). Before it builds or fetches anything, this script compares the
version it is about to install with the floor and the installed version.
It warns when the install moves the host AHEAD of the fleet (above the
floor while the installed version already meets it), BELOW the floor, or
when the floor is not known. It says nothing for a host with no fleet
store, for the floor's own version, for a reinstall of the installed
version, or for a lagging host going to a release above the floor (where a
floor roll would go too). The floor is read without contacting the daemon,
from the host's fleet-sync snapshot (~/.loom/fleet-sync-status.json), which
is at most one fleet.syncIntervalSecs old while the daemon runs and as old
as its last pass when it does not; the warning names the snapshot's time.
On a warning: with a terminal on stdin it asks `Continue? [y/N]` and only
a typed y continues; without one it refuses (exit 1) and names the
override, and never waits on stdin. --yes / LOOM_DAEMON_UPDATE_YES=1
continue without asking. --dry-run prints the warning and never asks.
--check and --resolve-json are unchanged. A run started by the daemon or by
Loom tooling (LOOM_DAEMON_UPDATE_INVOKER) prints it and continues.

"#;

/// #11044: the `--yes` and `--to-floor` usage lines, inserted before the
/// `--help` usage line.
const FLOOR_CHECK_USAGE_LINES: &str = r#"  ./.loom/scripts/cli/loom-daemon-update.sh --yes         Fleet hosts (Issue #11044): install even when the version would move this host off the fleet floor, without asking. Same as LOOM_DAEMON_UPDATE_YES=1. See "Fleet floor check" above.
  ./.loom/scripts/cli/loom-daemon-update.sh --to-floor    Fleet hosts (Issue #11044): install EXACTLY the floor's release (vX.Y.Z of loom_min_version) instead of the newest one — to bring a lagging host up to the fleet. Implies --fetch --tag v<floor>. A host already at or above the floor installs nothing (exit 0); a host with no fleet store, or whose floor is not known, is refused (exit 1). Cannot be combined with --tag, --no-fetch or --resolve-json.
"#;

/// #11044: the `LOOM_DAEMON_UPDATE_YES` / `LOOM_DAEMON_UPDATE_INVOKER`
/// entries, inserted at the top of `Environment:`.
const FLOOR_CHECK_ENV_BLOCK: &str = r#"  LOOM_DAEMON_UPDATE_YES  1/true/yes: same as --yes (Issue #11044).
  LOOM_DAEMON_UPDATE_INVOKER  Set by Loom's own non-interactive callers —
                         the daemon's self-update loop (`daemon`), `fleet
                         roll` (`fleet-roll`) and `fleet add-worker`
                         (`add-worker`). A run carrying it never prompts and
                         is never refused by the fleet floor check; it prints
                         the warning and continues. Not for operators: use
                         --yes.
"#;

/// #11044: the exit-code-1 lines for an unconfirmed fleet floor move.
const FLOOR_CHECK_EXIT_1_LINES: &str = r#"     Also used by the fleet floor check (#11044) when a fleet host would
     move off its floor and the run was not confirmed: the operator
     answered anything but y at the prompt, or there was no terminal to
     ask on and neither --yes nor LOOM_DAEMON_UPDATE_YES=1 was given.
     Nothing was changed. Also used by --to-floor when there is no floor.
"#;

/// The lines the four #11044 blocks sit directly in front of, in order.
pub(crate) const FLOOR_CHECK_INSERTS: [(&str, &str); 4] = [
    ("Usage:", FLOOR_CHECK_SECTION),
    ("  ./.loom/scripts/cli/loom-daemon-update.sh --help", FLOOR_CHECK_USAGE_LINES),
    ("  LOOM_DAEMON_UPDATE_FETCH ", FLOOR_CHECK_ENV_BLOCK),
    ("  3  (--check only)", FLOOR_CHECK_EXIT_1_LINES),
];

/// The #11044 class admits all four floor-check blocks together and nothing
/// less, more or different.
#[test]
fn the_fleet_floor_check_help_divergence_class_is_narrow() {
    let base = Answer {
        rc: 0,
        stdout: "intro\n\nUsage:\n  ./.loom/scripts/cli/loom-daemon-update.sh --fetch  f\n\
                 \x20 ./.loom/scripts/cli/loom-daemon-update.sh --help\n\nEnvironment:\n\
                 \x20 LOOM_DAEMON_UPDATE_FETCH  precedence\n\nExit codes:\n  1  usage\n\
                 \x20 3  (--check only) update available\n"
            .to_string(),
        stderr: String::new(),
    };
    let insert_all = |skip: Option<usize>| {
        let mut out = base.stdout.clone();
        for (i, (anchor, block)) in FLOOR_CHECK_INSERTS.iter().enumerate() {
            if Some(i) == skip {
                continue;
            }
            out = insert_before_unique(&out, anchor, block).expect("anchor present once");
        }
        Answer {
            stdout: out,
            ..base.clone()
        }
    };
    assert_eq!(
        classify(&base, &insert_all(None)),
        Ok(vec![Divergence::HelpDocumentsFleetFloorCheck])
    );
    // Any one block missing: not this class.
    for skip in 0..FLOOR_CHECK_INSERTS.len() {
        assert_eq!(classify(&base, &insert_all(Some(skip))), Err(()), "block {skip} missing");
    }
    // A tampered block.
    let all = insert_all(None);
    let tampered = Answer {
        stdout: all
            .stdout
            .replace("only\na typed y continues", "any\nkey continues"),
        ..base.clone()
    };
    assert_ne!(tampered.stdout, all.stdout, "the tamper must change the text");
    assert_eq!(classify(&base, &tampered), Err(()));
    // Right stdout, but the exit code drifted.
    assert_eq!(classify(&base, &Answer { rc: 1, ..all }), Err(()));
}
