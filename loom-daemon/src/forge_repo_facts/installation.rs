//! Per-credential repo-facts snapshot: one conditional
//! `GET installation/repositories` per credential, revalidated hourly (W8).
//!
//! # Why
//!
//! Three readers asked the forge a per-repo question whose answer an App
//! installation token can list for every repo at once:
//!
//! - telemetry visibility (`visibility.repo`, `GET repos/<nwo>` → `.private`),
//! - the D32 story key (`telemetry.repo_identity`, `.id` + `.full_name`),
//! - the write-scope probe's installation leg (`write_scope.probe`).
//!
//! Each credential now keeps ONE snapshot of its installation's repositories
//! (`id`, `full_name`, `private`), fetched page by page
//! (`per_page=100`), every page revalidated with its own `If-None-Match`, so
//! an unchanged installation answers `304`s that are free against the rate
//! limit. Call row: [`SNAPSHOT_CALLER`].
//!
//! # Where it lives
//!
//! Memory (this module's slot in [`super::state::State`]) over the shared ETag
//! store directory ([`crate::forge_etag_store::daemon_store_dir`]) as
//! `instsnap-<sha16(key)>.json`, written atomically and owner-only
//! ([`crate::forge_etag_store::write_private_json`]), so every daemon and CLI
//! process of this user on this host shares one snapshot per credential. The
//! key is the forge host plus the credential scope the ETag store already
//! uses ([`crate::forge_etag_store::credential_scope_with`]: the
//! `GH_CONFIG_DIR`, plus a fingerprint of an env token — never the token).
//!
//! # Fail-private
//!
//! A snapshot answers only while it was verified within [`ttl_secs`] (never
//! more than [`SNAPSHOT_TTL_MAX_SECS`]). Past that it is revalidated; when
//! that fails it answers [`Answer::Unavailable`] — a stale snapshot is never
//! served. A snapshot stamped later than now (a clock step, a damaged file)
//! is no snapshot at all. Visibility maps `Unavailable` and a repo absent
//! from a fresh snapshot ([`Answer::Listed`]`(None)`) to PRIVATE, so this
//! layer can never serve a stale "public".
//!
//! The price of that: while the listing cannot be had at TTL expiry (an
//! outage, a rate limit), every repo this credential answers for — PUBLIC
//! ones included — is stamped private until a revalidation succeeds.
//!
//! # User credentials
//!
//! A user token (PAT, OAuth, `gh auth login`) is refused this endpoint. That
//! refusal is remembered for the same TTL as [`Answer::PerRepo`]: the caller
//! keeps its own per-repo read, exactly as before W8. Only the forge saying
//! so ([`failure::is_installation_token_refusal`]) turns a verified
//! installation snapshot into a user credential: any other `403`/`404` on a
//! credential that listed before is a failed revalidation, never a downgrade.
//!
//! # Rate limits
//!
//! A refused listing is reported for the credential that made it
//! ([`failure::report`]): a READER is withdrawn for its own `(app, owner)`
//! bucket and the lookup falls through to the writer's snapshot (or to
//! private) — it never reaches the host-wide breaker, which would stop every
//! forge call on the host for one read-only App's dry pool. The WRITER's
//! refusal does reach the breaker, with its `GH_CONFIG_DIR` and the response
//! head so the breaker reads the bucket that was refused.
//!
//! # Kill switch
//!
//! Off with repo facts (`LOOM_REPO_FACTS=0`) or on its own
//! (`LOOM_INSTALLATION_SNAPSHOT=0`): every consumer gets [`Answer::Disabled`]
//! and issues its pre-W8 calls.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::forge_etag_store as store;
use crate::forge_identity::IdentityRole;
use crate::forge_listing::{parse_http_response, HttpResponse};

use super::state;

