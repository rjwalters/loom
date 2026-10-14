//! Demotion of issues *opened with* `loom:issue` or `loom:curated` (#11820).
//!
//! Operator ruling (2026-10-13, 2AMLogic/2am#4408): issues enter at
//! `loom:triage`. An issue opened already carrying `loom:issue` or
//! `loom:curated` skips intake, the Curator and approval, so the intake pass
//! takes those labels off again, adds `loom:triage`, and says why in one
//! comment. A *later* promotion (Champion, a person, or the author after the
//! grace window) is a legitimate transition and is never touched.
//!
//! Both intake paths run the same step ([`demote_step`]) over the same pure
//! selector ([`select_demote_candidates`]): the inline per-host pass
//! ([`super::run_once`], through [`InlineForge`]) and the fleet-captain
//! singleton ([`super::singleton::run_repo`], whose `IntakeForge` extends
//! [`DemoteForge`]). Neither can drift from the other.
//!
//! # Detection ("opened with")
//!
//! 1. **Prefilter, free**, from the listing the pass already has: an open
//!    issue whose `loom:*` labels are a subset of [`DEMOTE_LABELS`] plus
//!    [`DEMOTE_COMPANIONS`] and include one of [`DEMOTE_LABELS`]. Anything
//!    carrying another lifecycle label (`loom:building`, `loom:curating`,
//!    `loom:blocked`, `loom:architect`, `loom:epic*`, …) is past intake or
//!    architect-originated and left alone. Age at least `MIN_AGE_SECS` (the
//!    filer's grace, as for the unlabelled selector) and at most the lookback
//!    window (`LOOM_INTAKE_RECONCILE_DEMOTE_WINDOW_SECS`, default 86400), so
//!    the standing backlog is never re-walked.
//! 2. **Confirm**, one issue-events read per candidate: for each of
//!    [`DEMOTE_LABELS`] the issue carries, the latest `labeled` event for it
//!    must be by the issue's author and within `MIN_AGE_SECS` of creation.
//!    Any carried label applied later or by someone else is a promotion: the
//!    issue is left alone and remembered ([`PROMOTIONS`]) so it is read at
//!    most once per process. A failed or shed read demotes nothing.
//! 3. **Cost**: events reads count against the pass's per-repo cap, oldest
//!    first; the unlabelled-to-triage step gets what is left.
//! 4. **Demote**: re-read the labels immediately before writing; add
//!    `loom:triage`, remove each demoted label, then post the comment. Labels
//!    first, so a failure can never leave a comment with no demotion. When
//!    the re-read already shows `loom:triage` (an earlier pass got part way)
//!    only the labels are fixed. A fully demoted issue carries no
//!    [`DEMOTE_LABELS`] and drops out of the prefilter, so the comment is
//!    posted once.
//!
//! No new telemetry: each demotion is a label change, which the forge's
//! webhook ingest already records as `label.transition`.

use super::{gh_call, IntakeRow, MIN_AGE_SECS, TRIAGE_LABEL};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, Utc};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

/// The labels an issue may not be opened with.
pub const DEMOTE_LABELS: [&str; 2] = ["loom:issue", "loom:curated"];
/// `loom:*` labels that may accompany a [`DEMOTE_LABELS`] label on a
/// candidate: priority markers, and `loom:triage` itself (an earlier pass
/// that added it and then failed to remove the rest).
pub const DEMOTE_COMPANIONS: [&str; 3] = ["loom:very-important", "loom:important", TRIAGE_LABEL];
/// Default lookback (`LOOM_INTAKE_RECONCILE_DEMOTE_WINDOW_SECS` overrides).
pub const DEFAULT_WINDOW_SECS: i64 = 86_400;
/// The one comment a demoted issue gets.
pub const DEMOTE_COMMENT: &str = "Opened as `loom:issue`/`loom:curated`; issues enter at \
`loom:triage`, so this goes through Curator admission first.";

/// The configured lookback window, in seconds.
#[must_use]
pub fn window_secs() -> i64 {
    super::env_num("LOOM_INTAKE_RECONCILE_DEMOTE_WINDOW_SECS", DEFAULT_WINDOW_SECS)
}

/// Whether `labels` qualify an issue for demotion: every `loom:*` label is a
/// [`DEMOTE_LABELS`] or [`DEMOTE_COMPANIONS`] entry, and at least one is a
/// [`DEMOTE_LABELS`] entry.
#[must_use]
pub fn demote_eligible(labels: &[String]) -> bool {
    let mut any = false;
    for l in labels.iter().filter(|l| l.starts_with("loom:")) {
        if DEMOTE_LABELS.contains(&l.as_str()) {
            any = true;
        } else if !DEMOTE_COMPANIONS.contains(&l.as_str()) {
            return false;
        }
    }
    any
}

