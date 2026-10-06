//! Shared on-disk ETag store for conditional `gh api` REST reads (#9252).
//!
//! Extracted from [`crate::forge_listing`] so every conditional-GET surface
//! shares ONE store, ONE key scheme and ONE fetch primitive:
//!
//! - the daemon's own hot polling loops ([`crate::forge_listing::list_issues_cached`]),
//!   which since #9252 back their in-memory map with this store so an ETag
//!   survives a daemon restart (self-update, supervised restart);
//! - the agent-facing `forge … --cached` path
//!   ([`crate::forge_listing::list_issues_cached_persistent`], #5056);
//! - single-entity reads (`GET /issues/{n}`, `/pulls/{n}`) added by #9254.
//!
//! # Key: resolved identity, not raw `cwd`
//!
//! [`cache_key`] resolves `{owner}/{repo}` BEFORE keying (an explicit repo, or
//! `cwd`'s `origin` remote, memoised per root), and folds in the forge host and
//! the `gh` credential identity. The credential component matters because since
//! #5401 there is no longer one credential per process: a cross-owner root's
//! child `gh` runs under that owner's `GH_CONFIG_DIR`
//! ([`crate::credential_preflight::gh_config_dir_for_root`]). A `GH_TOKEN`
//! in the environment is folded in as a truncated SHA-256 fingerprint, never
//! the token itself. So the same repo reached from a workspace root or a
//! worktree shares one entry, and two identities never share one.
//!
//! # Location and durability
//!
//! `${TMPDIR:-/tmp}/loom-forge-listing-cache` (`LOOM_LISTING_CACHE_DIR`
//! overrides), shared by every loom process of this user on this host. On
//! Linux `/tmp` may be tmpfs: entries survive a daemon restart but not a
//! reboot, which costs one `200` per listing after a boot — acceptable. An
//! agent running under a different credential gets a different key, so a
//! `Vary: Authorization` mismatch can only ever cost a call, never serve
//! another identity's answer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};

use crate::forge_listing::{parse_http_response, HttpResponse};
use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
use crate::proc_exec::Completion;

/// `${TMPDIR:-/tmp}` — the per-user, per-host scratch base the ETag store and
/// the forge-call sink ([`crate::forge_call_stats`]) both live under.
pub(crate) fn host_tmp_base() -> PathBuf {
    let base = std::env::var("TMPDIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    PathBuf::from(base)
}

/// On-disk store directory. `LOOM_LISTING_CACHE_DIR` overrides (tests point
/// it at a tempdir); otherwise `${TMPDIR:-/tmp}/loom-forge-listing-cache`,
/// mirroring the existing `gh-cached` `/tmp/gh-cache` convention.
pub(crate) fn disk_cache_dir() -> PathBuf {
    if let Ok(d) = std::env::var("LOOM_LISTING_CACHE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    host_tmp_base().join("loom-forge-listing-cache")
}

/// Deterministic on-disk filename for `cache_key` inside [`disk_cache_dir`].
pub(crate) fn disk_cache_path(cache_key: &str) -> PathBuf {
    entry_path_in(&disk_cache_dir(), cache_key)
}

/// Deterministic filename for `cache_key` inside `dir`, prefixed with
/// `"listing-"` (see [`entry_path_with_prefix`] for a caller-chosen prefix,
/// used by [`crate::forge_cached_view`]'s `"view-"` entries sharing this same
/// directory).
pub(crate) fn entry_path_in(dir: &Path, cache_key: &str) -> PathBuf {
    entry_path_with_prefix(dir, "listing-", cache_key)
}

/// Deterministic filename for `cache_key` inside `dir`, under `prefix` (FNV-1a
/// hash → hex, so no path-unsafe characters from the URL leak into the
/// filename). `prefix` lets two callers share one directory — and one
/// `private_dir`/prune/invalidate story — while keeping their entries
/// distinguishable by filename.
pub(crate) fn entry_path_with_prefix(dir: &Path, prefix: &str, cache_key: &str) -> PathBuf {
    // FNV-1a 64-bit — dependency-free and more than adequate for a filename.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in cache_key.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    dir.join(format!("{prefix}{hash:016x}.json"))
}

/// The on-disk entry shape: the validator ETag plus the raw JSON body it
/// validated, so a `304` can reconstruct the exact prior parse.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DiskEntry {
    pub(crate) etag: String,
    pub(crate) body: String,
}

/// Read the entry at `path` — only from a [`private_dir`] (never from a
/// directory another local user could have planted entries in).
pub(crate) fn read_disk_entry(path: &Path) -> Option<DiskEntry> {
    read_private_json(path)
}

/// Read any JSON value stored at `path` by [`write_private_json`] — only from
/// a [`private_dir`], exactly like [`read_disk_entry`].
pub(crate) fn read_private_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    if !private_dir(path.parent()?, false) {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Is `dir` safe to read cache entries from / write them into? On unix: a
/// real directory (not a symlink) owned by the current uid, tightened to
/// `0700`. A directory owned by someone else is always refused. An own
/// directory that others could write is refused for reads; with `create`
/// (writers) it is tightened and its contents purged first. With `create`, a
/// missing directory is created `0700`. The store degrades to "no cache",
/// never to a planted `(etag, body)` pair being served on a `304`.
pub(crate) fn private_dir(dir: &Path, create: bool) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        if create && std::fs::symlink_metadata(dir).is_err() {
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir);
        }
        let Ok(meta) = std::fs::symlink_metadata(dir) else {
            return false;
        };
        // SAFETY: geteuid only reads the caller's effective user id.
        let owned = meta.uid() == unsafe { libc::geteuid() };
        if !meta.is_dir() || !owned {
            log::debug!("forge_etag_store: refusing untrusted cache dir {}", dir.display());
            return false;
        }
        let others_could_write = meta.mode() & 0o022 != 0;
        if others_could_write && !create {
            return false;
        }
        if meta.mode() & 0o077 != 0 {
            if std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).is_err() {
                return false;
            }
            // Our own dir, but others could write it (e.g. a umask-002
            // `create_dir_all` from an older loom): anything in it may be
            // planted, so purge it before trusting the now-0700 dir.
            if others_could_write {
                for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        true
    }
    #[cfg(not(unix))]
    {
        !create || std::fs::create_dir_all(dir).is_ok()
    }
}

