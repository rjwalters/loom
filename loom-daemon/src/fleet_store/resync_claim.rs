//! The per-repo workspace-resync claim (#10718, tracker #10698 Phase 4).
//!
//! One ref per repo, [`CLAIM_REF`], says "a host is resyncing this repo's
//! installed Loom right now". The first host to create it wins; every other
//! host passes until its next fleet-sync tick. The claim exists to avoid
//! duplicate work, not for correctness: the resync's push is never forced, so
//! of two writers that both believe they hold the claim at most one lands,
//! and the other re-reads and finds nothing left to do.
//!
//! # The claim object
//!
//! A commit made with `POST git/commits` over the default branch head's tree,
//! whose message is [`claim_message`]: holder host, the version being
//! installed and the time. The time is the holder's clock; [`stale_after`] is
//! generous enough to absorb skew.
//!
//! # Operations
//!
//! * **Acquire.** Read the ref first. If it exists and is fresh, report the
//!   holder: two reads, and nothing is created. If it does not exist, create
//!   the claim commit and `POST git/refs`: `201` wins, `422 Reference already
//!   exists` means another host got there in between.
//! * **Takeover**, only of a claim older than [`stale_after`] (or one whose
//!   message does not parse). See below.
//! * **Fence**, before the push: the ref must still be our commit, and less
//!   than half of [`stale_after`] may have passed since we acquired it.
//! * **Release.** Re-read, and `DELETE` only while the ref is still ours.
//!   Then list the takeover tickets (one read, normally empty) and delete any
//!   that were abandoned.
//!
//! # Why takeover is not a fast-forward `PATCH`
//!
//! The curated design used `PATCH git/refs {force: false}` from the stale sha
//! as the compare-and-swap. Checked against GitHub on 2026-10-08 with a
//! throwaway ref in this namespace: creating, updating and deleting
//! `refs/loom/*` all work, but GitHub does **not** enforce fast-forward
//! outside `refs/heads/`. A `force: false` update to a sibling commit, and
//! one to an ancestor, both returned `200`. So that `PATCH` cannot arbitrate
//! between two hosts.
//!
//! What GitHub does arbitrate is creation: a second `POST git/refs` for the
//! same name is `422`. Takeover therefore goes through a ticket whose NAME
//! carries the stale sha, [`TAKEOVER_PREFIX`]`<stale sha>`:
//!
//! 1. create the ticket (`201` for exactly one host; the others pass), after
//!    a read that lets a host that is already too late create nothing;
//! 2. re-read the claim: it must still be the stale sha the ticket names;
//! 3. `PATCH` the claim to a commit whose parent is the stale sha;
//! 4. re-read the claim: it must now be that commit;
//! 5. delete the ticket.
//!
//! A host that dies between 1 and 5 leaves its ticket behind. A ticket older
//! than [`stale_after`] is deleted by the next host that meets it, which then
//! passes, so the tick after that races for it again. One that nobody will
//! meet again (the taker died after step 3, so the stale sha it is named
//! after is gone for good) is collected by the next release, which lists the
//! tickets. The window left open is one request wide (between steps 2 and
//! 3); what closes it is the fence and the non-forced push.
//!
//! # Transport
//!
//! Nothing new: the fleet store's [`Transport`] / [`WriteTransport`] seams.
//! Production reads under the writer credential ([`ClaimGh`]), because a
//! reader App that is not installed on the repo answers `404`, and here that
//! would read as "the claim is gone".

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};

use super::fetch::{Reply, Transport};
use super::gh::GhTransport;
use super::propose::WriteTransport;

/// The claim ref. One per repo; outside `refs/heads` and `refs/tags`, so it
/// triggers no CI, no release and no branch rule.
pub const CLAIM_REF: &str = "refs/loom/resync-claim";

/// Prefix of a takeover ticket; the stale claim's sha follows.
pub const TAKEOVER_PREFIX: &str = "refs/loom/resync-takeover/";

/// First word of a claim commit's message.
const MESSAGE_TAG: &str = "loom-resync-claim";

/// The least a claim lives before it may be taken over.
const MIN_STALE: Duration = Duration::from_secs(15 * 60);

