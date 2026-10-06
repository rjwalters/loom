//! The forge half of [`super::pr_features`]: one conditional GET per planned
//! read, through the shared ETag store ([`crate::forge_etag_store`]) and the
//! `gh` facade, so each read uses the repo's reader App when one is usable,
//! is recorded in the forge-call accounting, and costs a free `304` when the
//! answer has not changed. A `304` reuses the stored body.
//!
//! Entries are prefixed `eta-feature-` in the shared store directory, and
//! entries older than [`ENTRY_MAX_AGE`] are pruned after each fresh answer:
//! check-run entries are keyed by commit, so they would otherwise grow.
//!
//! The one read that is not a conditional GET is the base branch's
//! required-context set: it is the lookup `forge wait-checks` uses
//! ([`crate::forge_wait_checks::reads::required_contexts`]), so the estimate
//! and the wait agree about what a branch requires. It is accounted under
//! that lookup's own caller, and the store plans it at most once per base
//! branch per hour. A failed lookup is a failed read, never an empty set.

use super::pr_features::{FeatureRead, ReadKind};
use crate::forge_call_stats::{ops, ForgeOp};
use crate::forge_etag_store::{self as store, ConditionalRead, DiskEntry};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The store filename prefix.
const PREFIX: &str = "eta-feature-";

/// Entries not rewritten for this long are removed.
const ENTRY_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

/// The inventoried operation a read serves.
fn op(kind: ReadKind) -> ForgeOp {
    match kind {
        ReadKind::Pull => ops::PR_VIEW_STATE,
        ReadKind::Issue => ForgeOp::uninventoried("single-issue REST read has no inventory row"),
        ReadKind::Checks => ops::CI_CHECK_RUNS_FOR_SHA,
        // Never used: `fetch` hands this kind to `fetch_required`.
        ReadKind::Required => ForgeOp::uninventoried("required-context lookup accounts itself"),
    }
}

/// How a conditional read settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// `200`: a new body, to store under its ETag.
    Fresh {
        /// The body.
        body: String,
        /// Its ETag, when the answer carried one.
        etag: Option<String>,
    },
    /// `304`: the stored body is still current.
    Reused(String),
}

/// Settle a response: `http` is the status, ETag and body of the answer
/// (`None` when `gh` produced none), `exit_ok` whether `gh` succeeded,
/// `prior` the stored entry sent as `If-None-Match`. `None` means the read
/// failed, including a `304` with nothing stored to reuse.
#[must_use]
pub fn settle(
    http: Option<(u16, Option<String>, String)>,
    exit_ok: bool,
    prior: Option<DiskEntry>,
) -> Option<Settled> {
    match http {
        Some((304, ..)) => prior.map(|p| Settled::Reused(p.body)),
        Some((200, etag, body)) if exit_ok => Some(Settled::Fresh { body, etag }),
        _ => None,
    }
}

/// Run one read from `root` (the repo's checkout, whose credential it uses).
/// The parsed body, or `None` when it failed. A `Checks` read is two calls:
/// the check runs, and the head's combined legacy status filed under
/// `status`. Either failing fails the read, so a required context reported
/// only as a commit status is never inferred pending.
#[must_use]
pub fn fetch(root: &Path, read: &FeatureRead) -> Option<Value> {
    let gh_bin = std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string());
    if read.kind == ReadKind::Required {
        return fetch_required(&gh_bin, read);
    }
    let mut body = get(&gh_bin, root, read, &read.url(), op(read.kind))?;
    if read.kind == ReadKind::Checks {
        let status = get(&gh_bin, root, read, &read.status_url(), op(read.kind))?;
        body.as_object_mut()?.insert("status".to_string(), status);
    }
    Some(body)
}

/// One conditional GET of `url` for `read`'s repo.
fn get(gh_bin: &str, root: &Path, read: &FeatureRead, url: &str, op: ForgeOp) -> Option<Value> {
    let target = store::resolve_target(Some(root), Some(&read.repo));
    let dir = store::disk_cache_dir();
    let path =
        store::entry_path_with_prefix(&dir, PREFIX, &store::cache_key(Some(root), &target, url));
    let prior = store::read_disk_entry(&path);
    let site = ConditionalRead::new("eta_feature_read", op);
    let etag = prior.as_ref().map(|p| p.etag.clone());
    let (status, response, _) = store::fetch_conditional(
        site,
        Path::new(gh_bin),
        Some(root),
        &target,
        url,
        etag.as_deref(),
    )
    .ok()?;
    let http = response.map(|r| (r.status, r.etag, r.body));
    let body = match settle(http, status.success(), prior)? {
        Settled::Reused(body) => body,
        Settled::Fresh { body, etag } => {
            if let Some(etag) = etag {
                store::write_disk_entry(
                    &path,
                    &DiskEntry {
                        etag,
                        body: body.clone(),
                    },
                );
                prune(&dir);
            }
            body
        }
    };
    serde_json::from_str(&body).ok()
}

