//! Two more facts the self-heal's classifier needs (#11989): which paths the
//! resync owns beyond `installed_files`, and how long a detached HEAD has sat.
//!
//! # Why `installed_files` alone is not enough
//!
//! `installed_files` is written by the installer and never by a resync
//! (`resync-installed.sh` leaves it to the installer). Every file a later
//! release adds is therefore missing from it until the next full install: a
//! new `.loom/scripts/tests/*.sh`, a new `.loom/docs/*.md`, and most of the
//! `.agents/skills/loom-*/SKILL.md` mirror. A stranded `chore: resync installed
//! Loom surfaces` commit always touches some of them, so a classifier that
//! read `installed_files` only left every such commit alone, and the
//! checkouts stayed `diverged` for days (loom-worker-1/2/3, 2026-10-15).
//!
//! The surfaces below are the ones `resync-installed.sh` copies wholesale
//! (its header lists them). A hand edit there does not survive the next
//! resync anyway. A path pinned in `.loom/resync-ignore` is the exception: a
//! pin is a deliberate local divergence, so it is never counted here.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};

/// Directories `resync-installed.sh` overwrites wholesale.
const MANAGED_DIRS: &[&str] = &[
    ".loom/roles/",
    ".loom/scripts/",
    ".loom/docs/",
    ".loom/runtimes/",
    ".loom/bin/",
    ".loom/hooks/",
    ".claude/commands/loom/",
];

/// Single files it overwrites (or stamps, for the metadata).
const MANAGED_FILES: &[&str] = &[
    ".loom/install-metadata.json",
    ".loom/pricing.json",
    ".loom/biome.jsonc",
    ".claude/biome.jsonc",
    ".claude/README.md",
    ".github/CONFIGURATION.md",
];

/// The Codex skill mirror: `.agents/skills/loom-<name>/…`, Loom's namespace.
const SKILLS_PREFIX: &str = ".agents/skills/loom-";

/// How long a detached HEAD must sit unmoved before it is re-attached.
const DETACHED_ENV: &str = "LOOM_CHECKOUT_DETACHED_AFTER_MINUTES";
const DEFAULT_DETACHED_MINUTES: u64 = 360;

/// The resync's own pins, as `.loom/resync-ignore` lists them: comments and
/// blanks dropped, a leading `./` stripped.
pub(super) fn pins(root: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.join(".loom/resync-ignore")) else {
        return Vec::new();
    };
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .map(|l| l.strip_prefix("./").unwrap_or(l))
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Is `path` pinned? A pin names the file in the resync's report form, which
/// is relative to `.loom/` (`scripts/x.sh`) or `.claude/`
/// (`commands/loom/judge.md`), or in full.
fn pinned(path: &str, pins: &[String]) -> bool {
    pins.iter().any(|pin| {
        path == pin
            || path.strip_prefix(".loom/") == Some(pin.as_str())
            || path.strip_prefix(".claude/") == Some(pin.as_str())
    })
}

/// Does the resync overwrite `path` wholesale, and is it not pinned?
pub(super) fn resync_owned(path: &str, pins: &[String]) -> bool {
    let surface = MANAGED_DIRS.iter().any(|d| path.starts_with(d))
        || MANAGED_FILES.contains(&path)
        || path
            .strip_prefix(SKILLS_PREFIX)
            .is_some_and(|rest| rest.contains('/'));
    surface && !pinned(path, pins)
}

/// How long a detached HEAD must sit before the self-heal re-attaches it.
///
/// Long on purpose. A role session that checks out a PR head or `origin/main`
/// in the main checkout is done with it in minutes; a tool installer that
/// keeps a workspace detached at a pinned commit (2am's `provision.sh` does
/// this for geode-fem) moves it at least hourly, and a checkout it keeps
/// current is never behind for long. Days-old detached checkouts are the
/// ones this is for.
pub(super) fn detached_after() -> Duration {
    let minutes = std::env::var(DETACHED_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_DETACHED_MINUTES);
    Duration::from_secs(minutes * 60)
}

/// When HEAD last moved, from the newest line of `<git dir>/logs/HEAD`
/// (`<old> <new> <name> <email> <unix-time> <tz>\t<message>`). `None` when
/// there is no reflog or the line cannot be read: callers treat that as
/// "recent", so nothing is repaired on a guess.
pub(super) fn head_moved_at(gd: &Path) -> Option<DateTime<Utc>> {
    let text = std::fs::read_to_string(gd.join("logs/HEAD")).ok()?;
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    let before_msg = line.split('\t').next()?;
    let mut words = before_msg.split_whitespace().rev();
    let _tz = words.next()?;
    let secs: i64 = words.next()?.parse().ok()?;
    Utc.timestamp_opt(secs, 0).single()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resync_surfaces_are_owned_and_other_paths_are_not() {
        let none: Vec<String> = Vec::new();
        for p in [
            ".loom/scripts/tests/test-new.sh",
            ".loom/docs/daemon-reference.md",
            ".loom/hooks/guard.sh",
            ".claude/commands/loom/builder.md",
            ".agents/skills/loom-builder-complexity/SKILL.md",
            ".loom/install-metadata.json",
            ".github/CONFIGURATION.md",
        ] {
            assert!(resync_owned(p, &none), "{p}");
        }
        for p in [
            ".loom/config.json",
            ".loom/resync-ignore",
            ".claude/commands/repo/scrub.md",
            ".claude/settings.json",
            ".agents/skills/custom/SKILL.md",
            ".agents/skills/loom-",
            ".github/labels.yml",
            ".github/workflows/ci.yml",
            "CLAUDE.md",
            "src/lib.rs",
        ] {
            assert!(!resync_owned(p, &none), "{p}");
        }
    }

    #[test]
    fn a_pinned_path_is_not_owned_in_any_spelling() {
        let pins = vec![
            "scripts/merge-pr.sh".to_string(),
            "commands/loom/judge.md".to_string(),
            ".loom/docs/x.md".to_string(),
        ];
        assert!(!resync_owned(".loom/scripts/merge-pr.sh", &pins));
        assert!(!resync_owned(".claude/commands/loom/judge.md", &pins));
        assert!(!resync_owned(".loom/docs/x.md", &pins));
        assert!(resync_owned(".loom/scripts/worktree.sh", &pins));
    }

    #[test]
    fn pins_are_read_without_comments_blanks_or_a_leading_dot_slash() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        std::fs::write(
            tmp.path().join(".loom/resync-ignore"),
            "# header\n\n./scripts/a.sh  # why\nroles/b.md\n",
        )
        .unwrap();
        assert_eq!(pins(tmp.path()), vec!["scripts/a.sh", "roles/b.md"]);
        assert!(pins(&tmp.path().join("missing")).is_empty());
    }

    #[test]
    fn head_moved_at_reads_the_newest_reflog_line() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("logs")).unwrap();
        let z = "0".repeat(40);
        std::fs::write(
            tmp.path().join("logs/HEAD"),
            format!(
                "{z} {z} A U Thor <a@example.invalid> 1700000000 +0000\tclone\n\
                 {z} {z} A U Thor <a@example.invalid> 1791760325 -0700\tcheckout: moving from main to origin/main\n"
            ),
        )
        .unwrap();
        assert_eq!(head_moved_at(tmp.path()).unwrap().timestamp(), 1_791_760_325);
        assert!(head_moved_at(&tmp.path().join("none")).is_none());
    }
}
