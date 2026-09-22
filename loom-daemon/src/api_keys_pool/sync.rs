//! Pull-based convergence of this host's API-key pool on an operator-maintained
//! external secret source (issue #8511).
//!
//! The registry ([`super::registry`]) is a **per-host** directory of files, and
//! until now its only write path was an operator running `api-keys add` once per
//! account per host. That does not survive a fleet that is mostly ephemeral
//! cloud workers: a rebuilt Spot instance boots with an empty pool and nothing
//! registers accounts for it. `api-keys sync --from <source>` closes that gap by
//! *pulling* the account set from a source of truth the operator already keeps.
//!
//! # Shape of a source
//!
//! `<source>` carries a scheme so the backend set can grow (a native
//! `ssm:/path/prefix` is the obvious next one). The first — and today only —
//! backend is `cmd:<command>`: a command whose **stdout** is
//!
//! ```text
//! <provider>/<account><TAB><KEY>=<value>
//! ```
//!
//! one account per line, `#` comments and blank lines ignored. That covers AWS
//! SSM (`get-parameters-by-path --with-decryption`), Vault, the 1Password CLI,
//! `age -d`, and anything else an operator can pipe, without Loom linking a
//! single provider SDK.
//!
//! # Rules this module is built around
//!
//! - **Key material comes off a pipe, never argv and never a log.** The source's
//!   stdout is read into memory, parsed, and dropped into `0600` files by
//!   [`super::registry::add`]. No parse error, no plan, no outcome, and no
//!   `Debug` rendering of anything here echoes a value — errors name a **line
//!   number** and a shape, and plans name **accounts**.
//! - **Fail-safe.** The source is fetched and fully parsed *before* a single
//!   file is touched, so an unreachable source or one malformed line leaves the
//!   pool byte-for-byte as it was, and exits non-zero. A transient secret-store
//!   outage must never empty a working pool.
//! - **Idempotent.** An account whose stored value already matches the source is
//!   not rewritten at all — not even with identical bytes — so its mtime, its
//!   `.disabled` entry, its `.allowlist` pin and its `.bad_accounts.json` mark
//!   all survive a sync untouched. Only a *changed* account is replaced, through
//!   the same atomic temp-file + `rename` every other pool write uses.
//! - **`--prune` is scoped to the providers the source mentions.** Convergence
//!   means "this provider's accounts are exactly what the source says", not
//!   "delete everything the source did not mention": a host that syncs `zai`
//!   from SSM and holds a hand-registered `openai` key keeps the `openai` key.
//!   A source that emits nothing therefore prunes nothing, which is the same
//!   fail-safe stated from the other direction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::paths::{
    provider_dir, validate_account, validate_provider, validate_stored_env_name, ACCOUNT_FILE_EXT,
};
use super::registry;

/// Root-level record of the last successful sync, written only after one
/// completes. Not a secret, but it lives inside the `0700` pool root and is
/// written with the same atomic replace as everything else there.
pub const SYNC_STATE_FILE: &str = ".sync_state.json";

/// When this host last converged on a source, and which one.
///
/// Surfaced by `api-keys health` so a host whose sync has been failing for days
/// is visible as a stale timestamp rather than as a pool that merely looks
/// small.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncState {
    /// Unix seconds at which the last **successful** sync finished.
    pub last_success_at: u64,
    /// The `--from` string, verbatim. An operator-supplied command line, which
    /// argv already exposes to every process on the host; key material is read
    /// from the command's stdout and is never part of this.
    pub source: String,
    /// How many accounts the source described at that point.
    pub accounts: usize,
}

/// One account as the source describes it.
///
/// Deliberately has no derived `Debug` — [`SourceRecord::secret`] is key
/// material, and a derived one would put it in every `{:?}` of a `Vec` of these.
#[derive(Clone)]
pub struct SourceRecord {
    pub provider: String,
    pub name: String,
    pub env_name: String,
    secret: String,
}