/// Create `path` for writing, owner-only (`0600` on unix), failing if it exists.
pub(crate) fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)
}

/// Write the entry atomically (temp file + rename) so a concurrent reader on
/// the same host never observes a half-written file. Best-effort: any failure
/// just means the next call re-fetches.
pub(crate) fn write_disk_entry(path: &Path, entry: &DiskEntry) {
    write_private_json(path, entry);
}

/// [`write_disk_entry`]'s atomic, owner-only write for any serializable value
/// (the repo-facts records of [`crate::forge_repo_facts`] share this store).
pub(crate) fn write_private_json<T: serde::Serialize>(path: &Path, value: &T) {
    let Ok(serialized) = serde_json::to_string(value) else {
        return;
    };
    write_private_atomic(path, serialized.as_bytes());
}

/// Write `bytes` to `path` atomically (temp file + rename), owner-only, and
/// only inside a [`private_dir`]. Best-effort, like [`write_disk_entry`]:
/// any failure leaves the previous file (or none) in place.
pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    let Some(dir) = path.parent() else { return };
    if !private_dir(dir, true) {
        return;
    }
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        path.file_name().and_then(|n| n.to_str()).unwrap_or("entry")
    ));
    // A stale temp from a crashed writer with a recycled pid: drop and retry.
    let file = create_private_file(&tmp).or_else(|_| {
        let _ = std::fs::remove_file(&tmp);
        create_private_file(&tmp)
    });
    let Ok(mut file) = file else { return };
    if file.write_all(bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Who issued a conditional read and which inventoried forge operation it
/// serves (#9831).
///
/// Both halves are required: `caller` is the facade's telemetry name (a valid
/// [`Operation`], lowercase `snake_case` segments) and `op` the inventory row
/// the read is accounted under. There is no default `op`, so a new conditional
/// reader cannot land in the `unknown` row by omission — a site that maps to
/// no inventoried operation says so with
/// [`crate::forge_call_stats::ForgeOp::uninventoried`] and a reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConditionalRead {
    pub(crate) caller: &'static str,
    pub(crate) op: crate::forge_call_stats::ForgeOp,
}