/// Candidates for demotion: eligible labels, aged within
/// `[MIN_AGE_SECS, window_secs]`, not `skip`ped (a remembered promotion),
/// oldest first, at most `cap`. A row with no creation time is never a
/// candidate: its age, and so the window, is unknown. Pure.
#[must_use]
pub fn select_demote_candidates(
    rows: &[IntakeRow],
    now: DateTime<Utc>,
    window_secs: i64,
    cap: usize,
    skip: impl Fn(u32) -> bool,
) -> Vec<&IntakeRow> {
    let mut picked: Vec<&IntakeRow> = rows
        .iter()
        .filter(|r| demote_eligible(&r.labels))
        .filter(|r| {
            r.created_at.is_some_and(|c| {
                let age = now - c;
                age >= Duration::seconds(MIN_AGE_SECS) && age <= Duration::seconds(window_secs)
            })
        })
        .filter(|r| !skip(r.number))
        .collect();
    picked.sort_by_key(|r| (r.created_at, r.number));
    picked.truncate(cap);
    picked
}

/// One `labeled` event from an issue's events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelEvent {
    pub label: String,
    pub actor: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

/// The jq that reduces `GET …/issues/N/events` to `label<TAB>actor<TAB>at`
/// lines, `labeled` events only.
pub const EVENTS_JQ: &str = r#".[] | select(.event == "labeled") | [.label.name, (.actor.login // ""), .created_at] | @tsv"#;

/// Parse [`EVENTS_JQ`] output.
#[must_use]
pub fn parse_label_events(stdout: &str) -> Vec<LabelEvent> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let label = parts.next()?.trim();
            if label.is_empty() {
                return None;
            }
            let actor = parts
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let created_at = parts
                .next()
                .and_then(|s| DateTime::parse_from_rfc3339(s.trim()).ok())
                .map(|d| d.with_timezone(&Utc));
            Some(LabelEvent {
                label: label.to_owned(),
                actor,
                created_at,
            })
        })
        .collect()
}

/// The [`DEMOTE_LABELS`] `row` carries, when every one of them was opened
/// with (its latest `labeled` event is the author's, within `MIN_AGE_SECS`
/// of creation); `None` when any is a later promotion, or the facts to tell
/// are missing (no author, no creation time, no matching event). Pure.
#[must_use]
pub fn opened_with(row: &IntakeRow, events: &[LabelEvent]) -> Option<Vec<String>> {
    let author = row.author.as_deref()?;
    let created = row.created_at?;
    let carried: Vec<String> = row
        .labels
        .iter()
        .filter(|l| DEMOTE_LABELS.contains(&l.as_str()))
        .cloned()
        .collect();
    if carried.is_empty() {
        return None;
    }
    for label in &carried {
        let latest = events
            .iter()
            .filter(|e| &e.label == label)
            .max_by_key(|e| e.created_at)?;
        let by_author = latest
            .actor
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(author));
        let at_creation = latest
            .created_at
            .is_some_and(|at| at - created <= Duration::seconds(MIN_AGE_SECS));
        if !by_author || !at_creation {
            return None;
        }
    }
    Some(carried)
}

// ---------------------------------------------------------------------------
// The negative cache
// ---------------------------------------------------------------------------

/// `(repo key, issue)` pairs verified as later promotions: never read again
/// in this process. Bounded by the lookback window in practice (a candidate
/// ages out of the prefilter after it).
static PROMOTIONS: LazyLock<Mutex<HashSet<(String, u32)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Whether `(key, number)` is a remembered promotion.
#[must_use]
pub fn is_known_promotion(key: &str, number: u32) -> bool {
    PROMOTIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&(key.to_owned(), number))
}

fn remember_promotion(key: &str, number: u32) {
    PROMOTIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert((key.to_owned(), number));
}

/// [`select_demote_candidates`] with the remembered promotions of `key`
/// skipped.
#[must_use]
pub fn candidates<'a>(
    key: &str,
    rows: &'a [IntakeRow],
    now: DateTime<Utc>,
    cap: usize,
) -> Vec<&'a IntakeRow> {
    select_demote_candidates(rows, now, window_secs(), cap, |n| is_known_promotion(key, n))
}

// ---------------------------------------------------------------------------
// The step
// ---------------------------------------------------------------------------

