//! `loom-daemon fleet-config propose` — open a PR against the fleet store
//! instead of hand-editing it (#9599).
//!
//! The store is read-only from a host's point of view ([`super`] module
//! docs): every other `fleet-config` sub-verb only ever writes to *this*
//! host (the machine tier, the host-local tier, the workspace registry).
//! `propose` is the one place `fleet-config` writes to the store itself, and
//! it never writes directly — every edit lands as a branch + PR, reviewed
//! and merged by the operator like any other change to the store (merge
//! commits only, per the store's own branch policy; this module has no
//! auto-merge path, and it never touches `repos.yml`'s `fleet`/`firewall`
//! flags).
//!
//! - [`state_edit`] / [`priority_edit`] — format-preserving text edits of
//!   `fleet/state.yml` / `repos.yml` ([`block`]): touch only the lines a
//!   sub-verb's change requires, so a reviewer's diff is exactly that change.
//! - [`adopt`] — turns `render --check` drift into the store-side patch that
//!   would make it the new rendered value.
//! - [`diff`] — the `--dry-run` / PR-body rendering of a [`FileChange`].
//! - [`WriteTransport`] — the write half of the network seam
//!   ([`super::fetch::Transport`] is the read half): POST/PUT under the
//!   store's **writer** credential only — never a reader, which is never
//!   granted write scope. Its production impl is
//!   [`super::gh::GhTransport::write_raw`]; every write call this module
//!   makes goes through it, so a 403/404 here reliably means "the writer
//!   app's installation lacks `contents: write` / `pull_requests: write` on
//!   this store", not "wrong credential path" — see [`ensure_ok`], which
//!   turns that into the operator-facing message.

pub mod adopt;
mod block;
pub mod diff;
mod priority_edit;
mod state_edit;

use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose;
use base64::Engine as _;
use serde_json::{json, Value};

pub use adopt::AdoptedFile;
pub use priority_edit::edit as edit_priority;
pub use state_edit::edit as edit_state;

use super::fetch::Reply;

/// One file a [`Proposal`] changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Store-relative path.
    pub path: String,
    /// Current text; `None` for a file the store does not have yet.
    pub before: Option<String>,
    /// The current blob's SHA, for the update-not-clobber check on the
    /// contents API. `Some` updates the file, `None` creates it.
    pub before_sha: Option<String>,
    /// The new text.
    pub after: String,
}

/// A branch + PR this module is about to (or, in `--dry-run`, would) open.
#[derive(Debug, Clone)]
pub struct Proposal {
    /// New branch name, off the store's current base commit.
    pub branch: String,
    /// PR title.
    pub title: String,
    /// PR body, including the hidden provenance marker.
    pub body: String,
    /// The files it changes. Never empty — a proposal with nothing to change
    /// is not opened at all (the caller reports "nothing to propose" before
    /// building one).
    pub files: Vec<FileChange>,
}

impl Proposal {
    /// The `--dry-run` / pre-submit report: the branch, the title, and every
    /// file's diff.
    #[must_use]
    pub fn report(&self) -> String {
        let mut out = format!("branch: {}\ntitle:  {}\n", self.branch, self.title);
        for f in &self.files {
            out.push_str(&diff::unified(&f.path, f.before.as_deref().unwrap_or(""), &f.after));
        }
        out
    }
}

/// A PR [`submit`] opened.
#[derive(Debug, Clone, Default)]
pub struct SubmittedPr {
    /// The PR number, when the response named one.
    pub number: Option<u64>,
    /// The PR's URL, when the response named one.
    pub url: Option<String>,
}

/// The write half of the network seam: one POST/PUT under the store's writer
/// credential. `Err` means the request could not be made at all; any HTTP
/// status — including a 403 that means a missing scope grant — comes back
/// as `Ok`, exactly like [`super::fetch::Transport::get`] on the read side.
pub trait WriteTransport {
    /// `method` `api_path` with `body` as the JSON request payload.
    fn write(&self, method: &str, api_path: &str, body: &Value) -> Result<Reply>;
}

