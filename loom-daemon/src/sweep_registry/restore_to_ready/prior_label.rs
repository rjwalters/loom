//! Did the claim being released take `loom:issue` away? (Issue #10955)
//!
//! # The measured bug
//!
//! A claim is a swap: `--remove-label loom:issue --add-label loom:building`.
//! A release used to undo it unconditionally, re-adding `loom:issue`. But an
//! issue can be claimed without ever having carried `loom:issue` (an operator
//! hands a `loom:curated` issue to a Builder by number). When that Builder's
//! pre-flight gave up, the release labeled the issue `loom:issue`, which made
//! an issue no operator had approved eligible for fleet dispatch (#10718).
//!
//! # The rule
//!
//! **A release re-adds `loom:issue` only when the issue carried it when
//! `loom:building` was added.** No other label needs restoring: the claim
//! removes nothing else, so `loom:curated` and the rest are still there.
//!
//! The evidence is the issue's own label history on the forge, replayed up to
//! the latest `labeled loom:building` event ([`carried_ready_at_claim`]).
//!
//! # Fail-open contract
//!
//! Same family as the other carve-outs in
//! [`restore_label_to_ready_with_state_check`](SweepRegistry::restore_label_to_ready_with_state_check):
//! only a positive "it did not carry `loom:issue`" skips the re-add. An
//! unreadable, empty or claim-less history answers `None`, and the caller then
//! re-adds as before, because a stranded claim is the more common failure.

use chrono::{DateTime, Duration, Utc};

use super::*;

const READY_LABEL: &str = "loom:issue";
const CLAIM_LABEL: &str = "loom:building";

/// How far apart the two halves of one claim swap may be stamped. The forge
/// stamps whole seconds and `gh issue edit` may send the removal and the
/// addition as separate requests, in either order.
const SWAP_WINDOW_SECS: i64 = 10;

/// One `<rfc3339>\t<labeled|unlabeled>\t<label>` line per label event.
pub(super) const LABEL_HISTORY_JQ: &str = r#".[] | select(.event == "labeled" or .event == "unlabeled") | "\(.created_at)\t\(.event)\t\(.label.name)""#;

/// One label event from an issue's timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LabelChange {
    at: DateTime<Utc>,
    added: bool,
    label: String,
}

/// Parse [`LABEL_HISTORY_JQ`]'s stdout, oldest first. `--paginate` runs the
/// program once per page, so the pages are merged by timestamp; the sort is
/// stable, which keeps same-second events in the order the forge gave them.
/// Lines that do not parse are skipped.
pub(super) fn parse_label_history(stdout: &[u8]) -> Vec<LabelChange> {
    let raw = String::from_utf8_lossy(stdout);
    let mut changes: Vec<LabelChange> = raw
        .lines()
        .filter_map(|line| {
            let mut f = line.trim_end_matches('\r').splitn(3, '\t');
            let at = DateTime::parse_from_rfc3339(f.next()?.trim().trim_matches('"'))
                .ok()?
                .with_timezone(&Utc);
            let added = match f.next()?.trim() {
                "labeled" => true,
                "unlabeled" => false,
                _ => return None,
            };
            let label = f.next()?.trim();
            (!label.is_empty()).then(|| LabelChange {
                at,
                added,
                label: label.to_string(),
            })
        })
        .collect();
    changes.sort_by_key(|c| c.at);
    changes
}

/// Whether the issue carried `loom:issue` when its latest `loom:building`
/// claim was applied. `None` when the history records no claim at all.
///
/// The claim's own removal of `loom:issue` lands within [`SWAP_WINDOW_SECS`]
/// of the `loom:building` event, on either side of it. So the answer is the
/// last `loom:issue` event up to the end of that window: an addition, or a
/// removal inside the window (the swap), means it was carried; a removal
/// before the window, or no `loom:issue` event at all, means it was not.
pub(super) fn carried_ready_at_claim(history: &[LabelChange]) -> Option<bool> {
    let claimed_at = history
        .iter()
        .rev()
        .find(|c| c.added && c.label == CLAIM_LABEL)?
        .at;
    let window = Duration::seconds(SWAP_WINDOW_SECS);
    let last_ready = history
        .iter()
        .rev()
        .find(|c| c.label == READY_LABEL && c.at <= claimed_at + window);
    Some(match last_ready {
        None => false,
        Some(c) if c.added => true,
        Some(c) => c.at >= claimed_at - window,
    })
}