impl ConditionalRead {
    #[must_use]
    pub(crate) const fn new(caller: &'static str, op: crate::forge_call_stats::ForgeOp) -> Self {
        Self { caller, op }
    }
}

/// One `gh api --include <url>` invocation against `target`, optionally
/// conditional on `etag`,
/// run in `cwd` under that root's credential (#5401). Every call is recorded
/// against `site` in [`crate::forge_call_stats`] (#9251, by the
/// [`GhInvocation`] facade since #10089) — a local bookkeeping write, never an
/// extra forge call — keyed by `site.op` and `target`'s host and repository
/// (#9831).
pub(crate) fn fetch_conditional(
    site: ConditionalRead,
    gh_bin: &Path,
    cwd: Option<&Path>,
    target: &Target,
    url: &str,
    etag: Option<&str>,
) -> Result<(ExitStatus, Option<HttpResponse>, String)> {
    fetch_conditional_via(site, gh_bin, cwd, target, url, etag, &|req| {
        crate::forge_identity::route_read(req, std::time::SystemTime::now())
    })
}

/// [`fetch_conditional`] with the reader routing injected (tests pass a
/// fixed [`crate::forge_identity::RouteDecision`]).
pub(crate) fn fetch_conditional_via(
    site: ConditionalRead,
    gh_bin: &Path,
    cwd: Option<&Path>,
    target: &Target,
    url: &str,
    etag: Option<&str>,
    route: &dyn Fn(
        &crate::forge_identity::RouteRequest<'_>,
    ) -> crate::forge_identity::RouteDecision,
) -> Result<(ExitStatus, Option<HttpResponse>, String)> {
    // #9537: a listing is a read, so it goes to the repo's reader App when one
    // is usable. On a credential failure the reader is withdrawn and the SAME
    // request is retried once on the writer, so a broken reader costs one
    // extra call, never a failed poll (the shared shape,
    // `forge_identity::reader_then_writer`, #9872). The cache key deliberately
    // stays on the writer's credential scope: reader choice is deterministic
    // per URL (W4-B: the URL is the affinity key, so a split repo's URL
    // always lands on the same reader), so keeping the key means no ETag is
    // invalidated when readers come online. The caller's stored ETag is sent
    // to whichever reader serves the read; GitHub answers 304 only when that
    // reader's own validator matches, so a URL that moved readers (a spill,
    // or a split rolled out) costs at most one 200, never a stale body.
    let affinity = crate::gh_invocation::url_affinity_key(url);
    let reader = target.repo.as_deref().and_then(|r| {
        route(
            &crate::forge_identity::RouteRequest::gate(
                r,
                target.host.as_deref(),
                crate::forge_bucket_book::Resource::Core,
            )
            .affinity(Some(&affinity)),
        )
        .into_credential()
    });
    let reader_dir = reader.as_ref().map(|(dir, _)| dir.as_path());
    let http_ok = |r: &FetchResult| {
        r.0.success() || matches!(r.1.as_ref().map(|h| h.status), Some(200 | 304))
    };
    crate::forge_identity::reader_then_writer(
        reader_dir,
        // The reader attempt drops env tokens (#9872): `gh` prefers an env
        // `GH_TOKEN`/`GITHUB_TOKEN` over `GH_CONFIG_DIR`, so without this an
        // ambient personal token would serve the "reader" read.
        |dir, role| {
            // The reader's public bucket label (#10232), only on the reader attempt.
            let bucket = dir.and(reader.as_ref()).map(|(_, app_id)| {
                crate::forge_identity::reader_bucket(
                    app_id,
                    target.repo.as_deref().unwrap_or_default(),
                )
            });
            run_fetch_with(
                site,
                gh_bin,
                cwd,
                target,
                url,
                etag,
                dir,
                dir.is_some(),
                role,
                bucket.as_deref(),
            )
        },
        http_ok,
        reader_failure,
        |failure, _| {
            if let Some((_, app_id)) = &reader {
                let repo = target.repo.as_deref().unwrap_or_default();
                let why = format!("{} {url}", site.caller);
                crate::forge_identity::withdraw_after(app_id, repo, failure, &why);
            }
        },
    )
}

