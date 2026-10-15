//! The `review_role_pins` health section (#11970).
//!
//! A repo that pins a Champion, Judge or Doctor prompt in
//! `.loom/resync-ignore` keeps running that copy against a newer daemon, and
//! these three roles enforce the label contract the daemon's code assumes. A
//! pinned pre-#11888 `champion-pr-merge.md` re-added `loom:operator` to held
//! PRs, which every role then skips: approved PRs stranded with nothing in
//! `health` to say why. This section names the repo and the pinned files.
//!
//! Conditional: no section unless a managed repo pins one of these files, so
//! a host without such pins renders exactly what it did before. A **warning**,
//! never a verdict: the section is Green and never moves `health`'s exit code,
//! because a repo may pin these files deliberately (a re-ported Champion under
//! an operator ruling), and a permanently Degraded host drowns the signal.
//! The daemon also logs one WARN per such repo at startup
//! ([`pinned_review_roles`], `daemon_startup_reconciliation`).

use super::{HealthInputs, HealthSection, Verdict};

/// The role-file name prefixes whose pins are reported.
const REVIEW_ROLES: [&str; 3] = ["champion", "judge", "doctor"];

/// Whether a `.loom/resync-ignore` pin (canonical label,
/// [`crate::resync_pin::canonical_label`]) names a Champion, Judge or Doctor
/// role or command file.
#[must_use]
pub fn is_review_role_pin(label: &str) -> bool {
    let Some((dir, name)) = label.rsplit_once('/') else {
        return false;
    };
    matches!(dir, "roles" | "commands/loom" | ".claude/commands/loom")
        && name.ends_with(".md")
        && REVIEW_ROLES.iter().any(|r| name.starts_with(r))
}

/// The review-role pins in `root`'s `.loom/resync-ignore`, in file order.
#[must_use]
pub fn pinned_review_roles(root: &std::path::Path) -> Vec<String> {
    crate::resync_pin::read_pins(root)
        .into_iter()
        .filter(|p| is_review_role_pin(p))
        .collect()
}

/// Assess the `review_role_pins` section from each managed root's pins:
/// `Some` (a Green warning, `detail.warning = true`) only when one pins a
/// review role file.
#[must_use]
pub fn assess_review_role_pins(inputs: &HealthInputs) -> Option<HealthSection> {
    let hits: Vec<(&str, Vec<&str>)> = inputs
        .resync_pins
        .iter()
        .filter_map(|(root, pins)| {
            let pinned: Vec<&str> = pins
                .iter()
                .map(String::as_str)
                .filter(|p| is_review_role_pin(p))
                .collect();
            (!pinned.is_empty()).then_some((root.as_str(), pinned))
        })
        .collect();
    if hits.is_empty() {
        return None;
    }
    let named = hits
        .iter()
        .map(|(root, pins)| format!("{root}: {}", pins.join(", ")))
        .collect::<Vec<_>>()
        .join("; ");
    Some(HealthSection::new(
        "review_role_pins",
        Verdict::Green,
        format!(
            "WARNING: {} repo(s) pin Champion/Judge/Doctor prompts in .loom/resync-ignore, so they run \
             stale against this daemon (#11970); lift the pin or upstream the change: {named}",
            hits.len()
        ),
        serde_json::json!({
            "warning": true,
            "repos": hits
                .iter()
                .map(|(root, pins)| serde_json::json!({"root": root, "pins": pins}))
                .collect::<Vec<_>>(),
        }),
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn inputs(pins: &[(&str, &[&str])]) -> HealthInputs {
        HealthInputs {
            resync_pins: pins
                .iter()
                .map(|(r, p)| ((*r).to_string(), p.iter().map(|s| (*s).to_string()).collect()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn review_role_files_are_recognised() {
        for pin in [
            "commands/loom/champion-pr-merge.md",
            "commands/loom/champion.md",
            "commands/loom/judge.md",
            "commands/loom/doctor-pr.md",
            "roles/champion.md",
            "roles/judge.md",
            ".claude/commands/loom/champion-critical-file-hold.md",
        ] {
            assert!(is_review_role_pin(pin), "{pin}");
        }
        for pin in [
            "commands/loom/builder.md",
            "roles/curator.md",
            "scripts/judge-helper.sh",
            "docs/champion-notes.md",
            "champion-pr-merge.md",
            "hooks/guard.sh",
        ] {
            assert!(!is_review_role_pin(pin), "{pin}");
        }
    }

    #[test]
    fn a_pinned_champion_prompt_is_a_named_warning_that_keeps_the_exit_code() {
        let s = assess_review_role_pins(&inputs(&[
            ("/w/a", &["hooks/guard.sh"]),
            (
                "/w/b",
                &[
                    "commands/loom/champion-pr-merge.md",
                    "roles/judge.md",
                    "roles/builder.md",
                ],
            ),
        ]))
        .unwrap();
        assert_eq!(s.verdict, Verdict::Green, "a warning, never a verdict");
        assert_eq!(s.detail["warning"], true);
        assert!(s.summary.starts_with("WARNING: 1 repo(s)"), "{}", s.summary);
        assert!(
            s.summary
                .contains("/w/b: commands/loom/champion-pr-merge.md, roles/judge.md"),
            "{}",
            s.summary
        );
        assert!(!s.summary.contains("/w/a") && !s.summary.contains("builder"), "{}", s.summary);
        assert_eq!(s.detail["repos"][0]["pins"][1], "roles/judge.md");
    }

    #[test]
    fn the_warning_never_changes_the_health_exit_code() {
        let pinned = inputs(&[("/w/b", &["commands/loom/champion-pr-merge.md"])]);
        let with = super::super::assess(&pinned);
        let without = super::super::assess(&inputs(&[]));
        assert!(with.section("review_role_pins").is_some());
        assert!(without.section("review_role_pins").is_none());
        assert_eq!(with.overall, without.overall);
        assert_eq!(with.exit_code(), without.exit_code());
    }

    #[test]
    fn pinned_review_roles_reads_the_ignore_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(pinned_review_roles(dir.path()).is_empty(), "no ignore file");
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/resync-ignore"),
            "hooks/guard.sh
.loom/commands/loom/champion-pr-merge.md # local port
roles/judge.md
",
        )
        .unwrap();
        assert_eq!(
            pinned_review_roles(dir.path()),
            vec!["commands/loom/champion-pr-merge.md", "roles/judge.md"]
        );
    }

    #[test]
    fn no_section_without_a_review_role_pin() {
        assert!(assess_review_role_pins(&inputs(&[])).is_none());
        assert!(assess_review_role_pins(&inputs(&[("/w/a", &[])])).is_none());
        assert!(assess_review_role_pins(&inputs(&[("/w/a", &["roles/builder.md"])])).is_none());
    }
}