impl SweepRegistry {
    /// [`carried_ready_at_claim`] for `issue`, read from the forge. `None`
    /// whenever the forge was not successfully consulted (label flips
    /// skipped, breaker open, `gh` failed or timed out) or records no claim.
    pub(super) fn claim_took_ready_label(&self, issue: u32) -> Option<bool> {
        if self.config.skip_label_flip
            || crate::rate_limit_breaker::global_skip_pass("restore_label_to_ready")
        {
            return None;
        }
        // `{owner}/{repo}` is resolved by `gh` against this registry's
        // workspace, with `LOOM_REPO` as `GH_REPO` (never `--repo`, which
        // `gh api` rejects, #8263) — the facade applies both.
        let path = format!("repos/{{owner}}/{{repo}}/issues/{issue}/timeline");
        let args = ["api", path.as_str(), "--paginate", "--jq", LABEL_HISTORY_JQ];
        match self.gh_read("restore.label_timeline", args) {
            Ok(Some(o)) if o.status.success() => {
                carried_ready_at_claim(&parse_label_history(&o.stdout))
            }
            Ok(Some(o)) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                crate::rate_limit_breaker::global_observe_failure(
                    &stderr,
                    "restore_label_timeline",
                );
                log::warn!(
                    "sweep_registry: label history read for #{issue} failed ({}) — falling \
                     back to re-adding `loom:issue` (#10955): {stderr}",
                    o.status
                );
                None
            }
            Ok(None) => {
                log::warn!(
                    "sweep_registry: label history read for #{issue} timed out — falling back \
                     to re-adding `loom:issue` (#10955)"
                );
                None
            }
            Err(e) => {
                log::warn!(
                    "sweep_registry: could not invoke gh for #{issue}'s label history: {e} — \
                     falling back to re-adding `loom:issue` (#10955)"
                );
                None
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn history(rows: &[(&str, &str, &str)]) -> Vec<LabelChange> {
        let joined: String = rows
            .iter()
            .map(|(at, event, label)| format!("{at}\t{event}\t{label}\n"))
            .collect();
        parse_label_history(joined.as_bytes())
    }

    #[test]
    fn parses_and_orders_rows_and_skips_junk() {
        let h = parse_label_history(
            b"2026-10-08T10:00:05Z\tlabeled\tloom:building\n\
              not a row\n\
              2026-10-08T10:00:00Z\tunlabeled\tloom:issue\n\
              2026-10-08T10:00:06Z\trenamed\tx\n",
        );
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].label.as_str(), h[0].added), ("loom:issue", false));
        assert_eq!((h[1].label.as_str(), h[1].added), ("loom:building", true));
    }

    #[test]
    fn a_swap_claim_carried_the_ready_label() {
        // Removal stamped just before the claim …
        let h = history(&[
            ("2026-10-08T09:00:00Z", "labeled", "loom:curated"),
            ("2026-10-08T09:30:00Z", "labeled", "loom:issue"),
            ("2026-10-08T10:00:00Z", "unlabeled", "loom:issue"),
            ("2026-10-08T10:00:01Z", "labeled", "loom:building"),
        ]);
        assert_eq!(carried_ready_at_claim(&h), Some(true));
        // … and just after it (the two requests landed the other way round).
        let h = history(&[
            ("2026-10-08T09:30:00Z", "labeled", "loom:issue"),
            ("2026-10-08T10:00:01Z", "labeled", "loom:building"),
            ("2026-10-08T10:00:02Z", "unlabeled", "loom:issue"),
        ]);
        assert_eq!(carried_ready_at_claim(&h), Some(true));
    }

    #[test]
    fn a_claim_that_left_the_ready_label_in_place_carried_it() {
        let h = history(&[
            ("2026-10-08T09:30:00Z", "labeled", "loom:issue"),
            ("2026-10-08T10:00:00Z", "labeled", "loom:building"),
        ]);
        assert_eq!(carried_ready_at_claim(&h), Some(true));
    }

    #[test]
    fn a_curated_only_issue_never_carried_it() {
        // #10718: claimed by number while only `loom:curated`.
        let h = history(&[
            ("2026-10-08T09:00:00Z", "labeled", "loom:curated"),
            ("2026-10-08T10:00:00Z", "labeled", "loom:building"),
        ]);
        assert_eq!(carried_ready_at_claim(&h), Some(false));
    }

    #[test]
    fn a_ready_label_removed_long_before_the_claim_was_not_carried() {
        let h = history(&[
            ("2026-10-07T09:00:00Z", "labeled", "loom:issue"),
            ("2026-10-07T12:00:00Z", "unlabeled", "loom:issue"),
            ("2026-10-08T10:00:00Z", "labeled", "loom:building"),
        ]);
        assert_eq!(carried_ready_at_claim(&h), Some(false));
    }

    #[test]
    fn only_the_latest_claim_counts() {
        // First claim took `loom:issue`; it was released without it, and the
        // second claim (the one being released now) found it absent.
        let h = history(&[
            ("2026-10-07T09:00:00Z", "labeled", "loom:issue"),
            ("2026-10-07T10:00:00Z", "unlabeled", "loom:issue"),
            ("2026-10-07T10:00:00Z", "labeled", "loom:building"),
            ("2026-10-07T11:00:00Z", "unlabeled", "loom:building"),
            ("2026-10-08T10:00:00Z", "labeled", "loom:building"),
        ]);
        assert_eq!(carried_ready_at_claim(&h), Some(false));
        // A later re-approval is not evidence about the claim either way, but
        // it post-dates the window, so the answer does not change.
        let mut later = h.clone();
        later.extend(history(&[("2026-10-08T12:00:00Z", "labeled", "loom:issue")]));
        assert_eq!(carried_ready_at_claim(&later), Some(false));
    }

    #[test]
    fn a_history_without_a_claim_is_unknown() {
        assert_eq!(carried_ready_at_claim(&[]), None);
        let h = history(&[("2026-10-08T09:30:00Z", "labeled", "loom:issue")]);
        assert_eq!(carried_ready_at_claim(&h), None);
    }

    /// Fake `gh`: an open, unparked issue whose label timeline is `rows`
    /// (or a failing timeline read when `rows` is `None`).
    fn fake_gh(dir: &Path, rows: Option<&str>) -> (PathBuf, PathBuf) {
        let gh = dir.join("fake-gh.sh");
        let log = dir.join("gh-calls.log");
        let timeline = match rows {
            Some(rows) => format!("printf '%s' '{rows}'; exit 0"),
            None => "echo 'gh: HTTP 500' >&2; exit 1".to_string(),
        };
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then echo "false"; exit 0; fi
if [ "$1" = "api" ]; then
  case "$2" in */timeline) {timeline} ;; esac
  case "$*" in
    *".pull_request != null"*) echo "false" ;;
    *".state"*) printf '{{"body":"","state":"open","closed_at":null,"labels":[]}}' ;;
    *) echo "" ;;
  esac
  exit 0
