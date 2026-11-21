//! The host-owned per-repository object mirror (#14680, operator ruling
//! 2026-11-19, option b).
//!
//! One mirror per registered forge repository lives on the host and is
//! populated and refreshed SOLELY from that repository's canonical origin
//! through a proved read authority (#14716): the fetch runs against the
//! exact verified route with the exact verified credential
//! ([`forge_auth::ProvedReader`]), in a child environment stripped of every
//! ambient source — an inherited `GIT_CONFIG_PARAMETERS` included — and
//! against a mirror config verified to carry only the keys this module
//! itself wrote. Seat clones, seat refs and unpublished seat objects are
//! never inputs to the store.
//!
//! Retention is the contract, not an accident: the fetch never prunes, the
//! mirror disables automatic maintenance at the config level, and a failed
//! fetch leaves the previous object set — and every borrower record —
//! exactly as it was, so an object whose upstream ref moved or vanished
//! stays readable for every seat that may still reference it.
//!
//! Publication is a separate fail-closed decision from admission
//! ([`read_authority::publish`] / [`read_authority::admit_borrower`]): a
//! store pins the FIRST proved grant it was created under, a later
//! credential under the same authority rotates through freely, and a
//! different authority never inherits the store. A proof with no durable
//! expiry (Gitea) is in-flight only by construction here: the store never
//! persists a credential or a proof, so every publication starts from a
//! fresh verification against the forge.
//!
//! Seat-side borrowing — the read-only objects bind, the single verified
//! alternate in seat clones, and idle in-place migration — is the next
//! slice of #14680 and consumes [`MirrorStore::attach`],
//! [`MirrorStore::detach`] and [`MirrorStore::objects_dir`] for that.
//! Until it lands, seats keep fully private clones with no alternate,
//! exactly as before.
use super::forge_auth::ProvedReader;
use super::read_authority::{
    self, Admission, BorrowerAuthority, Credential, LiveProviderHttp, Provider, Publication,
    ReadGrant, ReadProof, ScopedMinter, ScriptScopedMinter, Verification, VerificationRequest,
};
use super::*;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// The on-disk identity of one mirror directory.
pub(super) const PROTOCOL: &str = "loom-mirror-v1";
/// Env override for the host-wide mirror root. Production resolves
/// `~/.loom/mirrors` when unset; tests always pass roots explicitly.
pub(super) const MIRROR_ROOT_ENV: &str = "LOOM_MIRROR_ROOT";
/// Bound for one mirror fetch. Generous on purpose: the FIRST fetch of a
/// large repository is a full clone-equivalent, and a bound that is too
/// tight would make every first mirror useless while the store stays empty.
const FETCH_TIMEOUT: Duration = Duration::from_secs(1800);
/// Bound for store-local git plumbing (`init`, `config`): no network.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a refresh waits for another seat's concurrent refresh of the
/// SAME repository: two seats starting at once converge on one usable store
/// (the lock serializes them), and a refresh that cannot converge in time
/// is skipped — never fatal to the session.
const LOCK_WAIT: Duration = Duration::from_secs(300);
/// Bound for one authenticated provider probe of a verification.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

const GIT_DIR: &str = "objects.git";
const MARKER: &str = "mirror.json";
const GRANT: &str = "grant.json";
const BORROWERS: &str = "borrowers";
const LOCK: &str = "lock";

/// What one refresh did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refreshed {
    /// The store was created under this proof's grant and fetched.
    Created,
    /// An existing store was refreshed (or an interrupted creation was
    /// completed — the same completion path, under the store's lock).
    Fetched,
}

/// One host-owned mirror store: a bare mirror repository, the pinned read
/// grant it was proved under, and the borrower records the next publication
/// decision checks. All secrets stay in flight: only [`ReadGrant`]s —
/// secret-free by construction — are ever written.
pub(super) struct MirrorStore {
    dir: PathBuf,
    id: RepoId,
}

#[derive(Serialize, Deserialize)]
struct Marker {
    protocol: String,
    key: String,
}

#[derive(Serialize, Deserialize)]
struct PinnedGrant {
    grant: ReadGrant,
}