/// The base branch's required contexts as `{"required_contexts": [...]}`,
/// or `None` when the lookup failed (or the read names no base).
fn fetch_required(gh_bin: &str, read: &FeatureRead) -> Option<Value> {
    let base = read.base.as_deref().filter(|b| !b.is_empty())?;
    let (contexts, _notices) =
        crate::forge_wait_checks::reads::required_contexts(gh_bin, &read.repo, base).ok()?;
    Some(serde_json::json!({ "required_contexts": contexts }))
}

/// Run `reads` in order, each with its repo's root from `roots`, stamping
/// each answer with the instant it returned. A read whose repo has no root
/// is answered as failed.
#[must_use]
pub fn run(
    reads: Vec<FeatureRead>,
    roots: &[(String, PathBuf)],
) -> Vec<(FeatureRead, Option<Value>, DateTime<Utc>)> {
    reads
        .into_iter()
        .map(|read| {
            let root = roots
                .iter()
                .find(|(slug, _)| slug.eq_ignore_ascii_case(&read.repo))
                .map(|(_, root)| root);
            let body = root.and_then(|root| fetch(root, &read));
            (read, body, Utc::now())
        })
        .collect()
}

/// Remove this module's entries last written more than [`ENTRY_MAX_AGE`] ago.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(PREFIX));
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > ENTRY_MAX_AGE);
        if ours && old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn entry(body: &str) -> DiskEntry {
        DiskEntry {
            etag: "W/\"1\"".to_string(),
            body: body.to_string(),
        }
    }

    #[test]
    fn a_304_reuses_the_stored_body() {
        let settled = settle(Some((304, None, String::new())), false, Some(entry("{\"a\":1}")));
        assert_eq!(settled, Some(Settled::Reused("{\"a\":1}".to_string())));
    }

    #[test]
    fn a_304_with_nothing_stored_is_a_failed_read() {
        assert_eq!(settle(Some((304, None, String::new())), false, None), None);
    }

    #[test]
    fn a_200_is_fresh_and_anything_else_fails() {
        let fresh = settle(Some((200, Some("e".into()), "{}".into())), true, Some(entry("old")));
        assert_eq!(
            fresh,
            Some(Settled::Fresh {
                body: "{}".into(),
                etag: Some("e".into())
            })
        );
        assert_eq!(settle(Some((200, None, "{}".into())), false, None), None);
        assert_eq!(settle(Some((404, None, "{}".into())), false, Some(entry("x"))), None);
        assert_eq!(settle(None, false, Some(entry("x"))), None);
    }

    #[test]
    fn urls_name_the_repo_and_kind() {
        let read = |kind, sha: Option<&str>| FeatureRead {
            repo: "o/r".into(),
            kind,
            number: 7,
            sha: sha.map(str::to_string),
            base: sha.map(str::to_string),
        };
        assert_eq!(read(ReadKind::Pull, None).url(), "repos/o/r/pulls/7");
        assert_eq!(read(ReadKind::Issue, None).url(), "repos/o/r/issues/7");
        assert_eq!(
            read(ReadKind::Checks, Some("abc")).url(),
            "repos/o/r/commits/abc/check-runs?per_page=100"
        );
        assert_eq!(read(ReadKind::Required, Some("main")).url(), "repos/o/r/rules/branches/main");
    }

    #[test]
    fn a_read_with_no_root_fails_without_a_call() {
        let read = FeatureRead {
            repo: "o/r".into(),
            kind: ReadKind::Pull,
            number: 1,
            sha: None,
            base: None,
        };
        let answers = run(vec![read.clone()], &[]);
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].0, read);
        assert!(answers[0].1.is_none());
    }

    #[test]
    fn a_required_read_with_no_base_fails_without_a_call() {
        let read = FeatureRead {
            repo: "o/r".into(),
            kind: ReadKind::Required,
            number: 0,
            sha: None,
            base: None,
        };
        // No base branch: a failed read, before any `gh` is run.
        assert!(fetch_required("/nonexistent/gh", &read).is_none());
    }

    /// Review of #10281, finding 1: count the real `gh` invocations a
    /// required lookup makes. It never exceeds what the budget charges.
    #[cfg(unix)]
    #[test]
    fn a_required_lookup_makes_no_more_calls_than_it_is_charged() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("eta-stub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls");
        let stub = dir.join("gh");
        let write = |body: &str| {
            std::fs::write(&stub, format!("#!/bin/sh\necho \"$1\" >> {}\n{body}\n", log.display()))
                .unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _ = std::fs::remove_file(&log);
        };
        let calls = || std::fs::read_to_string(&log).map_or(0, |s| s.lines().count());
        let read = FeatureRead {
            repo: "o/r".into(),
            kind: ReadKind::Required,
            number: 0,
            sha: None,
            base: Some("main".into()),
        };
        let charged = ReadKind::Required.cost();
        // Both sources answer: a ruleset REST call and a GraphQL call.
        write("echo build");
        assert!(fetch_required(stub.to_str().unwrap(), &read).is_some());
        assert_eq!(calls(), charged);
        // The ruleset call fails: the lookup fails, within the same charge.
        write("echo boom >&2; exit 1");
        assert!(fetch_required(stub.to_str().unwrap(), &read).is_none());
        assert!(calls() <= charged);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
