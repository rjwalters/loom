//! The `--help` text #11539 added for the third safe ff-abort class (every
//! dirty tracked file already identical to origin's version), spelled out
//! independently of `help.txt` so
//! [`super::Divergence::HelpDocumentsIdenticalToOriginSafeAbort`] verifies the
//! EXACT intended text rather than "whatever the port prints". A sibling of
//! the main file, which is at the file-size ratchet's threshold.

/// #11539: the second `--auto-resolve-safe-abort` usage line, inserted before
/// the `--fetch` usage line (that is, directly after the first one).
const USAGE_LINE: &str = r#"  ./.loom/scripts/cli/loom-daemon-update.sh --auto-resolve-safe-abort  Also safe (#11539): local <default-branch> strictly BEHIND origin with dirty tracked files that are ALL identical to origin's version, in both the working tree and the index (`git checkout HEAD --` them, then fast-forward; nothing is lost, the fast-forward lands that exact content). One file that differs from origin, a staged change that differs from both HEAD and origin, or any local-only commit still hard-aborts. Without this flag the abort names the safe command.
"#;

/// #11539: the exit-code-1 lines, inserted before the `--resolve-json` case.
const EXIT_1_LINES: &str = r#"     Also safely resolvable (#11539): local <default-branch> strictly
     behind origin with dirty tracked files that are ALL identical to
     origin's version (working tree and index); the abort names
     `git checkout HEAD -- <paths>`, which --auto-resolve-safe-abort runs
     before retrying the fast-forward.
"#;

/// The lines the two #11539 blocks sit directly in front of, in order.
pub(crate) const IDENTICAL_TO_ORIGIN_INSERTS: [(&str, &str); 2] = [
    ("  ./.loom/scripts/cli/loom-daemon-update.sh --fetch        ", USAGE_LINE),
    ("     Also used by --resolve-json (#7609)", EXIT_1_LINES),
];