impl MirrorStore {
    /// The store for `id` under `root`. Validates any existing directory's
    /// marker before anything is written; creates nothing.
    ///
    /// # Errors
    /// When the directory exists but is not this repository's mirror (a
    /// foreign or unrecognizable store is retained for recovery, never
    /// adopted or reset). A directory holding nothing but the lock file is
    /// an interrupted first refresh, not a foreign store.
    pub(super) fn open(root: &Path, id: &RepoId) -> Result<Self> {
        let dir = root.join(&id.slug);
        if let Ok(meta) = std::fs::symlink_metadata(&dir) {
            if !meta.is_dir() || meta.file_type().is_symlink() {
                bail!(
                    "mirror directory {} is not a real directory; retain it for recovery",
                    dir.display()
                );
            }
            match read_json::<Marker>(&dir.join(MARKER)) {
                Ok(Some(marker)) => {
                    if marker.protocol != PROTOCOL || marker.key != id.key {
                        bail!(
                            "mirror directory does not belong to repository {}; retain it for \
                             recovery",
                            id.key
                        );
                    }
                }
                Ok(None) => {
                    // No marker: fresh only when nothing but the lock file is
                    // there (the interrupted first refresh). Anything else is
                    // not ours to adopt.
                    let foreign = std::fs::read_dir(&dir)?
                        .filter_map(std::result::Result::ok)
                        .any(|entry| entry.file_name() != LOCK);
                    if foreign {
                        bail!(
                            "mirror directory holds no recognizable marker; retain it for \
                             recovery"
                        );
                    }
                }
                Err(error) => {
                    bail!("mirror marker is unreadable ({error}); retain it for recovery")
                }
            }
        }
        Ok(Self {
            dir,
            id: id.clone(),
        })
    }

    #[cfg(test)]
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(super) fn git_dir(&self) -> PathBuf {
        self.dir.join(GIT_DIR)
    }

    /// The objects directory a seat-side read-only bind mounts (the next
    /// slice of #14680). Nothing in this module ever exposes it writable.
    #[allow(dead_code)] // seat binds land with #14680's seat-side slice
    pub(super) fn objects_dir(&self) -> PathBuf {
        self.git_dir().join("objects")
    }

    /// The pinned read grant, or `None` for a store that was never proved.
    pub(super) fn pinned_grant(&self) -> Result<Option<ReadGrant>> {
        Ok(read_json::<PinnedGrant>(&self.dir.join(GRANT))?.map(|pinned| pinned.grant))
    }

    /// Refresh the store from origin under `proof`: create it when absent
    /// (pinning the proof's grant), then fetch through the proved reader —
    /// but only when the publication decision allows it.
    ///
    /// # Errors
    /// Fail-closed on every refusal and every fetch failure. The previous
    /// object set and borrower records survive all of them: a mirror fetch
    /// is additive (nothing is pruned or repacked away).
    pub(super) fn refresh(&self, proof: &ReadProof, now: SystemTime) -> Result<Refreshed> {
        self.refresh_with(proof, now, &GitFetch)
    }