/// What a failed conditional read says about its credential. The parsed
/// `--include` headers travel with it (W4-A), so a rate-limit refusal names
/// its pool and its real `x-ratelimit-reset` rather than the flat default.
pub(crate) fn reader_failure(r: &FetchResult) -> Option<crate::forge_identity::Failure> {
    crate::forge_identity::classify_failure(
        &r.2,
        r.1.as_ref().map(|h| h.status),
        r.1.as_ref().map(|h| &h.ratelimit),
        crate::forge_bucket_book::Resource::Core,
    )
}

/// `(exit status, parsed `--include` response, trimmed stderr)` of one fetch.
pub(crate) type FetchResult = (ExitStatus, Option<HttpResponse>, String);

/// One `gh api --include <url>` read under **exactly** the reader App whose
/// `GH_CONFIG_DIR` is `reader_dir` — the reader-only primitive (#10263).
///
/// Unlike [`fetch_conditional`] there is no writer anywhere on this path: a
/// failed read is returned as-is for the caller to classify (and withdraw the
/// reader through [`crate::forge_identity::withdraw_after`]), never retried on
/// the writer, and the child runs
/// [`GhInvocation::without_token_env`], so an env `GH_TOKEN` / `GITHUB_TOKEN`
/// cannot outrank the reader dir. A caller that must never spend the
/// operator's credential — the ETA fleet snapshot refresh — uses this and
/// nothing else. Call accounting, egress routing and the rate-limit headers
/// are the same [`GhInvocation`] path every conditional read takes.
pub(crate) fn fetch_with_reader(
    site: ConditionalRead,
    gh_bin: &Path,
    cwd: Option<&Path>,
    target: &Target,
    url: &str,
    etag: Option<&str>,
    reader_dir: &Path,
) -> Result<(ExitStatus, Option<HttpResponse>, String)> {
    let role = crate::forge_identity::IdentityRole::Reader;
    run_fetch_with(site, gh_bin, cwd, target, url, etag, Some(reader_dir), true, role, None)
}

/// Deadline for one conditional read (they were unbounded `.output()`s before
/// #10089). A listing is one page, so a minute is generous.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// One `gh api --include` run. `reader_dir` = `Some` runs it under that
/// reader's `GH_CONFIG_DIR`; `None` under the writer's (#5401 per-owner, else
/// process-global). `strip_token_env` removes every token env var from the
/// child ([`GhInvocation::without_token_env`]); `role` is the identity the
/// accounting row records (#9872).
///
/// Built through [`GhInvocation`] (#10089), which records the call against
/// `site` in [`crate::forge_call_stats`] from the `--include` status line
/// and rate-limit headers — the same [`crate::forge_call_stats::classify`]
/// the hand-rolled record call here used. [`GhTarget::None`] on purpose: the
/// writer credential stays [`writer_config_dir`]'s (#5401, #5431) and nothing
/// else, so the identity a call runs under keeps matching
/// [`credential_scope`]'s cache key.
#[allow(clippy::too_many_arguments)]
fn run_fetch_with(
    site: ConditionalRead,
    gh_bin: &Path,
    cwd: Option<&Path>,
    target: &Target,
    url: &str,
    etag: Option<&str>,
    reader_dir: Option<&Path>,
    strip_token_env: bool,
    role: crate::forge_identity::IdentityRole,
    bucket: Option<&str>,
) -> Result<FetchResult> {
    let mut inv = GhInvocation::new(
        Operation::new(site.caller),
        AccessIntent::Read,
        GhTarget::None,
        FETCH_TIMEOUT,
    )
    .forge_op(site.op)
    // Accounting only (#9831): the resolved host and repo, as two fields.
    .identity_scope(target.host.as_deref(), target.repo.as_deref())
    .program(gh_bin)
    .identity_role(role)
    .args(["api", "--include", url]);
    if let Some(bucket) = bucket {
        inv = inv.identity_bucket(bucket);
    }
    if let Some(host) = &target.host {
        // The URL names the remote-resolved repo explicitly, so name its host
        // too (gh would otherwise use its default host, not the remote's).
        inv = inv.arg("--hostname").arg(host);
    }
    if let Some(e) = etag {
        inv = inv.arg("-H").arg(format!("If-None-Match: {e}"));
    }
    if let Some(dir) = cwd {
        inv = inv.current_dir(dir);
    }
    if strip_token_env {
        inv = inv.without_token_env();
    }
    // A cwd-less writer read (`visibility.repo`) keeps its owner's credential
    // (#5431); with a `cwd` the facade's root lookup already applies.
    let owner_dir = (reader_dir.is_none() && cwd.is_none())
        .then(|| writer_config_dir(None, target))
        .flatten();
    let out = match inv
        .gh_config_dir(reader_dir.or(owner_dir.as_deref()))
        .execute()
    {
        Ok(GhCompletion::Captured(Completion::Exited(out))) => out,
        Ok(_) => anyhow::bail!("gh api {url} timed out after {}s", FETCH_TIMEOUT.as_secs()),
        Err(e) => return Err(e).with_context(|| format!("failed to invoke {}", gh_bin.display())),
    };
    let response = parse_http_response(&String::from_utf8_lossy(&out.stdout));
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Ok((out.status, response, stderr))
}