/// The call-ledger row of every snapshot read (`forge calls --by caller`).
pub(crate) const SNAPSHOT_CALLER: &str = "repo_facts.installation_snapshot";
/// Default for `LOOM_INSTALLATION_SNAPSHOT_TTL_SECS`.
pub(crate) const SNAPSHOT_TTL_DEFAULT_SECS: i64 = 3600;
/// The longest a verified snapshot may answer, whatever the override says:
/// the TTL is the bound on an unseen public → private flip, so a typo must
/// not stretch it to a day.
pub(crate) const SNAPSHOT_TTL_MAX_SECS: i64 = 3600;
/// No new read of a snapshot whose last revalidation failed, for this long.
pub(crate) const SNAPSHOT_FAILURE_BACKOFF_SECS: i64 = super::SUSPECT_BACKOFF_SECS;
const PER_PAGE: u64 = 100;
/// Upper bound on pages read per revalidation (10 000 repositories). Repos
/// beyond it are absent from the snapshot, which only ever means PRIVATE.
const MAX_PAGES: u64 = 100;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// One repository the credential's installation can reach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RepoEntry {
    pub(crate) id: u64,
    /// `Owner/Name` as GitHub spells it now.
    pub(crate) full_name: String,
    pub(crate) private: bool,
}

/// One page of the listing and the validator it was served with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Page {
    pub(crate) etag: Option<String>,
    pub(crate) repos: Vec<RepoEntry>,
}

/// What kind of credential answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    /// An App installation: the pages are its repositories.
    Installation,
    /// The endpoint refused it (a user token): no snapshot applies.
    NotInstallation,
}

/// The persisted snapshot of one credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub(crate) kind: Kind,
    #[serde(default)]
    pub(crate) total_count: u64,
    #[serde(default)]
    pub(crate) pages: Vec<Page>,
    /// When the forge last confirmed it (unix seconds); `0` = never.
    pub(crate) verified_at: i64,
    /// After a FAILED revalidation: no new read before this time.
    #[serde(default)]
    pub(crate) failed_until: Option<i64>,
}

impl Snapshot {
    /// Verified within `ttl` — and not in the future: a stamp later than
    /// `now` says nothing about how old the data is.
    fn fresh(&self, now: i64, ttl: i64) -> bool {
        self.verified_at > 0 && !self.stamped_ahead_of(now) && now - self.verified_at < ttl
    }

    /// Stamped later than `now` (the clock stepped back, or the file is
    /// damaged): [`load`] treats it as absent.
    fn stamped_ahead_of(&self, now: i64) -> bool {
        self.verified_at > now
    }

    /// Backing off — but never longer than one backoff window from `now`, so
    /// a damaged `failed_until` cannot silence a credential for good.
    fn in_backoff(&self, now: i64) -> bool {
        self.failed_until
            .is_some_and(|u| u > now && u - now <= SNAPSHOT_FAILURE_BACKOFF_SECS)
    }

    /// A listing the forge once confirmed for this credential.
    fn verified_installation(&self) -> bool {
        self.kind == Kind::Installation && self.verified_at > 0
    }

    fn find(&self, owner_repo: &str) -> Option<RepoEntry> {
        let want = owner_repo.trim();
        self.pages
            .iter()
            .flat_map(|p| p.repos.iter())
            .find(|r| r.full_name.eq_ignore_ascii_case(want))
            .cloned()
    }

    fn answer(&self, owner_repo: &str) -> Answer {
        match self.kind {
            Kind::Installation => Answer::Listed(self.find(owner_repo)),
            Kind::NotInstallation => Answer::PerRepo,
        }
    }
}

/// What a snapshot says about one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    /// A fresh installation snapshot: the repo's entry, or `None` when the
    /// installation does not list it.
    Listed(Option<RepoEntry>),
    /// No snapshot applies to this credential (a user token): use the
    /// per-repo read.
    PerRepo,
    /// The snapshot could not be established fresh right now (failed,
    /// stale, backing off, breaker open). Never a basis for "public".
    Unavailable,
    /// The snapshot is switched off: every consumer keeps its pre-W8 calls.
    Disabled,
}

/// A credential a snapshot belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credential {
    /// `GH_CONFIG_DIR` to read under; `None` = this process's own.
    pub(crate) config_dir: Option<PathBuf>,
    /// `Reader` strips every env token, so the reader's dir is the only
    /// credential `gh` sees; `Writer` keeps the process environment (the
    /// credential the writes themselves carry).
    pub(crate) role: IdentityRole,
    /// The reader's rate-limit bucket label, for the ledger.
    pub(crate) bucket: Option<String>,
    /// The reader App's id: who to withdraw when its listing is refused.
    pub(crate) app_id: Option<String>,
}