/// How old a claim must be before another host may take it over:
/// `max(10 x syncIntervalSecs, 15 min)`.
#[must_use]
pub fn stale_after(sync_interval: Duration) -> Duration {
    (sync_interval * 10).max(MIN_STALE)
}

/// What a claim commit's message records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRecord {
    /// The holder's host id.
    pub host: String,
    /// The Loom version the holder is installing.
    pub version: String,
    /// When the holder took the claim, by its own clock.
    pub at: DateTime<Utc>,
}

/// The message of a claim commit.
#[must_use]
pub fn claim_message(host: &str, version: &str, at: DateTime<Utc>) -> String {
    // The fields are space-separated, so a host id may not carry whitespace.
    let host: String = host
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .collect();
    format!(
        "{MESSAGE_TAG} host={host} version={version} at={}",
        at.to_rfc3339_opts(SecondsFormat::Secs, true)
    )
}

/// Parse a claim commit's message. `None` for anything else, which callers
/// treat as a stale claim: nobody can be shown to hold it.
#[must_use]
pub fn parse_claim(message: &str) -> Option<ClaimRecord> {
    let mut words = message.lines().next()?.split_whitespace();
    if words.next()? != MESSAGE_TAG {
        return None;
    }
    let (mut host, mut version, mut at) = (None, None, None);
    for word in words {
        match word.split_once('=')? {
            ("host", v) => host = Some(v.to_string()),
            ("version", v) => version = Some(v.to_string()),
            ("at", v) => at = Some(DateTime::parse_from_rfc3339(v).ok()?.with_timezone(&Utc)),
            _ => {}
        }
    }
    Some(ClaimRecord {
        host: host.filter(|h| !h.is_empty())?,
        version: version?,
        at: at?,
    })
}

/// A claim this host holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The claim commit the ref points at.
    pub sha: String,
    /// When it was acquired (the fence measures from here).
    pub acquired_at: DateTime<Utc>,
    /// Whether it was taken over from a stale holder.
    pub took_over: bool,
}

/// The outcome of [`Claimant::acquire`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquire {
    /// This host holds the claim.
    Won(Held),
    /// A fresh claim is held. Not an error: pass until the next tick.
    HeldBy {
        /// The holder (may be this host's own earlier process).
        host: String,
        /// When it took the claim.
        since: DateTime<Utc>,
    },
    /// Another host got there first this tick (it won the takeover, or the
    /// claim changed while we looked). Not an error: pass until the next tick.
    Lost(String),
}

/// The forge could not be reached at all: the request got no HTTP answer.
/// Kept apart from an answer the claim cannot use (a `403`, say), so the
/// caller can report an outage once for the host instead of once per repo.
#[derive(Debug)]
pub struct ForgeUnreachable(pub String);

impl std::fmt::Display for ForgeUnreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ForgeUnreachable {}

/// Why [`Claimant::fence`] says not to push. Neither is a forge failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FenceFailure {
    /// The ref no longer points at our commit.
    NotOurs,
    /// Half the stale window has passed; another host may be about to take
    /// the claim over.
    HeldTooLong,
}

impl std::fmt::Display for FenceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOurs => write!(f, "the claim is no longer ours"),
            Self::HeldTooLong => write!(f, "the claim has been held for half its stale window"),
        }
    }
}

/// Both halves of the forge seam as one object, which is how the workspace
/// pass hands a repo's forge to a [`Claimant`]. Anything that is both a
/// [`Transport`] and a [`WriteTransport`] is one.
pub trait ClaimForge {
    /// The read half.
    fn as_read(&self) -> &dyn Transport;
    /// The write half.
    fn as_write(&self) -> &dyn WriteTransport;
}

impl<T: Transport + WriteTransport> ClaimForge for T {
    fn as_read(&self) -> &dyn Transport {
        self
    }
    fn as_write(&self) -> &dyn WriteTransport {
        self
    }
}

/// The production forge for the claim: [`GhTransport`], with every read made
/// under the writer credential (see the module docs).
pub struct ClaimGh(pub GhTransport);

impl Transport for ClaimGh {
    fn get(&self, api_path: &str, _: Option<&str>, _: Option<&str>) -> Result<Reply> {
        self.0.get_as_writer(api_path)
    }
}