// ============================================================================
// Cache key: resolved repo + host + credential identity
// ============================================================================

/// `(host, owner/repo)` parsed from a git remote URL (`git@host:o/r(.git)`,
/// `ssh://git@host/o/r`, `https://host/o/r`, `http://host/o/r`).
pub(crate) fn parse_remote_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let stripped = url.strip_suffix(".git").unwrap_or(url);
    let (host, path) = if let Some(rest) = stripped.strip_prefix("git@") {
        rest.split_once(':')?
    } else {
        let rest = ["ssh://", "https://", "http://"]
            .iter()
            .find_map(|p| stripped.strip_prefix(p))?;
        let (authority, path) = rest.split_once('/')?;
        (authority.rsplit('@').next().unwrap_or(authority), path)
    };
    let (owner, repo) = path.trim_matches('/').split_once('/')?;
    if host.is_empty() || owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((host.to_ascii_lowercase(), format!("{owner}/{repo}")))
}

/// `cwd`'s `origin` remote as `(host, owner/repo)`, memoised per root: the
/// daemon's loops hit ~58 roots × several labels every tick, and forking
/// `git remote get-url` each time is pure waste. Only successful resolutions
/// are memoised, so a not-yet-configured remote is re-tried next call.
///
/// With repo facts on ([`crate::forge_repo_facts::enabled`]) the memo is
/// invalidated by the checkout's git-config fingerprint, so an origin moved
/// with `git remote set-url` is seen without a restart. `LOOM_REPO_FACTS=0`
/// restores the process-lifetime memo below exactly.
pub(crate) fn remote_identity(cwd: &Path) -> Option<(String, String)> {
    if crate::forge_repo_facts::enabled() {
        return crate::forge_repo_facts::origin_identity(cwd);
    }
    static MEMO: OnceLock<Mutex<HashMap<PathBuf, (String, String)>>> = OnceLock::new();
    let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = memo.lock().ok().and_then(|m| m.get(cwd).cloned()) {
        return Some(hit);
    }
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let resolved = parse_remote_url(&String::from_utf8_lossy(&output.stdout))?;
    if let Ok(mut m) = memo.lock() {
        m.insert(cwd.to_path_buf(), resolved.clone());
    }
    Some(resolved)
}

/// What a conditional GET is issued against — resolved ONCE and used for both
/// the request URL and the cache key, so the two can never disagree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Target {
    /// `owner/repo` to name explicitly in the URL; `None` keeps gh's
    /// `{owner}/{repo}` placeholder (resolution failed).
    pub(crate) repo: Option<String>,
    /// The remote's host, passed as `gh api --hostname`; `None` = gh's default
    /// (`GH_HOST` / github.com), used with an explicit `--repo`/`LOOM_REPO`.
    pub(crate) host: Option<String>,
}

/// Resolve the target for a call from `cwd` (#7275, #9252): an explicit
/// `--repo`/`LOOM_REPO` wins; else `cwd`'s `origin` remote (memoised). The URL
/// is then built from THIS repo — never gh's placeholder, whose own remote
/// choice (`gh repo set-default`, then `upstream` before `origin`) could query
/// a different repo under the same key. Only when neither resolves does the
/// placeholder form (keyed by the raw `cwd`) remain.
pub(crate) fn resolve_target(cwd: Option<&Path>, repo: Option<&str>) -> Target {
    if let Some(r) = repo {
        return Target {
            repo: Some(r.to_string()),
            host: None,
        };
    }
    match cwd.and_then(remote_identity) {
        Some((host, nwo)) => Target {
            repo: Some(nwo),
            host: Some(host),
        },
        None => Target::default(),
    }
}