impl SourceRecord {
    /// `provider/account` — the only form of this record that is ever printed.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}/{}", self.provider, self.name)
    }
}

impl std::fmt::Debug for SourceRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceRecord")
            .field("provider", &self.provider)
            .field("name", &self.name)
            .field("env_name", &self.env_name)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// What a sync would do (or did), by account name only.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPlan {
    /// Accounts in the source that this host does not have.
    pub added: Vec<String>,
    /// Accounts whose stored value or variable name differs from the source.
    pub updated: Vec<String>,
    /// Accounts of a source-mentioned provider that the source no longer lists.
    /// Always empty without `--prune`.
    pub removed: Vec<String>,
    /// Accounts already identical to the source — left completely untouched.
    pub unchanged: Vec<String>,
}

impl SyncPlan {
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

/// Result of [`sync`]. Carries no key material by construction.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncOutcome {
    pub source: String,
    pub dry_run: bool,
    pub pruned: bool,
    pub root: PathBuf,
    pub plan: SyncPlan,
    /// The state record written by this run, or `None` for a dry run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<SyncState>,
}

/// Inputs to one [`sync`] run.
pub struct SyncOptions<'a> {
    /// Pool root to converge — the per-repo `.loom/api-keys` or the shared one.
    pub root: &'a Path,
    /// The `--from` source string (see the module docs).
    pub source: &'a str,
    /// Remove accounts of a source-mentioned provider that the source no longer
    /// lists.
    pub prune: bool,
    /// Report the plan and write nothing.
    pub dry_run: bool,
}

/// Converge `root` on `source`.
///
/// # Errors
/// Any failure to reach, read, or parse the source — in which case **nothing has
/// been written**. Also any failure to enumerate the existing pool, because
/// "I could not read what is registered" must not be converged as "nothing is".
pub fn sync(options: &SyncOptions<'_>) -> Result<SyncOutcome, String> {
    let text = fetch(options.source)?;
    let records = parse_source(&text)?;
    let plan = plan_sync(options.root, &records, options.prune)?;
    if options.dry_run {
        return Ok(SyncOutcome {
            source: options.source.to_string(),
            dry_run: true,
            pruned: options.prune,
            root: options.root.to_path_buf(),
            plan,
            state: None,
        });
    }
    apply(options.root, &records, &plan)?;
    let state = SyncState {
        last_success_at: super::bad_marks::epoch_now(),
        source: options.source.to_string(),
        accounts: records.len(),
    };
    write_state(options.root, &state)?;
    Ok(SyncOutcome {
        source: options.source.to_string(),
        dry_run: false,
        pruned: options.prune,
        root: options.root.to_path_buf(),
        plan,
        state: Some(state),
    })
}

/// Read the last successful sync for a pool root.
///
/// # Errors
/// An existing state file that cannot be read or parsed. Callers on the *health*
/// path may degrade to `None`: unlike `.disabled` or `.bad_accounts.json`, this
/// file decides nothing about eligibility, so a damaged one must not withhold
/// accounts.
pub fn read_state(root: &Path) -> Result<Option<SyncState>, String> {
    let path = root.join(SYNC_STATE_FILE);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {} ({:?})", path.display(), e.kind())),
    };
    serde_json::from_str(&body)
        .map(Some)
        .map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

fn write_state(root: &Path, state: &SyncState) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    registry::restrict_dir(root);
    let body = serde_json::to_string_pretty(state)
        .map_err(|e| format!("cannot serialise sync state: {e}"))?;
    registry::write_secret(&root.join(SYNC_STATE_FILE), &format!("{body}\n"))
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// Fetch a source's raw text.
///
/// The scheme is split off first so an unknown backend (`ssm:`, `vault:`) fails
/// with "unsupported", never by being handed to a shell. A source with no
/// `<scheme>:` prefix at all is taken as a command, which is what an operator
/// pointing `--from` at a script means.
fn fetch(source: &str) -> Result<String, String> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(
            "--from needs a source, e.g. cmd:'aws ssm get-parameters-by-path …'".to_string()
        );
    }
    match split_scheme(trimmed) {
        Some(("cmd", command)) => run_command(command),
        Some((scheme, _)) => Err(format!(
            "unsupported source scheme {scheme:?}. Supported: cmd:<command> — a command whose \
             stdout is \"provider/account<TAB>KEY=value\" lines (a bare string with no scheme is \
             run as a command too)"
        )),
        None => run_command(trimmed),
    }
}