    /// [`MirrorStore::refresh`] with the fetch leg injected (tests).
    pub(super) fn refresh_with(
        &self,
        proof: &ReadProof,
        now: SystemTime,
        fetch: &dyn MirrorFetch,
    ) -> Result<Refreshed> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let created = self.ensure(proof)?;
        let pinned = self
            .pinned_grant()?
            .context("mirror store pins no grant after creation; retain it for recovery")?;
        let borrowers = self.borrower_authorities()?;
        match read_authority::publish(Some(proof), &pinned, &borrowers, now) {
            Publication::Publish { .. } => {}
            Publication::Withheld { reason } => {
                bail!(
                    "mirror publication withheld ({reason:?}); the retained store and its \
                     borrowers are unchanged"
                );
            }
        }
        let reader = ProvedReader::new(proof, &self.id.url).context(
            "the proved route does not name this mirror's canonical origin; refusing to fetch",
        )?;
        fetch.fetch(&self.git_dir(), &reader)?;
        Ok(if created {
            Refreshed::Created
        } else {
            Refreshed::Fetched
        })
    }

    /// Make the store exist under `proof`'s grant: write the marker, create
    /// the bare mirror with retention-first config, pin the FIRST proved
    /// grant. Idempotent and self-healing — every step is ordered so an
    /// interrupted creation is completed (never half-kept) by a repeat under
    /// the store lock. An existing store is never re-pinned: a later
    /// authority does not inherit it; the publication decision refuses
    /// instead. `true` when this call created the store.
    fn ensure(&self, proof: &ReadProof) -> Result<bool> {
        let created = !self.dir.join(MARKER).exists();
        if created {
            std::fs::create_dir_all(&self.dir)?;
            if std::fs::symlink_metadata(&self.dir)?
                .file_type()
                .is_symlink()
            {
                bail!("mirror directory must not be a symlink");
            }
            save(
                &self.dir.join(MARKER),
                &Marker {
                    protocol: PROTOCOL.to_owned(),
                    key: self.id.key.clone(),
                },
            )?;
        }
        // A partial mirror (marker present, git dir not yet created) is
        // completed here; a complete one is left alone.
        if !self.git_dir().join("config").exists() {
            init_bare(&self.git_dir())?;
        }
        if self.pinned_grant()?.is_none() {
            // A store that pins no grant has never been borrowed from; this
            // proof's grant is the first authority, or the completion of an
            // interrupted first refresh.
            save(
                &self.dir.join(GRANT),
                &PinnedGrant {
                    grant: proof.grant().clone(),
                },
            )?;
        }
        Ok(created)
    }

    /// Admit one seat as a borrower of this store under `proof` (the
    /// seat-side slice of #14680 calls this at bind time). A refused
    /// admission writes nothing and the seat stays on its private clone.
    ///
    /// # Errors
    /// [`read_authority::admit_borrower`]'s refusal, or a write failure.
    #[allow(dead_code)] // seat binds land with #14680's seat-side slice
    pub(super) fn attach(
        &self,
        account: &str,
        proof: &ReadProof,
        now: SystemTime,
    ) -> Result<ReadGrant> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let pinned = self.pinned_grant()?;
        match read_authority::admit_borrower(Some(proof), pinned.as_ref(), now) {
            Admission::Admitted { grant } => {
                let dir = self.dir.join(BORROWERS);
                std::fs::create_dir_all(&dir)?;
                save(
                    &dir.join(format!("{}.json", borrower_name(account))),
                    &PinnedGrant {
                        grant: grant.clone(),
                    },
                )?;
                Ok(grant)
            }
            Admission::Refused { reason } => {
                bail!("mirror admission refused ({reason:?}); the seat stays on its private clone");
            }
        }
    }

    /// Retire one borrower's record (a seat that stopped borrowing). Absent
    /// is success: there is nothing to retire.
    #[allow(dead_code)] // seat binds land with #14680's seat-side slice
    pub(super) fn detach(&self, account: &str) -> Result<()> {
        let path = self
            .dir
            .join(BORROWERS)
            .join(format!("{}.json", borrower_name(account)));
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Every attached borrower's pinned authority, as a publication decision
    /// needs it. A record that cannot be read counts as
    /// [`BorrowerAuthority::Unknown`] — never as fine.
    fn borrower_authorities(&self) -> Result<Vec<BorrowerAuthority>> {
        let dir = self.dir.join(BORROWERS);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                continue;
            }
            match read_json::<PinnedGrant>(&entry.path()) {
                Ok(Some(pinned)) => out.push(BorrowerAuthority::Pinned(pinned.grant)),
                Ok(None) | Err(_) => out.push(BorrowerAuthority::Unknown),
            }
        }
        Ok(out)
    }

    /// Bytes used by the mirror's object store — the number the one-host
    /// before/after comparison (#14680) and the recurring size metric
    /// (#14678) report, counted once per host per repository.
    #[allow(dead_code)] // consumed by #14680's measurement / #14678 telemetry
    pub(super) fn objects_bytes(&self) -> Result<u64> {
        dir_size(&self.objects_dir())
    }
}

/// The fetch leg of a refresh, as a seam: production shells `git`; tests
/// script it so every publication decision is hermetic.
pub(super) trait MirrorFetch {
    fn fetch(&self, git_dir: &Path, reader: &ProvedReader) -> Result<()>;
}