/// The repo-identifying key component: the resolved repo; else the raw `cwd`
/// path (still per-location); else `""` when there is neither.
pub(crate) fn repo_scope(cwd: Option<&Path>, target: &Target) -> String {
    match (&target.repo, cwd) {
        (Some(r), _) => r.clone(),
        (None, Some(dir)) => dir.display().to_string(),
        (None, None) => String::new(),
    }
}

/// The forge host: the remote's, else `GH_HOST`, else `github.com`.
fn host_scope(target: &Target) -> String {
    target
        .host
        .clone()
        .or_else(|| std::env::var("GH_HOST").ok().filter(|h| !h.is_empty()))
        .unwrap_or_else(|| "github.com".to_string())
}

/// The writer's per-owner `GH_CONFIG_DIR` for a read from `cwd` against
/// `target`: the root's (#5401); with no `cwd`, the target owner's (#5431,
/// the slug lookup a typed `GhTarget::Repo` gets from the facade). `None` =
/// the process-global credential.
fn writer_config_dir(cwd: Option<&Path>, target: &Target) -> Option<PathBuf> {
    match cwd {
        Some(dir) => crate::credential_preflight::gh_config_dir_for_root(dir),
        None => target
            .repo
            .as_deref()
            .and_then(crate::credential_preflight::gh_config_dir_for_owner_slug),
    }
}

/// The `gh` credential identity a call from `cwd` against `target` runs
/// under: [`writer_config_dir`], else the process's own `GH_CONFIG_DIR`,
/// else `default`; plus a truncated SHA-256 fingerprint of any env token
/// (which overrides the config dir in `gh`). Never the token.
pub(crate) fn credential_scope(cwd: Option<&Path>, target: &Target) -> String {
    let owner_config = writer_config_dir(cwd, target);
    let token = ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|t| !t.is_empty()));
    credential_scope_with(owner_config.as_deref(), token.as_deref())
}

fn credential_scope_with(owner_config: Option<&Path>, token: Option<&str>) -> String {
    let config = owner_config
        .map(|p| p.display().to_string())
        .or_else(|| {
            std::env::var("GH_CONFIG_DIR")
                .ok()
                .filter(|d| !d.is_empty())
        })
        .unwrap_or_else(|| "default".to_string());
    let fingerprint = token.map_or_else(|| "-".to_string(), crate::short_hash::short_sha16);
    format!("{config};{fingerprint}")
}

/// The shared cache key for a conditional GET of `url` (built from `target`)
/// issued from `cwd`.
pub(crate) fn cache_key(cwd: Option<&Path>, target: &Target, url: &str) -> String {
    format!(
        "{}|{}|{}|{url}",
        repo_scope(cwd, target),
        host_scope(target),
        credential_scope(cwd, target)
    )
}

// ============================================================================
// Daemon persistence layer + test isolation
// ============================================================================

/// Where the daemon's listing cache persists (#9252): the shared store.
#[cfg(not(test))]
pub(crate) fn daemon_store_dir() -> Option<PathBuf> {
    Some(disk_cache_dir())
}

/// Test builds: OFF unless the current test thread opts in via
/// [`set_test_daemon_store_dir`]. Dozens of fake-`gh` stub tests across the
/// crate reuse fixed ETags (`W/"round1"`) — a default-on real temp dir (or a
/// process-global env var) would let them read each other's entries.
#[cfg(test)]
pub(crate) fn daemon_store_dir() -> Option<PathBuf> {
    TEST_DAEMON_STORE_DIR.with(|d| d.borrow().clone())
}