/// `"cmd:foo bar"` -> `Some(("cmd", "foo bar"))`; `"aws ssm get x"` -> `None`.
///
/// A scheme is a leading whitespace-free `[a-z][a-z0-9+.-]*` followed by `:`, so
/// an ordinary command line containing a colon later on (`sh -c 'a: b'`) is not
/// mistaken for one.
fn split_scheme(source: &str) -> Option<(&str, &str)> {
    let (head, rest) = source.split_once(':')?;
    let shaped = !head.is_empty()
        && head.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && head.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'+' | b'.' | b'-')
        });
    shaped.then_some((head, rest))
}

/// Run a source command and hand back its stdout.
///
/// - **stdin is `/dev/null`**: the source must not consume the operator's stdin
///   (which `api-keys add` uses for key material).
/// - **stderr is inherited, never captured.** A source that fails usually
///   explains itself there, and an operator needs to see it — but routing it
///   through Loom would put text Loom does not control into Loom's own error
///   strings and logs, which is exactly what this module promises not to do. It
///   goes straight to the terminal; Loom neither stores nor renders it.
fn run_command(command: &str) -> Result<String, String> {
    let mut invocation = shell_invocation();
    let output = invocation
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot run the source command: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "source command failed ({}); the pool was left unchanged",
            describe_status(&output.status)
        ));
    }
    // Never echoed: on a source that emits binary, the bytes ARE the secret.
    String::from_utf8(output.stdout)
        .map_err(|_| "source produced non-UTF-8 output; expected text lines".to_string())
}

#[cfg(unix)]
fn shell_invocation() -> Command {
    let mut command = Command::new("sh");
    command.arg("-c");
    command
}

#[cfg(not(unix))]
fn shell_invocation() -> Command {
    let mut command = Command::new("cmd");
    command.arg("/C");
    command
}

fn describe_status(status: &std::process::ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| "killed by a signal".to_string(), |c| format!("exit {c}"))
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a source's stdout into account records.
///
/// # Errors
/// The first malformed line, reported by **line number and shape only**: on a
/// line that does not have the expected structure, any field could be the key,
/// so nothing from the line is echoed. Once a line's structure *is* established
/// (a tab, then a conventional `KEY=`), its left-hand identifier is known not to
/// be key material and a bad provider/account slug names itself.
pub fn parse_source(text: &str) -> Result<Vec<SourceRecord>, String> {
    let mut records: Vec<SourceRecord> = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let record = parse_line(line, number)?;
        if let Some(first) = seen.insert(record.id(), number) {
            return Err(format!(
                "line {number}: duplicate entry for {} (already given on line {first})",
                record.id()
            ));
        }
        records.push(record);
    }
    Ok(records)
}