/// The production fetch: `git fetch` inside the bare mirror, with the proved
/// route on the command line and the proved credential in the child's
/// ENVIRONMENT (never argv), against a config verified to be exactly what
/// this module wrote. The child's environment is cleared wholesale, so an
/// inherited `GIT_CONFIG_PARAMETERS` cannot inject `url.*.insteadOf`
/// rewriting (#14726 release review, item 1), and `GIT_ALLOW_PROTOCOL`
/// narrows the transport to HTTPS.
struct GitFetch;

impl MirrorFetch for GitFetch {
    fn fetch(&self, git_dir: &Path, reader: &ProvedReader) -> Result<()> {
        fetch_origin(git_dir, reader.url(), &reader.fetch_env(), Some("https"))
    }
}

/// A [`ScopedMinter`] for routes that never mint (a Gitea verification
/// never reaches it); answering `NotConfigured` keeps `verify` total.
struct NoMinter;

impl ScopedMinter for NoMinter {
    fn mint_scoped(&self, _owner_repo: &str) -> read_authority::ScopedOutcome {
        read_authority::ScopedOutcome::NotConfigured
    }
}

// ---------------------------------------------------------------------------
// Store plumbing.
// ---------------------------------------------------------------------------

/// The per-store exclusive lock: serializes creation and refresh across
/// every seat and account of one repository, so two seats starting
/// concurrently converge on one usable store. Bounded waiting — a refresh
/// that cannot converge in time is skipped by the caller, never fatal.
struct StoreLock {
    _file: std::fs::File,
}

impl StoreLock {
    fn acquire(dir: &Path) -> Result<Self> {
        std::fs::DirBuilder::new().recursive(true).create(dir)?;
        // Never unlink or replace the lock inode: two callers must lock the
        // same one. O_NOFOLLOW keeps the lock a real file of this directory.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join(LOCK))?;
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self { _file: file });
            }
            if std::time::Instant::now() >= deadline {
                bail!("another refresh holds this repository's mirror; skipping this one");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The filename of one borrower's record. An account name that is not a
/// single safe path segment is hashed, never used verbatim.
fn borrower_name(account: &str) -> String {
    if safe_segment(account) && !account.contains("..") {
        return account.to_owned();
    }
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(account.as_bytes()))[..16].to_owned()
}

/// Recursive size of a directory tree, following no symlinks.
fn dir_size(path: &Path) -> Result<u64> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Ok(meta.len());
    }
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        total += dir_size(&entry?.path())?;
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// The mirror's own git plumbing.
// ---------------------------------------------------------------------------

/// One store-local git invocation, bounded, with the daemon's ambient git
/// configuration masked (system and global config unread) so nothing outside
/// this module shapes the mirror.
fn store_git(args: &[&str], timeout: Duration) -> Result<()> {
    let mut command = Command::new("git");
    command
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(std::process::Stdio::null());
    let completion = crate::proc_exec::run_bounded(command, timeout)
        .map_err(|e| anyhow::anyhow!("mirror store git could not run: {e}"))?;
    if !completion.succeeded() {
        // Git's own stderr is never surfaced: it may echo the URL or a
        // credential a helper printed (#14433's rule, kept here).
        bail!("mirror store git {} failed", args.first().unwrap_or(&"operation"));
    }
    Ok(())
}

/// Create the bare mirror repository with retention-first configuration:
/// automatic gc disabled and fetch pruning off at the config level, so even
/// a hand-run `git fetch` inside the mirror cannot drop objects a seat may
/// still reference (operator ruling 2026-11-19, condition 3).
fn init_bare(git_dir: &Path) -> Result<()> {
    store_git(&["init", "--bare", "--quiet", &git_dir.to_string_lossy()], LOCAL_TIMEOUT)?;
    let config = |key: &str, value: &str| {
        store_git(
            &[
                "--git-dir",
                &git_dir.to_string_lossy(),
                "config",
                key,
                value,
            ],
            LOCAL_TIMEOUT,
        )
    };
    config("gc.auto", "0")?;
    config("fetch.prune", "false")?;
    config("fetch.prunetags", "false")?;
    Ok(())
}