#[cfg(test)]
thread_local! {
    static TEST_DAEMON_STORE_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point THIS test thread's daemon listing cache at `dir` (`None` = off).
#[cfg(test)]
pub(crate) fn set_test_daemon_store_dir(dir: Option<PathBuf>) {
    TEST_DAEMON_STORE_DIR.with(|d| *d.borrow_mut() = dir);
}

/// The daemon's in-memory/disk key: [`cache_key`]. In test builds with no
/// opted-in store the raw `cwd` is prefixed, keeping the pre-#9252 per-`cwd`
/// isolation between parallel stub tests that share a remote and an ETag.
pub(crate) fn daemon_cache_key(cwd: Option<&Path>, target: &Target, url: &str) -> String {
    let key = cache_key(cwd, target, url);
    #[cfg(test)]
    {
        if daemon_store_dir().is_none() {
            let raw = cwd.map(|d| d.display().to_string()).unwrap_or_default();
            return format!("{raw}#{key}");
        }
    }
    key
}

// ============================================================================
// One conditional GET with a memory + disk ETag cache
// ============================================================================

/// Conditional GET of the REST path `url` against `repo` (else `cwd`'s
/// remote), accounted under `site` (#9831): `Ok(Some(body))` on a `200`, or on
/// a `304` served from the stored body; `Ok(None)` on a `404`.
///
/// The one copy of this mechanism (#10480): star liveness and the batched
/// `check-stale-blocked` gatherer both call it. Entries live in a
/// process-global memory map backed by [`daemon_store_dir`] under
/// `prefix` (`"star-"`, `"stale-"`), so a short-lived CLI process still gets
/// a free `304` from the previous run's ETag. A `304` with nothing sent is
/// anomalous: the entry is dropped and the read fails, so the next call
/// re-fetches unconditionally. Honours the rate-limit breaker under
/// `site.caller`.
///
/// # Errors
/// The breaker is suppressing calls, `gh` could not run, or the read failed
/// with anything but a `404`.
pub(crate) fn cached_get(
    site: ConditionalRead,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo: Option<&str>,
    url: &str,
    prefix: &'static str,
) -> Result<Option<String>> {
    cached_read(site, gh_bin, cwd, repo, url, prefix).map(|r| r.body)
}

/// What one [`cached_read`] answered, for a caller that accounts its own
/// spend (#10480's `forge_cost`): the body as [`cached_get`] returns it,
/// whether the forge answered `304` (free on the core bucket), and the
/// response's `x-ratelimit-remaining` when it is the core pool's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedRead {
    pub(crate) body: Option<String>,
    pub(crate) not_modified: bool,
    pub(crate) core_remaining: Option<u64>,
}

/// [`cached_get`], also reporting the HTTP outcome ([`CachedRead`]).
///
/// # Errors
/// As [`cached_get`].
pub(crate) fn cached_read(
    site: ConditionalRead,
    gh_bin: &Path,
    cwd: Option<&Path>,
    repo: Option<&str>,
    url: &str,
    prefix: &'static str,
) -> Result<CachedRead> {
    if crate::rate_limit_breaker::global_skip_pass(site.caller) {
        anyhow::bail!("rate-limit breaker is suppressing forge calls");
    }
    let target = resolve_target(cwd, repo);
    let key = daemon_cache_key(cwd, &target, url);
    let mem_key = format!("{prefix}{key}");
    let disk = daemon_store_dir().map(|d| entry_path_with_prefix(&d, prefix, &key));
    let sent = get_cache()
        .lock()
        .ok()
        .and_then(|m| m.get(&mem_key).cloned())
        .or_else(|| Some(std::sync::Arc::new(read_disk_entry(disk.as_deref()?)?)));
    let sent_etag = sent.as_ref().map(|e| e.etag.as_str());
    let (status, response, stderr) = fetch_conditional(site, gh_bin, cwd, &target, url, sent_etag)?;
    let core_remaining = response.as_ref().and_then(|r| {
        let core = r
            .ratelimit
            .resource
            .as_deref()
            .is_none_or(|res| res == "core");
        r.ratelimit.remaining.filter(|_| core)
    });
    let answered = |body: Option<String>, not_modified: bool| CachedRead {
        body,
        not_modified,
        core_remaining,
    };
    match response {
        Some(r) if r.status == 304 => match sent {
            Some(e) => Ok(answered(Some(e.body.clone()), true)),
            None => {
                if let Ok(mut m) = get_cache().lock() {
                    m.remove(&mem_key);
                }
                if let Some(p) = &disk {
                    let _ = std::fs::remove_file(p);
                }
                anyhow::bail!("gh api {url}: 304 but the cache entry vanished")
            }
        },
        Some(r) if r.status == 200 && status.success() => {
            if let Some(etag) = r.etag.clone() {
                let entry = DiskEntry {
                    etag,
                    body: r.body.clone(),
                };
                if let Some(p) = &disk {
                    write_disk_entry(p, &entry);
                }
                if let Ok(mut m) = get_cache().lock() {
                    m.insert(mem_key, std::sync::Arc::new(entry));
                }
            }
            Ok(answered(Some(r.body), false))
        }
        Some(r) if r.status == 404 => Ok(answered(None, false)),
        _ => {
            crate::rate_limit_breaker::global_observe_failure(&stderr, site.caller);
            anyhow::bail!("gh api {url} failed: {stderr}")
        }
    }
}

