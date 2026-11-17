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
//! A path is Loom's here only when the payload embedded in this binary ships
//! a file at that destination. The resync never touches a file with no
//! upstream counterpart (a repo's `.loom/roles/custom-role.md`), so a
//! directory prefix would call the repo's own work Loom's. A path pinned in
//! `.loom/resync-ignore` is the other exception: a pin is a deliberate local
//! divergence, so it is never counted here.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};

use crate::agent_skills::MARKER;
use crate::init::payload::{materialize_with, Payload};

/// The Codex skill mirror. A skill file is the resync's only when it carries
/// [`MARKER`] (`resync_agent_skills`); the classifier checks that per file.
const SKILLS_PREFIX: &str = ".agents/skills/";

/// Surfaces the resync refreshes only where the repo already has the file.
/// Pre-created in the empty destination so the payload reports them.
const COPY_IF_PRESENT: &[&str] = &[".claude/README.md", ".github/CONFIGURATION.md"];

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
/// (`commands/loom/judge.md`), `agents-skills/<rel>` for `.agents/skills/<rel>`,
/// or in full.
fn pinned(path: &str, pins: &[String]) -> bool {
    pins.iter().any(|pin| {
        path == pin
            || path.strip_prefix(".loom/") == Some(pin.as_str())
            || path.strip_prefix(".claude/") == Some(pin.as_str())
            || path
                .strip_prefix(SKILLS_PREFIX)
                .is_some_and(|rest| pin.strip_prefix("agents-skills/") == Some(rest))
    })
}

/// Every destination the embedded payload ships a file at. `None` when the
/// payload cannot be staged: nothing is then owned beyond `installed_files`.
fn shipped() -> Option<&'static BTreeSet<String>> {
    static SHIPPED: OnceLock<Option<BTreeSet<String>>> = OnceLock::new();
    SHIPPED
        .get_or_init(|| {
            let payload = Payload::embedded().ok()?;
            let dest = tempfile::Builder::new()
                .prefix("loom-shipped-")
                .tempdir()
                .ok()?;
            for rel in COPY_IF_PRESENT {
                let file = dest.path().join(rel);
                std::fs::create_dir_all(file.parent()?).ok()?;
                std::fs::write(file, "").ok()?;
            }
            let diff = materialize_with(&payload, dest.path()).ok()?;
            Some(diff.added.iter().chain(&diff.changed).cloned().collect())
        })
        .as_ref()
}

/// Does the payload ship `path`, and is it not pinned? A skill also needs
/// its generation marker: see [`is_skill`] and [`has_marker`].
pub(super) fn resync_owned(path: &str, pins: &[String]) -> bool {
    shipped().is_some_and(|set| set.contains(path)) && !pinned(path, pins)
}

/// Is `path` in the Codex skill mirror?
pub(super) fn is_skill(path: &str) -> bool {
    path.starts_with(SKILLS_PREFIX)
}

/// Does `content` carry the generation marker that makes a skill the
/// resync's?
pub(super) fn has_marker(content: &str) -> bool {
    content.contains(MARKER)
}

/// The repo-scoped config OpenCode rewrites (#13970): committed by the squad
/// installer, never by Loom, yet its tool-added `$schema` entry used to hold
/// a primary checkout for an operator.
pub(super) const OPENCODE_CONFIG: &str = "opencode.json";

/// The one entry OpenCode adds to [`OPENCODE_CONFIG`] when the committed
/// file lacks it.
const OPENCODE_SCHEMA_ENTRY: &str = "\"$schema\": \"https://opencode.ai/config.json\"";

/// Is a line exactly the `$schema` entry OpenCode adds? Indentation and the
/// trailing comma of a non-final entry do not count; a different URL does.
fn is_opencode_schema_entry(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == OPENCODE_SCHEMA_ENTRY || trimmed.strip_suffix(',') == Some(OPENCODE_SCHEMA_ENTRY)
}

/// Is a working-tree `opencode.json` exactly the committed one plus the one
/// `$schema` entry OpenCode adds (#13970)? Lines compare without their
/// surrounding blanks, so CRLF and a missing trailing newline do not count;
/// any other added, removed or edited line — and a file with no `$schema`
/// entry at all — is a real change and `false` here.
pub(super) fn opencode_schema_only(working: &str, head: &str) -> bool {
    let mut added = 0;
    let mut rest = Vec::new();
    for line in working.lines() {
        if is_opencode_schema_entry(line) {
            added += 1;
        } else {
            rest.push(line.trim());
        }
    }
    added > 0 && rest == head.lines().map(str::trim).collect::<Vec<_>>()
}