/// The only configuration keys a mirror may carry: what `git init --bare`
/// writes plus the retention keys this module sets. Anything else — an
/// `url.*.insteadOf` rewrite, an `http.*` proxy or credential route, an
/// unknown `remote.*` — refuses the fetch: repo-local config is a separate
/// injection channel the proved environment does not mask (#14726 release
/// review, item 2). Key VALUES are never surfaced, only key names.
fn verify_mirror_config(git_dir: &Path) -> Result<()> {
    let bytes = std::fs::read(git_dir.join("config"))
        .with_context(|| format!("mirror config unreadable at {}", git_dir.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let header = header.strip_suffix(']').unwrap_or(header);
            let (base, _) = header.split_once(" \"").unwrap_or((header, ""));
            section = base.to_ascii_lowercase();
            continue;
        }
        let key = line
            .split('=')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !allowed_config_key(&section, &key) {
            bail!(
                "mirror config carries {section}.{key}, which this module never wrote; refusing \
                 to fetch through it"
            );
        }
    }
    Ok(())
}

fn allowed_config_key(section: &str, key: &str) -> bool {
    matches!(
        (section, key),
        ("core", "repositoryformatversion")
            | ("core", "filemode")
            | ("core", "bare")
            | ("core", "logallrefupdates")
            | ("gc", "auto")
            | ("fetch", "prune")
            | ("fetch", "prunetags")
    )
}