fn parse_line(line: &str, number: usize) -> Result<SourceRecord, String> {
    let Some((identifier, assignment)) = line.split_once('\t') else {
        return Err(format!(
            "line {number}: expected a TAB between <provider>/<account> and KEY=value"
        ));
    };
    let Some((env_name, value)) = assignment.trim().split_once('=') else {
        return Err(format!(
            "line {number}: the field after the TAB is not a KEY=value assignment"
        ));
    };
    let env_name = env_name.trim();
    // The strict UPPER_SNAKE_CASE form the registry stores and reads back, so a
    // synced file can never be one `list` then reports unusable. The error does
    // not echo it: in the competing reading it is the key itself.
    validate_stored_env_name(env_name).map_err(|_| {
        format!(
            "line {number}: the assigned variable must be an UPPER_SNAKE_CASE environment \
             variable NAME ([A-Z][A-Z0-9_]*)"
        )
    })?;
    // Tolerate a quoted value the way `.env` conventionally allows — the same
    // unwrapping `registry::parse_entry` does when reading a stored file back.
    let value = value.trim();
    let secret = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value)
        .trim();
    if secret.is_empty() || secret.bytes().all(|b| b == b'=') {
        return Err(format!("line {number}: the assigned value is empty"));
    }
    // Structure is established from here on, so the identifier is safe to name.
    let Some((provider, name)) = identifier.trim().split_once('/') else {
        return Err(format!(
            "line {number}: expected the identifier before the TAB to be <provider>/<account>"
        ));
    };
    let (provider, name) = (provider.trim(), name.trim());
    validate_provider(provider).map_err(|e| format!("line {number}: {e}"))?;
    validate_account(name).map_err(|e| format!("line {number}: {e}"))?;
    Ok(SourceRecord {
        provider: provider.to_string(),
        name: name.to_string(),
        env_name: env_name.to_string(),
        secret: secret.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Diff + apply
// ---------------------------------------------------------------------------

fn account_path(root: &Path, provider: &str, name: &str) -> PathBuf {
    provider_dir(root, provider).join(format!("{name}.{ACCOUNT_FILE_EXT}"))
}

/// Diff `records` against what `root` already holds.
///
/// # Errors
/// A provider directory that exists but cannot be enumerated (only reachable
/// with `prune`, which is the only part of the plan that needs the existing
/// listing): pruning against an unreadable listing could delete an account that
/// the source does list.
pub fn plan_sync(root: &Path, records: &[SourceRecord], prune: bool) -> Result<SyncPlan, String> {
    let mut plan = SyncPlan::default();
    for record in records {
        let path = account_path(root, &record.provider, &record.name);
        let stored = registry::read_credential(root, &record.provider, &record.name);
        match stored {
            Ok(credential)
                if credential.env_name == record.env_name && credential.value == record.secret =>
            {
                plan.unchanged.push(record.id());
            }
            // Present but different, or present and unreadable/malformed: either
            // way the source is the truth and the file is replaced.
            Ok(_) => plan.updated.push(record.id()),
            Err(_) if path.exists() => plan.updated.push(record.id()),
            Err(_) => plan.added.push(record.id()),
        }
    }
    if prune {
        let mut providers: Vec<&str> = records.iter().map(|r| r.provider.as_str()).collect();
        providers.sort_unstable();
        providers.dedup();
        for provider in providers {
            let existing = registry::list_provider(root, provider).map_err(|e| e.to_string())?;
            for account in existing {
                let id = format!("{provider}/{}", account.name);
                if !records.iter().any(|r| r.id() == id) {
                    plan.removed.push(id);
                }
            }
        }
    }
    plan.added.sort();
    plan.updated.sort();
    plan.removed.sort();
    plan.unchanged.sort();
    Ok(plan)
}

/// Write the plan. Adds and updates land first, so a source that renames an
/// account never passes through a window with neither name registered.
fn apply(root: &Path, records: &[SourceRecord], plan: &SyncPlan) -> Result<(), String> {
    let by_id: BTreeMap<String, &SourceRecord> = records.iter().map(|r| (r.id(), r)).collect();
    for id in plan.added.iter().chain(&plan.updated) {
        let record = by_id
            .get(id)
            .ok_or_else(|| format!("internal: planned {id} has no source record"))?;
        registry::add(
            root,
            &record.provider,
            &record.name,
            &record.env_name,
            &record.secret,
            true,
        )?;
    }
    for id in &plan.removed {
        let (provider, name) = id
            .split_once('/')
            .ok_or_else(|| format!("internal: malformed planned removal {id}"))?;
        registry::remove(root, provider, name)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
