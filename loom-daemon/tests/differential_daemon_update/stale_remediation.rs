//! The stale-entry-point remediation text #11069 changed, spelled out
//! independently of `entry_points.rs` so
//! [`super::Divergence::StaleAdvisoryNoRmForUnprunableEntries`] verifies the
//! EXACT intended text rather than "whatever the port prints". A sibling of
//! the main file, which is at the file-size ratchet's threshold.

use super::{classify, Answer, Divergence};

/// The stale-entry-point remediation the frozen shell printed after the
/// flagged entries: a Python-package explanation and an `rm` hint for EVERY
/// flagged entry, whatever it was. `ARGV0` is `loom-daemon-update.sh`.
pub(crate) const STALE_REMEDIATION_SHELL: &str = "These do NOT resolve to the current loom-daemon binary. Loom's Python package
was retired (epic #4081 Phase 4, #4557), so nothing regenerates them — they are
frozen and will shadow the real binary's entry points (incident #4079).
Remove them, e.g.:  rm <path>    (or 'pipx uninstall loom-tools')
Or run:  loom-daemon-update.sh --prune-stale-entry-points   (removes exactly the stale Python console scripts above, #5139).
";

/// What #11069 prints in its place when no flagged entry is a Python console
/// script or a shim — the only shape this corpus produces (every fixture entry
/// is a `#!/bin/sh` script). Spelled out here, independently of
/// `entry_points.rs`, so the class verifies the EXACT intended text.
pub(crate) const STALE_REMEDIATION_UNRELATED: &str = "These do NOT resolve to the current loom-daemon binary, and can shadow its entry points (incident #4079).
Entries that are not Python console scripts or shims were not installed as Loom entry points, and --prune-stale-entry-points leaves them alone.
Find out what each one is before removing anything.
";

/// Is `port` exactly `shell` with [`STALE_REMEDIATION_SHELL`] replaced, once,
/// by [`STALE_REMEDIATION_UNRELATED`]?
pub(crate) fn explains(shell: &str, port: &str) -> bool {
    replace_unique(shell, STALE_REMEDIATION_SHELL, STALE_REMEDIATION_UNRELATED)
        .is_some_and(|expected| expected == port)
}

/// `text` with the single occurrence of `old` — which must start a line —
/// replaced by `new`; `None` when it is absent or not unique.
fn replace_unique(text: &str, old: &str, new: &str) -> Option<String> {
    let needle = format!("\n{old}");
    if text.matches(&needle).count() != 1 {
        return None;
    }
    Some(text.replacen(&needle, &format!("\n{new}"), 1))
}

/// The #11069 class admits exactly the intended replacement and nothing else.
#[test]
fn the_stale_remediation_divergence_class_is_narrow() {
    let base = Answer {
        rc: 3,
        stdout: String::new(),
        stderr: format!(
            "Stale 'loom-*' entry points found on PATH (1):\n  - /x/loom-foo — script, not a loom-daemon shim\n{STALE_REMEDIATION_SHELL}Suppress this check with LOOM_SKIP_STALE_ENTRY_POINT_CHECK=1.\n"
        ),
    };
    let with_stderr = |stderr: String| Answer {
        stderr,
        ..base.clone()
    };
    let good = with_stderr(
        base.stderr
            .replace(STALE_REMEDIATION_SHELL, STALE_REMEDIATION_UNRELATED),
    );
    assert_eq!(
        classify(&base, &good),
        Ok(vec![Divergence::StaleAdvisoryNoRmForUnprunableEntries])
    );

    // The old text kept, or the `rm` hint left in alongside the new text.
    assert_eq!(
        classify(&base, &with_stderr(base.stderr.replace("rm <path>", "rm -f <path>"))),
        Err(())
    );
    let both = base.stderr.replace(
        STALE_REMEDIATION_SHELL,
        &format!("{STALE_REMEDIATION_UNRELATED}{STALE_REMEDIATION_SHELL}"),
    );
    assert_eq!(classify(&base, &with_stderr(both)), Err(()));
    // A per-entry line changed alongside the replacement.
    assert_eq!(
        classify(&base, &with_stderr(good.stderr.replace("loom-foo", "loom-bar"))),
        Err(())
    );
    // The right stderr with a different exit code.
    assert_eq!(
        classify(
            &base,
            &Answer {
                rc: 0,
                ..good.clone()
            }
        ),
        Err(())
    );
    // A shell answer with no remediation block cannot be explained by it.
    let plain = Answer {
        rc: 3,
        stdout: String::new(),
        stderr: "x\n".to_string(),
    };
    assert_eq!(
        classify(
            &plain,
            &Answer {
                stderr: "y\n".to_string(),
                ..plain.clone()
            }
        ),
        Err(())
    );
}