/// Run one origin fetch into the bare mirror, after verifying the mirror's
/// config carries only what this module wrote (the gate lives here so no
/// caller can bypass it). `env` — together with `PATH` — REPLACES the
/// child's entire environment (so no inherited `GIT_CONFIG_PARAMETERS`
/// survives); `allow_protocol` narrows the transport. A failing fetch leaves
/// the store additive-intact: nothing here prunes, repacks or deletes.
pub(super) fn fetch_origin(
    git_dir: &Path,
    url: &str,
    env: &[(String, String)],
    allow_protocol: Option<&str>,
) -> Result<()> {
    verify_mirror_config(git_dir)?;
    let mut env = env.to_vec();
    if let Some(only) = allow_protocol {
        env.push(("GIT_ALLOW_PROTOCOL".to_owned(), only.to_owned()));
    }
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(git_dir)
        .args([
            "fetch",
            "--no-prune",
            "--no-tags",
            "--",
            url,
            "+refs/*:refs/*",
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::null());
    let completion = crate::proc_exec::run_bounded(command, FETCH_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("mirror fetch could not run: {e}"))?;
    if !completion.succeeded() {
        // Git's stderr stays unread: it may carry the URL or credential
        // material. The store keeps its previous object set either way.
        bail!("mirror fetch from the proved origin failed; the retained object set is unchanged");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Production verification and session wiring.
// ---------------------------------------------------------------------------

/// The host-wide mirror root: one store per repository per host (operator
/// ruling 2026-11-19, condition 1), machine-level beside the shared profile
/// root. `LOOM_MIRROR_ROOT` overrides it; under test the override is the
/// only source, so a test can never touch an operator's real mirrors.
pub(super) fn mirror_root() -> Result<PathBuf> {
    if let Ok(value) = std::env::var(MIRROR_ROOT_ENV) {
        if !value.trim().is_empty() {
            return Ok(PathBuf::from(value.trim()));
        }
    }
    #[cfg(test)]
    {
        bail!("no {MIRROR_ROOT_ENV} in a test; mirror tests pass roots explicitly")
    }
    #[cfg(not(test))]
    {
        let home = dirs::home_dir()
            .context("no home directory available for the repository mirror root")?;
        Ok(home.join(".loom").join("mirrors"))
    }
}

/// The GitHub host a mirror proof may be proved against: the configured
/// `GH_HOST` when it is the default public host (or unset) — never a GitHub
/// Enterprise host. The narrow mint script talks to `api.github.com` while
/// an Enterprise probe reads `<host>/api/v3`, and #14726's release review
/// requires refusing that split end to end rather than proving half a route.
/// `None` skips mirror verification entirely.
fn github_host_for_mirror() -> Option<String> {
    match std::env::var("GH_HOST") {
        Ok(host) if host.trim().eq_ignore_ascii_case("github.com") => Some(host.trim().to_owned()),
        Ok(_) => {
            log::debug!(
                "private workspace mirror: GH_HOST names a GitHub Enterprise host; mirror \
                 verification stays unsupported until the scoped mint serves it (#14726 review)"
            );
            None
        }
        Err(_) => Some("github.com".to_owned()),
    }
}

/// The exact credential selected for a Gitea route, BY NAME, from the daemon
/// process environment — never an ambient fallback: no selected credential
/// and no issuer is `NoCredential`, and the mirror is skipped (#14716).
fn selected_gitea_credential() -> Option<Credential> {
    ["GITEA_TOKEN", "FORGE_TOKEN"].into_iter().find_map(|name| {
        std::env::var(name).ok().and_then(|value| {
            (!value.is_empty() && !value.chars().any(char::is_control))
                .then(|| Credential::new(value))
        })
    })
}

/// Verify the read authority of one registered origin for a mirror refresh
/// (#14716 → #14680). GitHub needs the authorized App issuer's narrow mint
/// plus the live provider transport; Gitea needs a selected credential.
/// `None` means unproved: the caller skips the mirror and stays on the
/// legacy private-clone path.
pub(super) fn prove_for_mirror(workspace: &Path, id: &RepoId) -> Option<ReadProof> {
    let github_host = github_host_for_mirror()?;
    let base = read_authority::ForgeBase::of_repository_url(&id.url)?;
    let request = VerificationRequest {
        url: id.url.clone(),
        github_host: Some(github_host.clone()),
        gitea_credential: selected_gitea_credential(),
    };
    let minter: Box<dyn ScopedMinter> = match Provider::of_base(&base, &github_host) {
        Provider::GitHub => {
            let script = crate::credential_preflight::resolve_github_app_script(workspace)?;
            Box::new(ScriptScopedMinter {
                script_path: script,
                cwd: workspace.to_owned(),
                timeout: crate::credential_preflight::resolve_github_app_mint_timeout(workspace),
            })
        }
        Provider::Gitea => Box::new(NoMinter),
    };
    let http = LiveProviderHttp {
        timeout: PROBE_TIMEOUT,
    };
    match read_authority::verify(&request, &http, minter.as_ref(), SystemTime::now()) {
        Verification::Proved(proof) => Some(proof),
        Verification::Unproved(reason) => {
            log::debug!(
                "private workspace mirror {}: read authority unproved ({reason:?}); staying on \
                 the legacy path",
                id.key
            );
            None
        }
    }
}

/// Refresh every registered repository's mirror for one account's session
/// (#14680). Runs on the host under the caller's account lease. A mirror
/// that cannot be proved, converged or fetched is skipped with a log line:
/// the session and its private clones are never refused for a mirror's sake.
pub(super) fn refresh_session_mirrors(workspace: &Path, config: &Config) {
    let root = match mirror_root() {
        Ok(root) => root,
        Err(error) => {
            log::debug!(
                "private session {}: no mirror root configured; mirrors not refreshed ({error})",
                config.account
            );
            return;
        }
    };
    let mut targets = vec![config.primary()];
    for registered in &config.repositories {
        if let Ok(target) = config.target(Some(&registered.repository)) {
            targets.push(target);
        }
    }
    for target in targets {
        let id = match identify(&target.repository) {
            Ok(id) => id,
            Err(error) => {
                log::warn!(
                    "private session {}: mirror skipped for an unidentifiable repository \
                     ({error})",
                    config.account
                );
                continue;
            }
        };
        if let Err(error) = super::lifecycle::check_firewall(workspace, &id) {
            log::debug!(
                "private session {}: mirror skipped for {} ({error})",
                config.account,
                id.key
            );
            continue;
        }
        let store = match MirrorStore::open(&root, &id) {
            Ok(store) => store,
            Err(error) => {
                log::warn!(
                    "private session {}: mirror for {} is unusable ({error})",
                    config.account,
                    id.key
                );
                continue;
            }
        };
        let Some(proof) = prove_for_mirror(workspace, &id) else {
            continue;
        };
        match store.refresh(&proof, SystemTime::now()) {
            Ok(outcome) => {
                log::debug!("private session {}: mirror {} ({:?})", config.account, id.key, outcome)
            }
            Err(error) => log::warn!(
                "private session {}: mirror refresh for {} failed: {error}",
                config.account,
                id.key
            ),
        }
    }
}