/// The forge calls of a demotion, injected so the step is testable offline.
/// `R` names the repo: the singleton's `Repo`, or the inline path's
/// workspace root.
pub trait DemoteForge<R: ?Sized> {
    /// Issue `number`'s labels as of now, or `None` when it is no longer an
    /// open issue (closed, deleted, or a pull request).
    ///
    /// # Errors
    /// The read failed: nothing is labelled on a guess.
    fn current_labels(&mut self, repo: &R, number: u32) -> Result<Option<Vec<String>>>;

    /// Add [`TRIAGE_LABEL`] to issue `number`: the issue's label set as the
    /// forge answered the write (`Some(vec![])` when the answer did not
    /// parse), or `None` on any failure.
    fn add_triage(&mut self, repo: &R, number: u32) -> Option<Vec<String>>;

    /// The `labeled` events of issue `number`.
    ///
    /// # Errors
    /// The read failed or was shed: nothing is demoted on a guess.
    fn label_events(&mut self, repo: &R, number: u32) -> Result<Vec<LabelEvent>>;

    /// Remove `label` from issue `number`; already absent is success.
    fn remove_label(&mut self, repo: &R, number: u32, label: &str) -> bool;

    /// Post `body` as a comment on issue `number`.
    fn comment(&mut self, repo: &R, number: u32, body: &str) -> bool;
}

/// What one [`demote_step`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DemoteOutcome {
    /// Issues demoted to [`TRIAGE_LABEL`].
    pub demoted: usize,
    /// Events reads spent (counted against the pass's cap).
    pub reads: usize,
    /// A read or write failed: the caller's pass stops here, as the
    /// unlabelled-to-triage step does on a failed write.
    pub stopped: bool,
}