fi
exit 0
"#,
            log = log.display(),
        );
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        (gh, log)
    }

    fn restore(rows: Option<&str>) -> String {
        let dir = tempfile::tempdir().unwrap();
        let (gh, log) = fake_gh(dir.path(), rows);
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(gh);
        config.skip_label_flip = false;
        SweepRegistry::new(config)
            .restore_label_to_ready(10955)
            .unwrap();
        std::fs::read_to_string(&log).unwrap()
    }

    #[test]
    #[serial_test::serial]
    fn release_readds_the_ready_label_the_claim_took() {
        let calls = restore(Some(
            "2026-10-08T09:30:00Z\tlabeled\tloom:issue\n\
             2026-10-08T10:00:00Z\tunlabeled\tloom:issue\n\
             2026-10-08T10:00:00Z\tlabeled\tloom:building\n",
        ));
        assert!(
            calls.contains("issue edit 10955 --remove-label loom:building --add-label loom:issue"),
            "{calls}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn release_does_not_add_a_ready_label_the_issue_never_had() {
        let calls = restore(Some(
            "2026-10-08T09:00:00Z\tlabeled\tloom:curated\n\
             2026-10-08T10:00:00Z\tlabeled\tloom:building\n",
        ));
        assert!(calls.contains("issue edit 10955 --remove-label loom:building"), "{calls}");
        assert!(!calls.contains("--add-label loom:issue"), "{calls}");
    }

    #[test]
    #[serial_test::serial]
    fn an_unreadable_history_falls_back_to_the_readd() {
        let calls = restore(None);
        assert!(calls.contains("/timeline"), "the history was asked for: {calls}");
        assert!(
            calls.contains("issue edit 10955 --remove-label loom:building --add-label loom:issue"),
            "{calls}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn a_history_with_no_claim_falls_back_to_the_readd() {
        let calls = restore(Some(""));
        assert!(calls.contains("--add-label loom:issue"), "{calls}");
    }
}
