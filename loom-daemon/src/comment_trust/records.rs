//! Trust-filtered forge records for the marker readers outside the verdict
//! path (#9548, High slice).
//!
//! The readers here all have the same shape: fetch comments (or a body, or a
//! linked PR) from the forge, look for a Loom marker, change behaviour on what
//! they find. Each one now asks the same question first, through
//! [`TrustPolicy`]: *was this written by someone whose markers we believe?*
//! An untrusted record is dropped before any marker is read, so a well-formed
//! marker from an outsider or a foreign fleet reads exactly as if it were
//! absent.
//!
//! Two transports feed them:
//!
//! - **`--jq` projections** (lease, claim-activity): the jq keeps its marker
//!   pre-filter and also projects [`AUTHOR_JQ`], so the payload stays small
//!   and the author survives to the Rust side, where the policy decides.
//! - **Whole REST listings** ([`fetch_trusted_comments`],
//!   [`fetch_issue_object`]): for readers that previously used `gh … --json
//!   comments`, whose author spelling cannot name an App (see the parent
//!   module's docs).

use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::TrustPolicy;

/// The jq object fields that carry a REST comment's author, for splicing into
/// a `--jq` projection (`{id, body, <AUTHOR_JQ>}`). [`super::Author::from_json`]
/// reads exactly these.
pub const AUTHOR_JQ: &str =
    "user: {login: .user.login, type: .user.type}, author_association: .author_association";

/// Test fixtures: the JSON fields (no braces) of a comment by this fleet's
/// default App, which every [`TrustPolicy`] believes.
#[cfg(test)]
pub const TEST_FLEET_AUTHOR: &str =
    r#""user":{"login":"loom-fleet-dispatch[bot]","type":"Bot"},"author_association":"NONE""#;