impl WriteTransport for ClaimGh {
    fn write(&self, method: &str, api_path: &str, body: &Value) -> Result<Reply> {
        self.0.write_raw(method, api_path, body)
    }
}

/// One host's handle on one repo's claim.
pub struct Claimant<'a> {
    /// Reads.
    pub read: &'a dyn Transport,
    /// Writes.
    pub write: &'a dyn WriteTransport,
    /// `OWNER/REPO`.
    pub repo: &'a str,
    /// This host's id.
    pub host: &'a str,
    /// The version this host is installing (its running version).
    pub version: &'a str,
    /// See [`stale_after`].
    pub stale_after: Duration,
}

enum Created {
    Yes,
    Exists,
}

impl Claimant<'_> {
    /// Try to take the claim. `tree` is the default branch head's tree, which
    /// the forge is known to have.
    ///
    /// The ref is read before anything is created, so meeting a held claim
    /// costs two reads and leaves no commit behind.
    ///
    /// # Errors
    /// A forge request failed ([`ForgeUnreachable`]) or answered something
    /// unexpected. The caller never assumes it holds the claim.
    pub fn acquire(&self, tree: &str, now: DateTime<Utc>) -> Result<Acquire> {
        if let Some(current) = self.read_ref(CLAIM_REF)? {
            return self.contend(&current, tree, now);
        }
        let sha = self.commit(tree, &[], now)?;
        let won = || {
            Acquire::Won(Held {
                sha: sha.clone(),
                acquired_at: now,
                took_over: false,
            })
        };
        match self.create_ref(CLAIM_REF, &sha) {
            Ok(Created::Yes) => Ok(won()),
            // Another host created it between our read and our create.
            Ok(Created::Exists) => match self.read_ref(CLAIM_REF)? {
                Some(current) => self.contend(&current, tree, now),
                None => Ok(Acquire::Lost("the claim was released while it was being read".into())),
            },
            // The reply was lost (a timeout). The ref decides: if it points at
            // this pass's commit, the create landed and the claim is ours.
            Err(e) => match self.read_ref(CLAIM_REF) {
                Ok(Some(current)) if current == sha => Ok(won()),
                _ => Err(e),
            },
        }
    }

    /// The claim exists at `current`: report a fresh holder, take a stale one
    /// over.
    fn contend(&self, current: &str, tree: &str, now: DateTime<Utc>) -> Result<Acquire> {
        match self.read_record(current)? {
            Some(record) if !self.is_stale(&record, now) => Ok(Acquire::HeldBy {
                host: record.host,
                since: record.at,
            }),
            _ => self.take_over(current, tree, now),
        }
    }

    /// May the push go ahead? `Ok(Err(_))` means no, without a forge failure.
    ///
    /// # Errors
    /// The ref could not be read.
    pub fn fence(
        &self,
        held: &Held,
        now: DateTime<Utc>,
    ) -> Result<std::result::Result<(), FenceFailure>> {
        let age = (now - held.acquired_at).to_std().unwrap_or(Duration::ZERO);
        if age >= self.stale_after / 2 {
            return Ok(Err(FenceFailure::HeldTooLong));
        }
        Ok(match self.read_ref(CLAIM_REF)? {
            Some(current) if current == held.sha => Ok(()),
            _ => Err(FenceFailure::NotOurs),
        })
    }

    /// Release the claim, only if it is still ours. `Ok(false)` when it was
    /// not (someone took it over; it is theirs to release).
    ///
    /// # Errors
    /// The ref could not be read or deleted. The claim then expires on its
    /// own after [`stale_after`].
    pub fn release(&self, held: &Held, now: DateTime<Utc>) -> Result<bool> {
        if self.read_ref(CLAIM_REF)?.as_deref() != Some(held.sha.as_str()) {
            return Ok(false);
        }
        self.delete_ref(CLAIM_REF)?;
        self.collect_tickets(now);
        Ok(true)
    }

    /// Delete every takeover ticket that was abandoned: one whose commit is
    /// older than [`stale_after`] or is not a claim commit. One listing read,
    /// which is normally empty, so normally no `DELETE` is sent. Best effort:
    /// a ticket that survives is met again at the next release.
    fn collect_tickets(&self, now: DateTime<Utc>) {
        let Ok(tickets) = self.list_tickets() else {
            return;
        };
        for (name, sha) in tickets {
            let abandoned = self
                .read_record(&sha)
                .is_ok_and(|record| record.is_none_or(|r| self.is_stale(&r, now)));
            if abandoned {
                let _ = self.delete_ref(&name);
            }
        }
    }

    /// Every takeover ticket in the repo, as `(ref name, sha)`.
    fn list_tickets(&self) -> Result<Vec<(String, String)>> {
        let prefix = TAKEOVER_PREFIX
            .strip_prefix("refs/")
            .unwrap_or(TAKEOVER_PREFIX);
        let reply = self.get(&format!("repos/{}/git/matching-refs/{prefix}", self.repo))?;
        if reply.status != 200 {
            bail!(describe(&reply, "listing takeover tickets", self.repo));
        }
        let value: Value = serde_json::from_str(&reply.body)
            .map_err(|e| anyhow!("malformed ticket listing on {}: {e}", self.repo))?;
        Ok(value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let name = entry.get("ref")?.as_str()?;
                let sha = entry.get("object")?.get("sha")?.as_str()?;
                name.starts_with(TAKEOVER_PREFIX)
                    .then(|| (name.to_string(), sha.to_string()))
            })
            .collect())
    }

    fn get(&self, path: &str) -> Result<Reply> {
        self.read
            .get(path, None, None)
            .map_err(|e| ForgeUnreachable(format!("{e:#}")).into())
    }

    fn send(&self, method: &str, path: &str, body: &Value) -> Result<Reply> {
        self.write
            .write(method, path, body)
            .map_err(|e| ForgeUnreachable(format!("{e:#}")).into())
    }

    fn is_stale(&self, record: &ClaimRecord, now: DateTime<Utc>) -> bool {
        let window = chrono::Duration::from_std(self.stale_after).unwrap_or(chrono::Duration::MAX);
        // A claim dated further ahead than the window is not skew; it would
        // otherwise never expire.
        now - record.at > window || record.at - now > window
    }

    fn take_over(&self, stale: &str, tree: &str, now: DateTime<Utc>) -> Result<Acquire> {
        let ticket = format!("{TAKEOVER_PREFIX}{stale}");
        // Read first, as `acquire` does: a ticket that is already taken costs
        // reads only, and no commit is created for it.
        if let Some(theirs) = self.read_ref(&ticket)? {
            return self.yield_to(&ticket, &theirs, now);
        }
        let sha = self.commit(tree, &[stale], now)?;
        if matches!(self.create_ref(&ticket, &sha)?, Created::Exists) {
            return match self.read_ref(&ticket)? {
                Some(theirs) => self.yield_to(&ticket, &theirs, now),
                None => Ok(Acquire::Lost("another host took over the stale claim".into())),
            };
        }
        let outcome = self.swap_claim(stale, &sha, now);
        let _ = self.delete_ref(&ticket);
        outcome
    }

    /// Another host holds the takeover ticket. If the ticket is itself stale
    /// that host died part-way: clear it and pass, so the next tick can race
    /// for it again.
    fn yield_to(&self, ticket: &str, theirs: &str, now: DateTime<Utc>) -> Result<Acquire> {
        let abandoned = self
            .read_record(theirs)?
            .is_none_or(|r| self.is_stale(&r, now));
        if abandoned {
            let _ = self.delete_ref(ticket);
        }
        Ok(Acquire::Lost("another host is taking over the stale claim".into()))
    }

    /// Steps 2-4 of the takeover, with the ticket for `stale` held.
    fn swap_claim(&self, stale: &str, sha: &str, now: DateTime<Utc>) -> Result<Acquire> {
        let lost = |why: &str| Ok(Acquire::Lost(why.to_string()));
        if self.read_ref(CLAIM_REF)?.as_deref() != Some(stale) {
            return lost("the stale claim changed before it could be taken over");
        }
        let won = || {
            Acquire::Won(Held {
                sha: sha.to_string(),
                acquired_at: now,
                took_over: true,
            })
        };
        let patched = self.send(
            "PATCH",
            &format!("repos/{}/git/{CLAIM_REF}", self.repo),
            &json!({"sha": sha, "force": false}),
        );
        let reply = match patched {
            Ok(reply) => reply,
            // The reply was lost. The ref decides, as it does for a create:
            // if it points at our commit the update landed and the claim is
            // ours, and saying otherwise would strand it for the stale window.
            Err(e) => {
                return match self.read_ref(CLAIM_REF) {
                    Ok(Some(current)) if current == sha => Ok(won()),
                    _ => Err(e),
                };
            }
        };
        match reply.status {
            200 => {}
            404 | 422 => return lost("the stale claim was released during the takeover"),
            _ => bail!(describe(&reply, "taking over the resync claim", self.repo)),
        }
        if self.read_ref(CLAIM_REF)?.as_deref() != Some(sha) {
            return lost("another host replaced the claim during the takeover");
        }
        Ok(won())
    }

    fn commit(&self, tree: &str, parents: &[&str], now: DateTime<Utc>) -> Result<String> {
        let reply = self.send(
            "POST",
            &format!("repos/{}/git/commits", self.repo),
            &json!({
                "message": claim_message(self.host, self.version, now),
                "tree": tree,
                "parents": parents,
            }),
        )?;
        if reply.status != 201 {
            bail!(describe(&reply, "creating the resync claim commit", self.repo));
        }
        sha_field(&reply.body, &["sha"])
    }

    fn create_ref(&self, name: &str, sha: &str) -> Result<Created> {
        let reply = self.send(
            "POST",
            &format!("repos/{}/git/refs", self.repo),
            &json!({"ref": name, "sha": sha}),
        )?;
        match reply.status {
            201 => Ok(Created::Yes),
            422 if message(&reply.body).contains("already exists") => Ok(Created::Exists),
            _ => bail!(describe(&reply, &format!("creating {name}"), self.repo)),
        }
    }

    /// The sha `name` points at; `None` when the ref does not exist.
    fn read_ref(&self, name: &str) -> Result<Option<String>> {
        // `GET git/ref/<name without "refs/">` (singular) matches exactly.
        let short = name.strip_prefix("refs/").unwrap_or(name);
        let reply = self.get(&format!("repos/{}/git/ref/{short}", self.repo))?;
        match reply.status {
            200 => sha_field(&reply.body, &["object", "sha"]).map(Some),
            404 => Ok(None),
            _ => bail!(describe(&reply, &format!("reading {name}"), self.repo)),
        }
    }

    fn read_record(&self, sha: &str) -> Result<Option<ClaimRecord>> {
        let reply = self.get(&format!("repos/{}/git/commits/{sha}", self.repo))?;
        if reply.status != 200 {
            bail!(describe(&reply, &format!("reading claim commit {sha}"), self.repo));
        }
        let value: Value = serde_json::from_str(&reply.body)
            .map_err(|e| anyhow!("malformed commit reply for {sha} on {}: {e}", self.repo))?;
        Ok(value
            .get("message")
            .and_then(Value::as_str)
            .and_then(parse_claim))
    }

    /// Delete `name`. An already-missing ref (`404`, or GitHub's `422
    /// Reference does not exist`) is success.
    fn delete_ref(&self, name: &str) -> Result<()> {
        let reply =
            self.send("DELETE", &format!("repos/{}/git/{name}", self.repo), &Value::Null)?;
        match reply.status {
            200 | 204 | 404 | 422 => Ok(()),
            _ => bail!(describe(&reply, &format!("deleting {name}"), self.repo)),
        }
    }
}

fn message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

fn describe(reply: &Reply, action: &str, repo: &str) -> String {
    let detail = message(&reply.body);
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    let hint = if matches!(reply.status, 403 | 404) {
        " (the writer credential needs `contents: write` on this repo)"
    } else {
        ""
    };
    format!("{action} on {repo} failed (HTTP {}){detail}{hint}", reply.status)
}

fn sha_field(body: &str, path: &[&str]) -> Result<String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| anyhow!("malformed forge reply: {e}"))?;
    path.iter()
        .try_fold(&value, |v, key| v.get(key))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("forge reply has no {}", path.join(".")))
}

#[cfg(test)]
#[path = "tests/resync_claim_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "tests/resync_claim.rs"]
mod tests;