/// Demote each of `picked` (from [`candidates`]) that was opened with a
/// [`DEMOTE_LABELS`] label. `key` names the repo in the negative cache and
/// `name` in log lines.
pub fn demote_step<R: ?Sized, F: DemoteForge<R> + ?Sized>(
    forge: &mut F,
    repo: &R,
    key: &str,
    name: &str,
    picked: &[&IntakeRow],
) -> DemoteOutcome {
    let mut out = DemoteOutcome::default();
    for row in picked {
        let number = row.number;
        out.reads += 1;
        let events = match forge.label_events(repo, number) {
            Ok(events) => events,
            Err(e) => {
                log::warn!(
                    "intake_reconcile: could not read {name}#{number}'s events; nothing demoted \
                     on a guess: {e:#}"
                );
                out.stopped = true;
                break;
            }
        };
        let Some(carried) = opened_with(row, &events) else {
            remember_promotion(key, number);
            continue;
        };
        // The listing is a snapshot: confirm, immediately before the write,
        // that the issue still qualifies.
        let labels = match forge.current_labels(repo, number) {
            Ok(Some(labels)) if demote_eligible(&labels) => labels,
            Ok(_) => continue,
            Err(e) => {
                log::warn!(
                    "intake_reconcile: could not re-read {name}#{number}; nothing demoted on a \
                     guess: {e:#}"
                );
                out.stopped = true;
                break;
            }
        };
        let had_triage = labels.iter().any(|l| l == TRIAGE_LABEL);
        if !had_triage && forge.add_triage(repo, number).is_none() {
            log::warn!("intake_reconcile: failed to add {TRIAGE_LABEL} to {name}#{number}");
            out.stopped = true;
            break;
        }
        let removed = carried
            .iter()
            .filter(|l| labels.contains(l))
            .all(|l| forge.remove_label(repo, number, l));
        if !removed {
            log::warn!(
                "intake_reconcile: failed to remove {carried:?} from {name}#{number}; retried next \
                 pass"
            );
            out.stopped = true;
            break;
        }
        out.demoted += 1;
        if had_triage {
            log::info!(
                "intake_reconcile: {name}#{number} finished its demotion to {TRIAGE_LABEL} \
                 (#11820)"
            );
            continue;
        }
        if !forge.comment(repo, number, DEMOTE_COMMENT) {
            // The demotion stands; only the explanation is missing.
            log::warn!("intake_reconcile: demoted {name}#{number} but its comment failed");
        }
        log::info!(
            "intake_reconcile: {name}#{number} was opened with {carried:?}; demoted to \
             {TRIAGE_LABEL} (#11820)"
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Live calls, shared by both paths
// ---------------------------------------------------------------------------

fn host_args(host: Option<&str>) -> Vec<String> {
    host.map(|h| vec!["--hostname".to_string(), h.to_string()])
        .unwrap_or_default()
}

/// `GET {issue}/events`, reduced to `labeled` events. `issue` is the issue's
/// REST path (`repos/<slug>/issues/N`, or the `{owner}/{repo}` form).
///
/// # Errors
/// The read failed or was shed.
pub fn read_label_events(
    gh_bin: &Path,
    root: &Path,
    issue: &str,
    host: Option<&str>,
) -> Result<Vec<LabelEvent>> {
    let url = format!("{issue}/events?per_page=100");
    let outcome = gh_call::read("intake.label_events", gh_bin, root)
        .read_class(crate::gh_invocation::ReadClass::Hygiene)
        .args(["api", "--paginate", &url, "--jq", EVENTS_JQ])
        .args(host_args(host))
        .run();
    match outcome {
        crate::cmd_out::CmdOutcome::Ran(out) if out.status.success() => {
            Ok(parse_label_events(&String::from_utf8_lossy(&out.stdout)))
        }
        crate::cmd_out::CmdOutcome::Ran(out) => {
            Err(anyhow!("gh api {url} failed: {}", gh_call::stderr(&out)))
        }
        crate::cmd_out::CmdOutcome::Unavailable(u) => Err(anyhow!("gh api {url} unavailable: {u}")),
    }
}

/// `DELETE {issue}/labels/<label>`; already absent is success.
pub fn delete_label(
    gh_bin: &Path,
    root: &Path,
    issue: &str,
    label: &str,
    host: Option<&str>,
) -> bool {
    let path = format!("{issue}/labels/{label}");
    let call = gh_call::write("intake.remove_label", gh_bin, root)
        .args(["api", "-X", "DELETE", &path])
        .args(host_args(host));
    match gh_call::output(call) {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            let err = gh_call::stderr(&o);
            err.contains("404") || err.to_ascii_lowercase().contains("does not exist")
        }
        Err(_) => false,
    }
}

/// `POST {issue}/comments`.
pub fn post_comment(
    gh_bin: &Path,
    root: &Path,
    issue: &str,
    body: &str,
    host: Option<&str>,
) -> bool {
    let path = format!("{issue}/comments");
    let field = format!("body={body}");
    let call = gh_call::write("intake.demote_comment", gh_bin, root)
        .args(["api", "-X", "POST", &path, "-f", &field])
        .args(host_args(host));
    gh_call::output(call).is_ok_and(|o| o.status.success())
}

/// The inline path's forge: `gh api` on `{owner}/{repo}` from a workspace
/// root, exactly as [`super::run_once`]'s listing and label writes are.
pub struct InlineForge {
    pub gh_bin: PathBuf,
}

fn inline_issue(number: u32) -> String {
    format!("repos/{{owner}}/{{repo}}/issues/{number}")
}

impl DemoteForge<Path> for InlineForge {
    fn current_labels(&mut self, root: &Path, number: u32) -> Result<Option<Vec<String>>> {
        // A failed read (a 404 included) is an error: nothing is demoted on
        // a guess, and a deleted issue needs no demotion.
        let url = inline_issue(number);
        let out = gh_call::output(
            gh_call::read("intake.recheck", &self.gh_bin, root).args(["api", &url]),
        )?;
        if !out.status.success() {
            return Err(anyhow!("gh api {url} failed: {}", gh_call::stderr(&out)));
        }
        super::singleton::open_issue_labels(&String::from_utf8_lossy(&out.stdout))
    }

    fn add_triage(&mut self, root: &Path, number: u32) -> Option<Vec<String>> {
        let path = format!("{}/labels", inline_issue(number));
        let label = format!("labels[]={TRIAGE_LABEL}");
        let out = gh_call::output(
            gh_call::write("intake.add_triage", &self.gh_bin, root)
                .args(["api", "-X", "POST", &path, "-f", &label]),
        )
        .ok()
        .filter(|o| o.status.success())?;
        Some(
            super::singleton::label_names(&String::from_utf8_lossy(&out.stdout))
                .unwrap_or_default(),
        )
    }

    fn label_events(&mut self, root: &Path, number: u32) -> Result<Vec<LabelEvent>> {
        read_label_events(&self.gh_bin, root, &inline_issue(number), None)
    }

    fn remove_label(&mut self, root: &Path, number: u32, label: &str) -> bool {
        delete_label(&self.gh_bin, root, &inline_issue(number), label, None)
    }

    fn comment(&mut self, root: &Path, number: u32, body: &str) -> bool {
        post_comment(&self.gh_bin, root, &inline_issue(number), body, None)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "demote_tests.rs"]
mod tests;