/// How long a detached HEAD must sit before the self-heal re-attaches it.
///
/// Long on purpose. A role session that checks out a PR head or `origin/main`
/// in the main checkout is done with it in minutes; a tool installer that
/// keeps a registered workspace detached at a pinned commit moves it at least
/// hourly, and a checkout it keeps current is never behind for long. Days-old
/// detached checkouts are the ones this is for.
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
    fn only_what_the_payload_ships_is_owned() {
        let none: Vec<String> = Vec::new();
        assert!(resync_owned(".loom/scripts/worktree.sh", &none));
        assert!(resync_owned(".loom/roles/builder.md", &none));
        assert!(resync_owned(".claude/commands/loom/builder.md", &none));
        assert!(resync_owned(".agents/skills/loom-builder/SKILL.md", &none));
        for p in [
            ".loom/roles/custom-role.md",
            ".loom/scripts/my-local-helper.sh",
            ".claude/commands/loom/mine.md",
            ".agents/skills/loom-custom/SKILL.md",
            ".loom/config.json",
            ".claude/settings.json",
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
            "scripts/worktree.sh".to_string(),
            "commands/loom/builder.md".to_string(),
            "agents-skills/loom-builder/SKILL.md".to_string(),
            ".loom/roles/judge.md".to_string(),
        ];
        assert!(!resync_owned(".loom/scripts/worktree.sh", &pins));
        assert!(!resync_owned(".claude/commands/loom/builder.md", &pins));
        assert!(!resync_owned(".agents/skills/loom-builder/SKILL.md", &pins));
        assert!(!resync_owned(".loom/roles/judge.md", &pins));
        assert!(resync_owned(".loom/roles/builder.md", &pins));
    }

    #[test]
    fn the_skill_marker_is_what_makes_a_skill_the_resyncs() {
        assert!(is_skill(".agents/skills/loom-x/SKILL.md"));
        assert!(!is_skill(".loom/scripts/x.sh"));
        assert!(has_marker(&format!("---\n---\n{MARKER}\nbody")));
        assert!(!has_marker("---\n---\nbody"));
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

    /// The one-line diff OpenCode leaves, as observed in production (#13970).
    const COMMITTED: &str = "{\n  \"mcp\": {\n    \"squad\": {}\n  }\n}\n";
    const WITH_SCHEMA: &str = "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \
                               \"mcp\": {\n    \"squad\": {}\n  }\n}\n";

    #[test]
    fn the_opencode_schema_entry_alone_is_not_a_real_change() {
        assert!(opencode_schema_only(WITH_SCHEMA, COMMITTED));
    }

    #[test]
    fn opencode_schema_indentation_crlf_and_trailing_newline_do_not_count() {
        assert!(opencode_schema_only(
            "{\r\n        \"$schema\": \"https://opencode.ai/config.json\",\r\n  \
             \"mcp\": {\n    \"squad\": {}\n  }\n}",
            COMMITTED
        ));
    }

    #[test]
    fn a_first_entry_without_a_trailing_comma_is_still_the_schema_entry() {
        assert!(opencode_schema_only(
            "{\n  \"$schema\": \"https://opencode.ai/config.json\"\n}",
            "{\n}\n",
        ));
    }

    #[test]
    fn any_other_opencode_change_is_a_real_change() {
        // One edited line beside the schema entry.
        assert!(!opencode_schema_only(
            "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\": \
             {\n    \"squad\": {\"other\": true}\n  }\n}\n",
            COMMITTED
        ));
        // A different schema URL.
        assert!(!opencode_schema_only(
            "{\n  \"$schema\": \"https://example.invalid/config.json\",\n  \
             \"mcp\": {\n    \"squad\": {}\n  }\n}\n",
            COMMITTED
        ));
        // The schema entry removed, not added.
        assert!(!opencode_schema_only(COMMITTED, WITH_SCHEMA));
        // No `$schema` entry at all: a dirty file identical to HEAD is not
        // owned on a guess.
        assert!(!opencode_schema_only(COMMITTED, COMMITTED));
        // An extra key added (its comma edits the neighbour line too).
        assert!(!opencode_schema_only(
            "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\": \
             {\n    \"squad\": {},\n    \"mine\": true\n  }\n}\n",
            COMMITTED
        ));
    }

    #[test]
    fn only_the_schema_url_opencode_ships_matches() {
        assert!(is_opencode_schema_entry(
            "  \"$schema\": \"https://opencode.ai/config.json\",\n"
        ));
        assert!(!is_opencode_schema_entry(
            "  \"$schema\":\"https://opencode.ai/config.json\",\n"
        ));
        assert!(!is_opencode_schema_entry("  \"$schema\": null,\n"));
    }
}