/// Test fixtures: every NDJSON object line of `stdout` that names no author
/// gains [`TEST_FLEET_AUTHOR`], so a pre-#9548 fixture keeps modelling the
/// fleet's own comments.
#[cfg(test)]
#[must_use]
pub fn with_fleet_author(stdout: &str) -> String {
    stdout
        .split('\n')
        .map(|line| match line.find('{') {
            Some(i)
                if !line.contains("\"user\"") && line[i + 1..].trim_start().starts_with('"') =>
            {
                format!("{}{{{TEST_FLEET_AUTHOR},{}", &line[..i], &line[i + 1..])
            }
            _ => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse a stream of JSON records: NDJSON objects (a `--jq` projection),
/// arrays of objects, or concatenated arrays (a paginated listing). `None`
/// when any value is not an object or an array. Empty input is `Some([])`: a
/// `--jq` filter that matched nothing prints nothing, and the caller has
/// already checked `gh`'s exit status.
#[must_use]
pub fn parse_records(bytes: &[u8]) -> Option<Vec<Value>> {
    let mut out = Vec::new();
    for value in serde_json::Deserializer::from_slice(bytes).into_iter::<Value>() {
        match value.ok()? {
            Value::Array(items) => out.extend(items),
            v @ Value::Object(_) => out.push(v),
            _ => return None,
        }
    }
    Some(out)
}

/// A marker at the very start of `body` (leading whitespace ignored): the way
/// Loom writes its record comments (lease, quarantine). A marker quoted inside
/// someone's prose does not start the comment.
#[must_use]
pub fn anchored(body: &str, marker: &str) -> bool {
    body.trim_start().starts_with(marker)
}

/// The latest RFC-3339 `field` among `records` (e.g. `updated_at`).
#[must_use]
pub fn max_timestamp(records: &[Value], field: &str) -> Option<DateTime<Utc>> {
    records
        .iter()
        .filter_map(|r| r.get(field).and_then(Value::as_str))
        .filter_map(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .max()
}

impl TrustPolicy {
    /// The trusted records of [`parse_records`] output, in order. `None` when
    /// the output does not parse (a caller treats that as "could not read",
    /// never as "no marker").
    #[must_use]
    pub fn trusted_records(&self, bytes: &[u8]) -> Option<Vec<Value>> {
        parse_records(bytes).map(|items| self.filter(items))
    }

    /// The trusted comments of a whole REST listing (`gh api …/comments
    /// --paginate` stdout, NOT a `--jq` projection), oldest first. Unlike
    /// [`Self::trusted_records`], empty output is `None`: a listing with no
    /// comments prints `[]`, so nothing at all means the read did not happen
    /// (Judge #9593, mirroring [`super::parse_listing`]'s #9566 rule). A
    /// caller treats `None` as "could not read", never as "no marker".
    #[must_use]
    pub fn trusted_listing(&self, listing: &[u8]) -> Option<Vec<Value>> {
        super::parse_listing(listing).map(|items| self.filter(items))
    }

    /// `stdout` (NDJSON, one record per line) keeping only trusted lines, for
    /// a caller whose own parser reads NDJSON. Unparseable lines are dropped:
    /// they carry no author, so no rule could trust them.
    #[must_use]
    pub fn trusted_ndjson(&self, stdout: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for line in String::from_utf8_lossy(stdout).lines() {
            let keep = serde_json::from_str::<Value>(line.trim())
                .is_ok_and(|v| v.is_object() && self.trusts_json(&v));
            if keep {
                out.extend_from_slice(line.trim().as_bytes());
                out.push(b'\n');
            }
        }
        out
    }

    /// Whether the record's author is known and untrusted. A record without
    /// any author fields is *unknown*, not untrusted: the linked-PR probes use
    /// this so an unexpected payload shape errs toward "a PR exists" (the
    /// non-destructive answer for dispatch and orphan recovery), while every
    /// real response, which always carries the author, is judged.
    #[must_use]
    pub fn known_untrusted(&self, v: &Value) -> bool {
        let has_author = ["user", "author", "author_association", "authorAssociation"]
            .iter()
            .any(|k| v.get(*k).is_some_and(|f| !f.is_null()));
        has_author && !self.trusts_json(v)
    }

    /// H14: a closes-graph GraphQL payload with every OPEN node that is BOTH
    /// from a fork (`isCrossRepository: true`) and by an untrusted author
    /// removed. A same-repo branch needs write access to push, so it always
    /// counts. Returns `stdout` unchanged when it does not parse, so the
    /// caller's own parser still reports the failure.
    #[must_use]
    pub fn drop_untrusted_fork_prs(&self, stdout: &str) -> String {
        let Ok(mut v) = serde_json::from_str::<Value>(stdout) else {
            return stdout.to_string();
        };
        if let Some(Value::Array(nodes)) =
            v.pointer_mut("/data/repository/issue/closedByPullRequestsReferences/nodes")
        {
            nodes.retain(|n| {
                n.get("isCrossRepository").and_then(Value::as_bool) != Some(true)
                    || !self.known_untrusted(n)
            });
        }
        v.to_string()
    }

    /// H14, the REST timeline leg: candidate lines (`{number, body, <author>}`)
    /// whose author is known and untrusted are removed; everything else is
    /// passed through untouched for [`crate::worktree_ops::gh`]'s parser.
    #[must_use]
    pub fn drop_untrusted_timeline_prs(&self, stdout: &str) -> String {
        stdout
            .lines()
            .filter(|line| {
                serde_json::from_str::<Value>(line.trim())
                    .map_or(true, |v| !self.known_untrusted(&v))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Every trusted comment on issue/PR `number` of `repo` (`owner/name`), oldest
/// first, from the REST listing (whose `user.login` names an App as `x[bot]`).
/// Bounded through [`crate::script_helpers::run_gh`]. `None` on any failure.
#[must_use]
pub fn fetch_trusted_comments(
    repo: &str,
    number: &str,
    repo_root: &Path,
    use_cache: bool,
) -> Option<Vec<Value>> {
    let stdout = fetch_comment_listing(repo, number, repo_root, use_cache)?;
    TrustPolicy::for_root(repo_root).trusted_listing(&stdout)
}

/// The raw REST comment listing (`gh api …/comments --paginate` stdout) for
/// issue/PR `number` of `repo`. `None` when `gh` failed.
#[must_use]
pub fn fetch_comment_listing(
    repo: &str,
    number: &str,
    repo_root: &Path,
    use_cache: bool,
) -> Option<Vec<u8>> {
    rest_listing(&format!("repos/{repo}/issues/{number}/comments"), repo_root, use_cache)
}

/// The raw REST issue events listing (`labeled`/`unlabeled` with their
/// `actor`, oldest first) for issue `number` of `repo`, uncached: the
/// promotion gate's star provenance (#10827). `None` when `gh` failed.
#[must_use]
pub fn fetch_issue_events(repo: &str, number: &str, repo_root: &Path) -> Option<Vec<u8>> {
    rest_listing(&format!("repos/{repo}/issues/{number}/events"), repo_root, false)
}

/// One paginated REST listing (`gh api <path> --paginate` stdout).
fn rest_listing(path: &str, repo_root: &Path, use_cache: bool) -> Option<Vec<u8>> {
    let out = crate::script_helpers::run_gh(&["api", path, "--paginate"], repo_root, use_cache);
    Some(out.ok_output()?.stdout.clone())
}

/// The REST issue object for `number` of `repo` (a PR is an issue here), the
/// one shape that carries the body's author (`user`, `author_association`).
#[must_use]
pub fn fetch_issue_object(
    repo: &str,
    number: &str,
    repo_root: &Path,
    use_cache: bool,
) -> Option<Value> {
    let path = format!("repos/{repo}/issues/{number}");
    let out = crate::script_helpers::run_gh(&["api", &path], repo_root, use_cache);
    serde_json::from_slice::<Value>(&out.ok_output()?.stdout)
        .ok()
        .filter(Value::is_object)
}

/// H15: the `body` of a `{body, <AUTHOR_JQ>}` object (an issue or PR), but
/// only when its author is trusted by the workspace at `root`. `None` for an
/// untrusted author or an unparseable object; callers treat that exactly like
/// an absent declaration.
#[must_use]
pub fn trusted_body(root: &Path, stdout: &[u8]) -> Option<String> {
    let v = serde_json::from_slice::<Value>(stdout).ok()?;
    if !TrustPolicy::for_root(root).trusts_json(&v) {
        log::info!("comment_trust: body author is not trusted; its markers are ignored (#9548)");
        return None;
    }
    v.get("body").and_then(Value::as_str).map(str::to_string)
}

/// The bodies of `records`, in order.
#[must_use]
pub fn bodies(records: &[Value]) -> Vec<String> {
    records
        .iter()
        .filter_map(|v| v.get("body").and_then(Value::as_str).map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests;