/// Open `proposal` against `repo`, branching from `base_commit` and PRing
/// onto `base_ref`. Always a branch first, never a direct push — even a
/// single-file proposal gets its own branch.
///
/// Not transactional: a failure partway through leaves the branch (and any
/// file already written to it) behind, with no PR. That is the safe
/// direction — the store's own `base_ref` is never touched either way, and
/// an orphaned `loom/fleet-propose/…` branch is inert and named for what
/// left it there. Re-running gets a fresh, differently-stamped branch
/// ([`branch_name`]) rather than resuming the broken one, so a half-written
/// branch can never turn into a PR nobody meant to open.
pub fn submit(
    wt: &dyn WriteTransport,
    repo: &str,
    base_ref: &str,
    base_commit: &str,
    proposal: &Proposal,
) -> Result<SubmittedPr> {
    let reply = wt.write(
        "POST",
        &format!("repos/{repo}/git/refs"),
        &json!({"ref": format!("refs/heads/{}", proposal.branch), "sha": base_commit}),
    )?;
    ensure_ok(&reply, "creating the proposal branch", repo)?;

    for f in &proposal.files {
        let mut body = json!({
            "message": format!("fleet-config propose: {}", proposal.title),
            "content": general_purpose::STANDARD.encode(f.after.as_bytes()),
            "branch": proposal.branch,
        });
        if let Some(sha) = &f.before_sha {
            body["sha"] = json!(sha);
        }
        let reply = wt.write("PUT", &format!("repos/{repo}/contents/{}", f.path), &body)?;
        ensure_ok(&reply, &format!("writing {}", f.path), repo)?;
    }

    // No `maintainer_can_modify`: the head branch is always in the store
    // repo itself (created a few lines up), never a fork, so the field would
    // be meaningless here — and sending a field the endpoint can reject for
    // a same-repo head buys nothing.
    let reply = wt.write(
        "POST",
        &format!("repos/{repo}/pulls"),
        &json!({
            "title": proposal.title,
            "head": proposal.branch,
            "base": base_ref,
            "body": proposal.body,
        }),
    )?;
    ensure_ok(&reply, "opening the PR", repo)?;
    let v: Value =
        serde_json::from_str(&reply.body).context("malformed pull-request-creation response")?;
    Ok(SubmittedPr {
        number: v.get("number").and_then(Value::as_u64),
        url: v
            .get("html_url")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Turn a non-2xx write reply into a clear error, calling out the specific
/// scope grant a 403/404 usually means here: `propose` is the one
/// `fleet-config` sub-verb that asks for `contents: write` and
/// `pull_requests: write`, an explicit operator grant on the writer app's
/// installation, and silently retrying or crashing on its absence would be
/// worse than saying so plainly.
fn ensure_ok(reply: &Reply, action: &str, repo: &str) -> Result<()> {
    if (200..300).contains(&reply.status) {
        return Ok(());
    }
    let detail = error_detail(&reply.body);
    if matches!(reply.status, 403 | 404) {
        bail!(
            "{action} on {repo} failed (HTTP {}){detail} — the writer app's installation needs \
             `contents: write` and `pull_requests: write` on this store; that is an explicit \
             operator grant this command cannot make for itself",
            reply.status
        );
    }
    bail!("{action} on {repo} failed (HTTP {}){detail}", reply.status);
}

fn error_detail(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
        .map(|m| format!(": {m}"))
        .unwrap_or_default()
}

/// Drop every [`FileChange`] that would write a file back exactly as the
/// store already has it. A sub-verb can legitimately produce one — asking
/// for a priority the record already carries, adopting drift that turned
/// out not to be drift — and an empty PR is pure review noise, so the
/// caller reports "nothing to propose" when this comes back empty rather
/// than opening one.
#[must_use]
pub fn drop_unchanged(files: Vec<FileChange>) -> Vec<FileChange> {
    files
        .into_iter()
        .filter(|f| f.before.as_deref() != Some(f.after.as_str()))
        .collect()
}

/// A branch name for one proposal: `loom/fleet-propose/<kind>-<stamp>`.
#[must_use]
pub fn branch_name(kind: &str, stamp: &str) -> String {
    format!("loom/fleet-propose/{kind}-{stamp}")
}

/// Build the hidden provenance marker for a store PR (#9027 v1, see
/// [`crate::provenance`]). Unlike [`crate::provenance::marker::collect`],
/// this never inspects a local git checkout of the *store* — there isn't
/// one; every store write goes through the forge API — so `base` is simply
/// the commit this proposal branched from, which [`submit`] already
/// requires as an argument, rather than something derived from `git
/// merge-base` against a checkout that does not exist.
#[must_use]
pub fn provenance_marker(daemon_workspace: &Path, base_commit: &str) -> String {
    use crate::provenance::{marker, NONE, UNKNOWN};
    let host = crate::sweep_registry::host_identity();
    marker::Marker {
        build: crate::self_update::BUILD_STAMP.to_string(),
        prompts: marker::prompts_field(daemon_workspace),
        sweep: std::env::var("LOOM_SWEEP_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| NONE.to_string()),
        story: NONE.to_string(),
        trace: UNKNOWN.to_string(),
        host: if host == crate::sweep_registry::UNKNOWN_HOST {
            UNKNOWN.to_string()
        } else {
            host
        },
        base: base_commit.to_string(),
        run: marker::run_field(|k| std::env::var(k).ok()),
        installs: None,
        origin: Some(crate::provenance::origin::WorkOrigin::from_env()),
    }
    .render()
}

#[cfg(test)]
#[path = "tests/mod_tests.rs"]
mod tests;