impl Credential {
    pub(crate) fn writer(config_dir: Option<PathBuf>) -> Self {
        Self {
            config_dir,
            role: IdentityRole::Writer,
            bucket: None,
            app_id: None,
        }
    }

    /// Reader App `app_id`'s credential (`dir`) for `owner_repo`'s owner.
    pub(crate) fn reader(dir: PathBuf, app_id: &str, owner_repo: &str) -> Self {
        Self {
            config_dir: Some(dir),
            role: IdentityRole::Reader,
            bucket: Some(crate::forge_identity::reader_bucket(app_id, owner_repo)),
            app_id: Some(app_id.to_string()),
        }
    }

    fn strips_token_env(&self) -> bool {
        self.role == IdentityRole::Reader
    }

    /// `host|credential scope`. A reader never sees an env token, so none
    /// is folded into its key.
    fn key(&self) -> String {
        let host = state::env_var("GH_HOST")
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| "github.com".to_string())
            .to_ascii_lowercase();
        let token = if self.strips_token_env() {
            None
        } else {
            ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN"]
                .iter()
                .find_map(|v| state::env_var(v).filter(|t| !t.is_empty()))
        };
        let scope = store::credential_scope_with(self.config_dir.as_deref(), token.as_deref());
        format!("{host}|{scope}")
    }
}

/// Whether snapshots serve their consumers.
pub(crate) fn enabled() -> bool {
    let killed = state::env_var("LOOM_INSTALLATION_SNAPSHOT").is_some_and(|v| v.trim() == "0");
    !killed && super::enabled()
}

/// How long a verified snapshot answers before one conditional revalidation
/// (`LOOM_INSTALLATION_SNAPSHOT_TTL_SECS`, default one hour, never more than
/// [`SNAPSHOT_TTL_MAX_SECS`]). It is also the bound on how long a
/// public/private flip goes unseen by THIS layer; a consumer that caches the
/// answer for its own TTL (the write-scope probe) stacks on top of it.
pub(crate) fn ttl_secs() -> i64 {
    state::env_var("LOOM_INSTALLATION_SNAPSHOT_TTL_SECS")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .map_or(SNAPSHOT_TTL_DEFAULT_SECS, |v| v.min(SNAPSHOT_TTL_MAX_SECS))
}

fn snapshot_path(key: &str) -> Option<PathBuf> {
    let dir = store::daemon_store_dir()?;
    Some(dir.join(format!("instsnap-{}.json", crate::short_hash::short_sha16(key))))
}

/// The newest snapshot for `key`: memory, unless the disk holds one verified
/// later (another process on this host revalidated it). One stamped later
/// than `now` is dropped from both — it reads as no snapshot, so the caller
/// refetches unconditionally rather than trusting it or its validators.
fn load(key: &str, now: i64) -> Option<Snapshot> {
    let sane = |s: &Snapshot| !s.stamped_ahead_of(now);
    let mem = state::with(|s| {
        if s.snapshots.get(key).is_some_and(|m| !sane(m)) {
            s.snapshots.remove(key);
        }
        s.snapshots.get(key).cloned()
    });
    let disk: Option<Snapshot> = snapshot_path(key)
        .and_then(|p| store::read_private_json(&p))
        .filter(sane);
    let newest = match (mem, disk) {
        (Some(m), Some(d)) if d.verified_at > m.verified_at => Some(d),
        (Some(m), _) => Some(m),
        (None, d) => d,
    };
    if let Some(s) = &newest {
        state::with(|st| st.snapshots.insert(key.to_string(), s.clone()));
    }
    newest
}

fn save(key: &str, snap: &Snapshot) {
    state::with(|s| s.snapshots.insert(key.to_string(), snap.clone()));
    if let Some(path) = snapshot_path(key) {
        store::write_private_json(&path, snap);
    }
}