/// [`cached_get`]'s process-global hot layer, keyed `prefix + cache key`.
fn get_cache() -> &'static Mutex<HashMap<String, std::sync::Arc<DiskEntry>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, std::sync::Arc<DiskEntry>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_urls_with_host() {
        let gh = |u: &str| parse_remote_url(u);
        let want = Some(("github.com".to_string(), "o/r".to_string()));
        assert_eq!(gh("git@github.com:o/r.git"), want);
        assert_eq!(gh("https://github.com/o/r.git\n"), want);
        assert_eq!(gh("ssh://git@github.com/o/r"), want);
        assert_eq!(
            gh("https://GHE.example.com/o/r"),
            Some(("ghe.example.com".to_string(), "o/r".to_string()))
        );
        assert_eq!(gh("file:///tmp/x"), None);
        assert_eq!(gh("https://github.com/only-owner"), None);
    }

    #[test]
    fn credential_scope_never_contains_the_token_and_distinguishes_tokens() {
        let a = credential_scope_with(None, Some("ghs_secret_token_value_a"));
        let b = credential_scope_with(None, Some("ghs_secret_token_value_b"));
        assert!(!a.contains("ghs_secret"));
        assert_ne!(a, b, "two env tokens are two identities");
        assert_ne!(a, credential_scope_with(None, None));
    }

    #[test]
    fn a_per_owner_config_dir_is_a_distinct_identity() {
        let owner_a = credential_scope_with(Some(Path::new("/cfg/owner-a")), None);
        let owner_b = credential_scope_with(Some(Path::new("/cfg/owner-b")), None);
        assert_ne!(owner_a, owner_b, "a per-owner GH_CONFIG_DIR (#5401) is its own identity");
        assert!(owner_a.starts_with("/cfg/owner-a;"));
    }

    /// Entries are written into a `0700` directory as `0600` files.
    #[cfg(unix)]
    #[test]
    fn entries_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("store");
        let path = entry_path_in(&dir, "k");
        let entry = DiskEntry {
            etag: "W/\"e\"".into(),
            body: "[]".into(),
        };
        write_disk_entry(&path, &entry);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(read_disk_entry(&path).unwrap().etag, "W/\"e\"");
        // An own directory from an older loom (0755) is tightened on write.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_disk_entry(&path, &entry);
        assert_eq!(mode(&dir), 0o700);
    }

    /// A group/world-writable directory is never read from; a writer tightens
    /// its own such dir to `0700` and purges it first, so a planted
    /// `(etag, body)` pair can never be served on a `304`.
    #[cfg(unix)]
    #[test]
    fn a_group_or_world_writable_dir_is_never_read_and_is_purged_before_reuse() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("store");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let planted = entry_path_in(&dir, "k");
        std::fs::write(&planted, r#"{"etag":"W/\"real\"","body":"[]"}"#).unwrap();
        assert!(read_disk_entry(&planted).is_none(), "planted entry must not be read");
        let other = entry_path_in(&dir, "other");
        let entry = DiskEntry {
            etag: "e".into(),
            body: "[]".into(),
        };
        write_disk_entry(&other, &entry);
        assert!(!planted.exists(), "a writer purges an others-writable dir first");
        assert!(other.exists());
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // A symlinked "directory" is refused too.
        let link = base.path().join("link");
        std::os::unix::fs::symlink(base.path(), &link).unwrap();
        assert!(!private_dir(&link, false));
    }
}

#[cfg(test)]
#[path = "forge_etag_store_route_tests.rs"]
mod route_tests;