/// What `cred`'s snapshot says about `owner_repo`, revalidating it (through
/// `gh`) only when it is older than [`ttl_secs`] and not backing off.
pub(crate) fn lookup(gh: &Path, cred: &Credential, owner_repo: &str) -> Answer {
    if !enabled() {
        return Answer::Disabled;
    }
    let key = cred.key();
    let now = state::now();
    let ttl = ttl_secs();
    // The hot path: a fresh snapshot in memory answers with no file read.
    let hot = state::with(|s| {
        s.snapshots
            .get(&key)
            .filter(|s| s.fresh(now, ttl))
            .map(|s| s.answer(owner_repo))
    });
    if let Some(answer) = hot {
        return answer;
    }
    let prior = load(&key, now);
    if let Some(s) = prior.as_ref().filter(|s| s.fresh(now, ttl)) {
        return s.answer(owner_repo);
    }
    // A credential the endpoint refused stays a user token: a failed recheck
    // keeps the per-repo path rather than inventing an outage.
    let known_user = prior
        .as_ref()
        .is_some_and(|s| s.kind == Kind::NotInstallation && s.verified_at > 0);
    let request = Request {
        gh,
        cred,
        owner_repo,
        verified_installation: prior.as_ref().is_some_and(Snapshot::verified_installation),
    };
    let unavailable = || {
        if known_user {
            Answer::PerRepo
        } else {
            Answer::Unavailable
        }
    };
    if prior.as_ref().is_some_and(|s| s.in_backoff(now)) {
        return unavailable();
    }
    if crate::rate_limit_breaker::global_skip_pass(SNAPSHOT_CALLER) {
        return unavailable();
    }
    match revalidate(&request, prior.as_ref(), now) {
        Ok(snap) => {
            save(&key, &snap);
            note_recovered(&key, cred);
            snap.answer(owner_repo)
        }
        Err(why) => {
            let mut snap = prior.unwrap_or(Snapshot {
                kind: Kind::Installation,
                total_count: 0,
                pages: Vec::new(),
                verified_at: 0,
                failed_until: None,
            });
            snap.failed_until = Some(now + SNAPSHOT_FAILURE_BACKOFF_SECS);
            save(&key, &snap);
            note_failed(&key, cred, &why);
            unavailable()
        }
    }
}

/// The credentials a read of `owner_repo` would run under, in order: the
/// repo's reader App (when one is usable), then the writer's credential for
/// the owner (else this process's own).
pub(crate) fn read_credentials(owner_repo: &str) -> Vec<Credential> {
    let mut creds = Vec::new();
    if let Some((dir, app_id)) = crate::forge_identity::read_credential(owner_repo, None) {
        creds.push(Credential::reader(dir, &app_id, owner_repo));
    }
    creds.push(Credential::writer(crate::credential_preflight::gh_config_dir_for_owner_slug(
        owner_repo,
    )));
    creds
}

/// [`lookup_repo_with`] for a telemetry read of `owner_repo` under the
/// default `gh` and [`read_credentials`].
pub(crate) fn lookup_repo(owner_repo: &str) -> Answer {
    if !enabled() {
        return Answer::Disabled;
    }
    let gh = PathBuf::from(crate::gh_invocation::gh_bin());
    lookup_repo_with(&gh, &read_credentials(owner_repo), owner_repo)
}

/// The first credential whose snapshot lists `owner_repo`. Otherwise
/// [`Answer::PerRepo`] when some credential is a user token (its per-repo
/// read may still answer), else `Listed(None)` when a fresh snapshot does
/// not list it, else [`Answer::Unavailable`].
pub(crate) fn lookup_repo_with(gh: &Path, creds: &[Credential], owner_repo: &str) -> Answer {
    let (mut per_repo, mut absent) = (false, false);
    for cred in creds {
        match lookup(gh, cred, owner_repo) {
            hit @ Answer::Listed(Some(_)) => return hit,
            Answer::Listed(None) => absent = true,
            Answer::PerRepo => per_repo = true,
            Answer::Disabled => return Answer::Disabled,
            Answer::Unavailable => {}
        }
    }
    if per_repo {
        Answer::PerRepo
    } else if absent {
        Answer::Listed(None)
    } else {
        Answer::Unavailable
    }
}

/// Why a revalidation produced no snapshot.
type Failure = String;

/// One revalidation: the credential, the `gh` to run, the repo the lookup
/// was for (the owner a refused reader is withdrawn from), and whether the
/// forge has confirmed this credential as an installation before.
struct Request<'a> {
    gh: &'a Path,
    cred: &'a Credential,
    owner_repo: &'a str,
    verified_installation: bool,
}

/// Re-read every page of the credential's listing, conditionally on the
/// validators `prior` holds. Any page failing fails the whole revalidation:
/// a listing is never assembled from some pages of this read and none of
/// another.
fn revalidate(req: &Request<'_>, prior: Option<&Snapshot>, now: i64) -> Result<Snapshot, Failure> {
    let prior_page = |i: usize| {
        prior
            .filter(|p| p.verified_installation())
            .and_then(|p| p.pages.get(i))
    };
    let first = fetch_page(req, 1, prior_page(0))?;
    let (page1, total) = match first {
        PageOutcome::NotInstallation => {
            return Ok(Snapshot {
                kind: Kind::NotInstallation,
                total_count: 0,
                pages: Vec::new(),
                verified_at: now,
                failed_until: None,
            })
        }
        PageOutcome::Unchanged(page) => {
            let total = prior.map_or(0, |p| p.total_count);
            (page, total)
        }
        PageOutcome::Fetched(page, total) => (page, total),
    };
    let wanted = total.div_ceil(PER_PAGE).clamp(1, MAX_PAGES);
    let mut pages = vec![page1];
    for n in 2..=wanted {
        let i = usize::try_from(n - 1).unwrap_or(usize::MAX);
        match fetch_page(req, n, prior_page(i))? {
            PageOutcome::Unchanged(page) | PageOutcome::Fetched(page, _) => pages.push(page),
            PageOutcome::NotInstallation => {
                return Err(format!("page {n} refused after page 1 was served"))
            }
        }
    }
    Ok(Snapshot {
        kind: Kind::Installation,
        total_count: total,
        pages,
        verified_at: now,
        failed_until: None,
    })
}

enum PageOutcome {
    /// `304`: the prior page stands.
    Unchanged(Page),
    /// `200`: the page and the listing's `total_count`.
    Fetched(Page, u64),
    /// The endpoint refused the credential as not an installation.
    NotInstallation,
}

fn page_url(n: u64) -> String {
    format!("installation/repositories?per_page={PER_PAGE}&page={n}")
}

fn fetch_page(req: &Request<'_>, n: u64, prior: Option<&Page>) -> Result<PageOutcome, Failure> {
    let etag = prior.and_then(|p| p.etag.as_deref());
    let url = page_url(n);
    let Fetched {
        response,
        stderr,
        head,
    } = fetch(req.gh, req.cred, &url, etag)?;
    let fail = |why: String| {
        failure::report(
            req,
            &failure::Refusal {
                response: response.as_ref(),
                stderr: &stderr,
                head: head.as_deref(),
            },
        );
        Err(why)
    };
    let Some(r) = response.as_ref() else {
        return fail(format!("no HTTP response: {stderr}"));
    };
    match r.status {
        304 => match prior {
            Some(p) if etag.is_some() => Ok(PageOutcome::Unchanged(p.clone())),
            _ => fail("304 with no stored page".to_string()),
        },
        200 => match parse_page(&r.body) {
            Some((repos, total)) => Ok(PageOutcome::Fetched(
                Page {
                    etag: r.etag.clone(),
                    repos,
                },
                total,
            )),
            None => fail("unparseable installation listing".to_string()),
        },
        403 | 404 if !is_rate_limited(r, &stderr) => {
            // The forge saying "not an installation token" is a user
            // credential. Any other 403/404 is only that for a credential
            // never seen listing; one that listed before keeps its kind and
            // fails this revalidation (private), rather than being demoted
            // to per-repo reads by a transient or unrelated refusal.
            if failure::is_installation_token_refusal(&r.body, &stderr)
                || !req.verified_installation
            {
                Ok(PageOutcome::NotInstallation)
            } else {
                fail(format!(
                    "HTTP {} without the installation-token refusal on a verified \
                     installation: {stderr}",
                    r.status
                ))
            }
        }
        status => fail(format!("HTTP {status}: {stderr}")),
    }
}

fn is_rate_limited(r: &HttpResponse, stderr: &str) -> bool {
    r.ratelimit.remaining == Some(0)
        || r.ratelimit.retry_after_secs.is_some()
        || crate::rate_limit_breaker::indicates_rate_limit(stderr)
        || crate::rate_limit_breaker::indicates_rate_limit(&r.body)
}

/// `(repositories, total_count)` from one listing page. An entry without a
/// positive id, a two-segment `full_name` or a boolean `private` is dropped
/// — absent means PRIVATE, so a malformed row can only fail closed.
pub(crate) fn parse_page(body: &str) -> Option<(Vec<RepoEntry>, u64)> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let total = v.get("total_count")?.as_u64()?;
    let repos = v
        .get("repositories")?
        .as_array()?
        .iter()
        .filter_map(|r| {
            let id = r.get("id")?.as_u64().filter(|id| *id > 0)?;
            let full_name = r.get("full_name")?.as_str()?.trim();
            let mut segs = full_name.split('/');
            let ok = matches!((segs.next(), segs.next(), segs.next()),
                (Some(o), Some(n), None) if !o.is_empty() && !n.is_empty());
            ok.then_some(())?;
            Some(RepoEntry {
                id,
                full_name: full_name.to_string(),
                private: r.get("private")?.as_bool()?,
            })
        })
        .collect();
    Some((repos, total))
}

/// One page read: the parsed response, `gh`'s stderr, and the raw response
/// head (status line + headers) for the breaker's reset evidence.
struct Fetched {
    response: Option<HttpResponse>,
    stderr: String,
    head: Option<String>,
}

/// One `gh api --include <url>` under exactly `cred`, counted as
/// [`SNAPSHOT_CALLER`]. The explicit `GH_CONFIG_DIR` (or the writer pin)
/// keeps the facade from routing it elsewhere: a snapshot measures the
/// credential it is filed under.
fn fetch(gh: &Path, cred: &Credential, url: &str, etag: Option<&str>) -> Result<Fetched, Failure> {
    use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
    use crate::proc_exec::Completion;
    let op = crate::forge_call_stats::ops::REPO_LIST_FOR_INSTALLATION;
    let mut inv = GhInvocation::new(
        Operation::new(SNAPSHOT_CALLER),
        AccessIntent::Read,
        GhTarget::None,
        FETCH_TIMEOUT,
    )
    .forge_op(op)
    .program(gh)
    .gh_config_dir(cred.config_dir.as_deref())
    .identity_role(cred.role)
    .args(["api", "--include", url]);
    if cred.strips_token_env() {
        inv = inv.without_token_env();
    } else {
        // The listing depends on WHO asks: it must never be served by another
        // identity than the one this snapshot is filed under.
        inv = inv.writer_identity();
    }
    if let Some(bucket) = &cred.bucket {
        inv = inv.identity_bucket(bucket);
    }
    if let Some(e) = etag {
        inv = inv.arg("-H").arg(format!("If-None-Match: {e}"));
    }
    match inv.execute() {
        Ok(GhCompletion::Captured(Completion::Exited(out))) => {
            let raw = String::from_utf8_lossy(&out.stdout);
            let response = parse_http_response(&raw);
            Ok(Fetched {
                head: response.as_ref().map(|_| failure::response_head(&raw)),
                response,
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
        Ok(_) => Err(format!("gh api {url} timed out")),
        Err(e) => Err(format!("failed to invoke {}: {e}", gh.display())),
    }
}

/// A short, path-free label for a credential in log lines.
fn label(key: &str, cred: &Credential) -> String {
    let role = if cred.strips_token_env() {
        "reader"
    } else {
        "writer"
    };
    format!("{role} credential {}", crate::short_hash::short_sha16(key))
}

/// Warn once per failure streak per credential (not per lookup).
fn note_failed(key: &str, cred: &Credential, why: &str) {
    if state::with(|s| s.snapshot_failing.insert(key.to_string())) {
        log::warn!(
            "forge_repo_facts: installation snapshot for {} failed ({why}); its repos read as \
             private until it recovers",
            label(key, cred)
        );
    }
}

fn note_recovered(key: &str, cred: &Credential) {
    if state::with(|s| s.snapshot_failing.remove(key)) {
        log::info!("forge_repo_facts: installation snapshot for {} recovered", label(key, cred));
    }
}

mod failure;

#[cfg(test)]
#[path = "installation_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "installation_guard_tests.rs"]
mod guard_tests;
