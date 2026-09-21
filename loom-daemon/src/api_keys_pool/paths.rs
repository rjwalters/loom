//! Where the API-key account pool lives, and how a model profile maps onto a
//! provider namespace inside it (issue #8401).
//!
//! The layout deliberately mirrors [`crate::tokens_pool::paths`]: a per-repo
//! pool at `<workspace>/.loom/api-keys/`, a shared machine-level pool at
//! `~/.loom/api-keys/` (overridable, and disable-able with an explicitly empty
//! override), and per-repo-wins precedence — resolved **per provider**, see
//! [`resolve_provider_root`]. Both are **per host** — a lapsed
//! Z.ai key on one machine must never poison another, which is exactly why the
//! registry is a gitignored directory of files rather than anything in
//! `.loom/config.json` or `model-profiles.json`.
//!
//! One extra level of nesting compared with the Claude pool: accounts are
//! grouped by *provider* (`api-keys/<provider>/<account>.env`), because a
//! single host can hold subscriptions to several API-key providers at once.

use std::path::{Path, PathBuf};

/// Env override for the shared machine-level API-key pool location.
/// Non-empty names the directory (`~` expanded); explicitly empty disables the
/// shared fallback entirely.
pub const SHARED_API_KEYS_DIR_ENV: &str = "LOOM_SHARED_API_KEYS_DIR";

/// Extension of a single registered account's credential file.
pub const ACCOUNT_FILE_EXT: &str = "env";

/// Canonical per-repo pool dir `<workspace>/.loom/api-keys`.
#[must_use]
pub fn per_repo_api_keys_dir(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join("api-keys")
}

/// Shared machine-level pool dir, or `None` when disabled.
///
/// Refused under `cfg(test)` for the same reason
/// [`crate::tokens_pool::paths::shared_tokens_dir`] refuses it (#4657): the
/// env var is process-global and every `#[test]` in this crate links into one
/// multi-threaded binary, so a default fallback would let a test with no
/// per-repo pool write fixtures into the operator's live `~/.loom/api-keys`.
#[must_use]
pub fn shared_api_keys_dir() -> Option<PathBuf> {
    match std::env::var(SHARED_API_KEYS_DIR_ENV) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(expand_tilde(trimmed))
            }
        }
        #[cfg(test)]
        Err(_) => None,
        #[cfg(not(test))]
        Err(_) => dirs::home_dir().map(|home| home.join(".loom").join("api-keys")),
    }
}

fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(raw)
}

/// A pool directory that **exists but could not be read**.
///
/// This is the distinction the whole fail-closed design rests on. "The
/// directory is not there" (`ENOENT`) means this host never opted the provider
/// into pool management, and the pre-pool behaviour applies. *Every other*
/// error — `EACCES` from a uid-mismatched `0700` provider dir or bind mount,
/// `EIO`, `ESTALE`, a file where a directory belongs — means a pool may well be
/// registered and simply cannot be consulted. Reading that as "no pool" would
/// let a spawn run on the harness's ambient auth store while the operator
/// believes every account is disabled, so callers must refuse instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolReadError {
    pub path: PathBuf,
    pub kind: std::io::ErrorKind,
    detail: String,
}

impl PoolReadError {
    fn new(path: &Path, error: &std::io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            kind: error.kind(),
            detail: error.to_string(),
        }
    }
}

impl std::fmt::Display for PoolReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "API-key pool path {} exists but cannot be read ({:?}: {}). Refusing to treat an \
             unreadable pool as \"no pool\": fix its ownership/permissions (the provider \
             directory is 0700, so it must be owned by the user running the spawn), or remove it \
             if this host should not be pooled.",
            self.path.display(),
            self.kind,
            self.detail
        )
    }
}

impl std::error::Error for PoolReadError {}

/// `read_dir` with the absent/unreadable distinction made explicit: a missing
/// directory is `Ok(None)`, anything else that is not a successful read is a
/// [`PoolReadError`].
fn read_dir_tristate(dir: &Path) -> Result<Option<Vec<PathBuf>>, PoolReadError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(PoolReadError::new(dir, &e)),
    };
    let mut paths = Vec::new();
    for entry in entries {
        // A mid-iteration error is a partial listing; a partial listing that
        // happens to omit every account is the same fail-open in miniature.
        paths.push(entry.map_err(|e| PoolReadError::new(dir, &e))?.path());
    }
    Ok(Some(paths))
}

/// Account files (`*.env`) directly under one provider directory, sorted by
/// name so every caller sees a stable order. A provider directory that does
/// not exist holds no accounts; one that exists but cannot be read is an error
/// (see [`PoolReadError`]).
pub fn list_account_files(provider_dir: &Path) -> Result<Vec<PathBuf>, PoolReadError> {
    let mut files: Vec<PathBuf> = read_dir_tristate(provider_dir)?
        .unwrap_or_default()
        .into_iter()
        // Decided on the NAME alone, never on `is_file()`: a directory that is
        // listable but not searchable (`r--`) makes every `stat` fail, and an
        // `is_file()` filter would then report a registered pool as empty.
        // Anything named `<account>.env` is an account; if it turns out to be
        // unreadable, `registry::describe` reports it unusable and selection
        // fails closed on it.
        .filter(|p| {
            p.extension().and_then(|s| s.to_str()) == Some(ACCOUNT_FILE_EXT)
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|stem| !stem.is_empty() && !stem.starts_with('.'))
        })
        .collect();
    files.sort();
    Ok(files)
}

/// The pool roots that apply to `workspace`, highest precedence first: the
/// per-repo pool, then the shared machine-level pool when it is enabled.
#[must_use]
pub fn pool_roots(workspace: &Path) -> Vec<PathBuf> {
    let mut roots = vec![per_repo_api_keys_dir(workspace)];
    if let Some(shared) = shared_api_keys_dir() {
        if !roots.contains(&shared) {
            roots.push(shared);
        }
    }
    roots
}

/// Resolve the effective pool root for **one provider**: the per-repo pool
/// when it holds account files *for that provider*, else the shared
/// machine-level pool when *it* does, else the per-repo path unchanged (so an
/// error message names the repo the caller was invoked from).
///
/// Precedence is per provider on purpose. A host routinely holds a per-repo
/// pool for one provider and a machine-level pool for another (a flat-rate
/// subscription next to a metered shared key); resolving the root once for the
/// whole workspace made the first hide the second. Within one provider the
/// per-repo pool still wins outright — the two roots are never merged, so an
/// operator who registers per-repo accounts for `zai` has taken `zai` over for
/// that repo.
pub fn resolve_provider_root(workspace: &Path, provider: &str) -> Result<PathBuf, PoolReadError> {
    resolve_provider_root_in(&pool_roots(workspace), provider)
}

/// [`resolve_provider_root`] over an explicit, precedence-ordered root list.
/// Split out so the precedence rule is testable without the process-global
/// `LOOM_SHARED_API_KEYS_DIR`, which every concurrently running test reads.
pub fn resolve_provider_root_in(
    roots: &[PathBuf],
    provider: &str,
) -> Result<PathBuf, PoolReadError> {
    for root in roots {
        if !list_account_files(&provider_dir(root, provider))?.is_empty() {
            return Ok(root.clone());
        }
    }
    Ok(roots.first().cloned().unwrap_or_default())
}

/// Every provider that has a directory under any of `workspace`'s pool roots,
/// sorted and de-duplicated. Pair each with [`resolve_provider_root`].
pub fn list_workspace_providers(workspace: &Path) -> Result<Vec<String>, PoolReadError> {
    list_providers_in(&pool_roots(workspace))
}

/// [`list_workspace_providers`] over an explicit root list.
pub fn list_providers_in(roots: &[PathBuf]) -> Result<Vec<String>, PoolReadError> {
    let mut names = Vec::new();
    for root in roots {
        names.extend(list_providers(root)?);
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Directory holding `provider`'s accounts under an already-resolved pool root.
#[must_use]
pub fn provider_dir(root: &Path, provider: &str) -> PathBuf {
    root.join(provider)
}

/// Provider directories that exist under `root`, sorted. A root that does not
/// exist holds none; one that exists but cannot be read is an error.
pub fn list_providers(root: &Path) -> Result<Vec<String>, PoolReadError> {
    let mut names: Vec<String> = read_dir_tristate(root)?
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.is_dir())
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .filter(|n| validate_provider(n).is_ok())
        .collect();
    names.sort();
    Ok(names)
}

/// Reject anything that is not a plain lowercase slug — the provider and
/// account names become path components, so `..`, `/`, and friends must never
/// survive validation.
pub fn validate_provider(provider: &str) -> Result<(), String> {
    validate_slug(provider, "provider")
}

/// Account names share the provider slug rules: they are file stems.
pub fn validate_account(account: &str) -> Result<(), String> {
    validate_slug(account, "account")
}

fn validate_slug(value: &str, kind: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 64 {
        return Err(format!("{kind} name must be 1-64 characters"));
    }
    let ok = value
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && value
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{kind} name {value:?} must be lowercase [a-z0-9][a-z0-9_-]* (it becomes a path \
             component)"
        ))
    }
}

/// An environment variable name, not a value — the same shape check
/// `worker_spawn::profiles` applies to `credentialEnv`.
pub fn validate_env_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.bytes().next().is_some_and(|b| b.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        // Never echo the offending string: an operator who pasted a key where a
        // variable name belongs must not see it reflected back into a log.
        Err("credential variable must be an environment variable NAME, not a value".to_string())
    }
}

/// Whether `name` has the conventional `UPPER_SNAKE_CASE` shape of an
/// environment variable — `[A-Z][A-Z0-9_]*`.
///
/// Deliberately stricter than [`validate_env_name`]. It is the test applied to
/// text that came out of a *file* (an account file's left-hand side, or a
/// `--key-file` fragment), where the alternative reading is "this is the
/// secret itself": a hand-placed bare base64 key with `=` padding splits into
/// a plausible-looking name and a value of `=`. Mixed-case text is therefore
/// treated as key material, never as a variable name, and is never echoed.
#[must_use]
pub fn is_conventional_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && name.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
}

/// [`is_conventional_env_name`] as a validator for a variable name that will
/// be **stored in an account file**. The error never echoes the input.
pub fn validate_stored_env_name(name: &str) -> Result<(), String> {
    if is_conventional_env_name(name) {
        Ok(())
    } else {
        Err("credential variable must be an UPPER_SNAKE_CASE environment variable NAME \
             ([A-Z][A-Z0-9_]*), not a value"
            .to_string())
    }
}

/// Derive a pool provider namespace from a profile's `credentialEnv`.
///
/// `ZAI_API_KEY` -> `zai`, `OPENAI_API_KEY` -> `openai`, `GROQ_TOKEN` ->
/// `groq`. The derivation is deliberately from the profile's *source* variable
/// (`credentialEnv`), never from its harness-facing `providers` entry: the same
/// Z.ai subscription is `zai` to Pi and `zai-coding-plan` to OpenCode, and one
/// account must not have to be registered twice. A profile can always override
/// the result with an explicit `credentialPool`.
#[must_use]
pub fn provider_from_credential_env(credential_env: &str) -> Option<String> {
    let lowered = credential_env.trim().to_ascii_lowercase();
    let stem = [
        "_api_key",
        "_api_token",
        "_apikey",
        "_key",
        "_token",
        "_secret",
    ]
    .iter()
    .find_map(|suffix| lowered.strip_suffix(suffix))
    .unwrap_or(lowered.as_str());
    let slug = stem.trim_matches('_').to_string();
    validate_provider(&slug).ok().map(|()| slug)
}

/// The conventional environment-variable name for `provider`, used as the
/// default when `api-keys add` is not given an explicit `--env-var`. Inverse of
/// [`provider_from_credential_env`] for the common `<PROVIDER>_API_KEY` shape.
#[must_use]
pub fn default_env_name(provider: &str) -> String {
    format!("{}_API_KEY", provider.to_ascii_uppercase().replace('-', "_"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;

    fn write_account(root: &Path, provider: &str, name: &str) {
        let dir = provider_dir(root, provider);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.env")), "ZAI_API_KEY=fake\n").unwrap();
    }

    #[test]
    fn per_repo_dir_is_dot_loom_api_keys() {
        assert_eq!(
            per_repo_api_keys_dir(Path::new("/tmp/example-repo")),
            PathBuf::from("/tmp/example-repo/.loom/api-keys")
        );
    }

    /// `chmod` a directory for the duration of a test and restore it on drop,
    /// so `tempfile` can always clean up even when an assertion fails.
    #[cfg(unix)]
    struct ModeGuard(PathBuf);

    #[cfg(unix)]
    impl ModeGuard {
        fn set(path: &Path, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            Self(path.to_path_buf())
        }
    }

    #[cfg(unix)]
    impl Drop for ModeGuard {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }

    #[test]
    fn account_files_ignore_dotfiles_and_other_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = provider_dir(tmp.path(), "zai");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".disabled"), "x").unwrap();
        fs::write(dir.join(".alpha.env.tmp-1-2"), "x").unwrap();
        fs::write(dir.join("notes.txt"), "x").unwrap();
        fs::write(dir.join("beta.env"), "K=v").unwrap();
        fs::write(dir.join("alpha.env"), "K=v").unwrap();
        let files = list_account_files(&dir).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].file_stem().unwrap(), "alpha");
        assert_eq!(files[1].file_stem().unwrap(), "beta");
    }

    #[test]
    fn a_missing_directory_is_an_empty_pool_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(list_account_files(&tmp.path().join("absent")).unwrap(), Vec::<PathBuf>::new());
        assert_eq!(list_providers(&tmp.path().join("absent")).unwrap(), Vec::<String>::new());
    }

    /// Judge finding 1 (#8428): `EACCES` must not be indistinguishable from
    /// `ENOENT`. Before the fix this returned an empty list.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_provider_dir_is_an_error_not_an_empty_pool() {
        let tmp = tempfile::tempdir().unwrap();
        write_account(tmp.path(), "zai", "alpha");
        let dir = provider_dir(tmp.path(), "zai");
        let _guard = ModeGuard::set(&dir, 0o000);
        if fs::read_dir(&dir).is_ok() {
            return; // running as root: permission bits do not apply
        }
        let err = list_account_files(&dir).unwrap_err();
        assert_eq!(err.kind, std::io::ErrorKind::PermissionDenied);
        assert_eq!(err.path, dir);
        let text = err.to_string();
        assert!(text.contains(&dir.display().to_string()), "{text}");
        assert!(text.contains("PermissionDenied"), "{text}");
    }

    /// A directory that can be listed but not searched makes every `stat`
    /// fail; the accounts must still be reported, not filtered away.
    #[cfg(unix)]
    #[test]
    fn a_listable_but_unsearchable_dir_still_reports_its_accounts() {
        let tmp = tempfile::tempdir().unwrap();
        write_account(tmp.path(), "zai", "alpha");
        let dir = provider_dir(tmp.path(), "zai");
        let _guard = ModeGuard::set(&dir, 0o400);
        if fs::metadata(dir.join("alpha.env")).is_ok() {
            return; // running as root
        }
        assert_eq!(list_account_files(&dir).unwrap().len(), 1);
    }

    #[test]
    fn a_file_where_a_provider_dir_belongs_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("zai"), "not a directory").unwrap();
        assert!(list_account_files(&tmp.path().join("zai")).is_err());
    }

    #[test]
    fn resolve_prefers_per_repo_then_shared() {
        let repo = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        let roots = vec![
            per_repo_api_keys_dir(repo.path()),
            shared.path().to_path_buf(),
        ];
        write_account(shared.path(), "zai", "shared-one");
        assert_eq!(resolve_provider_root_in(&roots, "zai").unwrap(), shared.path());
        write_account(&roots[0], "zai", "local-one");
        assert_eq!(resolve_provider_root_in(&roots, "zai").unwrap(), roots[0]);
    }

    /// Judge finding 3 (#8428): a per-repo pool for one provider must not hide
    /// the shared pool for a different provider. Before the fix the root was
    /// resolved once per workspace, so `zai` resolved to the per-repo root
    /// (which holds no `zai` accounts) and read as unpooled.
    #[test]
    fn root_resolution_is_per_provider() {
        let repo = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        let roots = vec![
            per_repo_api_keys_dir(repo.path()),
            shared.path().to_path_buf(),
        ];
        write_account(&roots[0], "loomtest", "local-one");
        write_account(shared.path(), "zai", "shared-one");
        assert_eq!(resolve_provider_root_in(&roots, "loomtest").unwrap(), roots[0]);
        assert_eq!(resolve_provider_root_in(&roots, "zai").unwrap(), shared.path());
        assert_eq!(
            list_providers_in(&roots).unwrap(),
            vec!["loomtest".to_string(), "zai".to_string()]
        );
        // A provider registered nowhere names the per-repo path.
        assert_eq!(resolve_provider_root_in(&roots, "openai").unwrap(), roots[0]);
    }

    /// The shared root is consulted through `LOOM_SHARED_API_KEYS_DIR`. Points
    /// at an EMPTY directory on purpose: the variable is process-global, and a
    /// populated one would leak accounts into concurrently running tests.
    #[test]
    #[serial]
    fn the_shared_root_comes_from_the_env_override() {
        let repo = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        std::env::set_var(SHARED_API_KEYS_DIR_ENV, shared.path().to_str().unwrap());
        assert_eq!(
            pool_roots(repo.path()),
            vec![
                per_repo_api_keys_dir(repo.path()),
                shared.path().to_path_buf()
            ]
        );
        std::env::remove_var(SHARED_API_KEYS_DIR_ENV);
    }

    #[test]
    #[serial]
    fn empty_shared_override_disables_the_fallback() {
        std::env::set_var(SHARED_API_KEYS_DIR_ENV, "");
        assert_eq!(shared_api_keys_dir(), None);
        let repo = tempfile::tempdir().unwrap();
        assert_eq!(pool_roots(repo.path()), vec![per_repo_api_keys_dir(repo.path())]);
        assert_eq!(
            resolve_provider_root(repo.path(), "zai").unwrap(),
            per_repo_api_keys_dir(repo.path())
        );
        std::env::remove_var(SHARED_API_KEYS_DIR_ENV);
    }

    #[test]
    fn conventional_env_names_are_upper_snake_case_only() {
        for good in ["ZAI_API_KEY", "K", "OPENAI_API_KEY", "A1_B2"] {
            assert!(is_conventional_env_name(good), "{good:?}");
        }
        // A bare base64 key split at its `=` padding must not read as a name.
        for bad in [
            "RkFLRUtFWW1hdGVyaWFsMDAwNQ",
            "zai_api_key",
            "1ABC",
            "_ABC",
            "",
            "A-B",
        ] {
            assert!(!is_conventional_env_name(bad), "{bad:?}");
        }
        let err = validate_stored_env_name("RkFLRUtFWW1hdGVyaWFsMDAwNQ").unwrap_err();
        assert!(!err.contains("RkFLRUtFWW1hdGVyaWFsMDAwNQ"), "{err}");
    }

    #[test]
    fn slug_validation_rejects_path_traversal() {
        for bad in ["..", "a/b", "../etc", "", "Zai", "-lead", "_lead"] {
            assert!(validate_provider(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in ["zai", "zai-coding", "openai", "z3"] {
            assert!(validate_provider(good).is_ok(), "{good:?} must be accepted");
        }
    }

    #[test]
    fn env_name_validation_never_echoes_the_offending_value() {
        let err = validate_env_name("sk-live-should-not-appear").unwrap_err();
        assert!(!err.contains("sk-live-should-not-appear"), "{err}");
        assert!(validate_env_name("ZAI_API_KEY").is_ok());
        assert!(validate_env_name("1BAD").is_err());
    }

    #[test]
    fn provider_derivation_strips_conventional_suffixes() {
        assert_eq!(provider_from_credential_env("ZAI_API_KEY").as_deref(), Some("zai"));
        assert_eq!(provider_from_credential_env("OPENAI_API_KEY").as_deref(), Some("openai"));
        assert_eq!(provider_from_credential_env("GROQ_TOKEN").as_deref(), Some("groq"));
        assert_eq!(provider_from_credential_env("ZHIPU_API_KEY").as_deref(), Some("zhipu"));
        // No recognizable stem left.
        assert_eq!(provider_from_credential_env("_API_KEY"), None);
    }

    #[test]
    fn default_env_name_round_trips_the_derivation() {
        assert_eq!(default_env_name("zai"), "ZAI_API_KEY");
        assert_eq!(default_env_name("zai-coding"), "ZAI_CODING_API_KEY");
        assert_eq!(
            provider_from_credential_env(&default_env_name("openai")).as_deref(),
            Some("openai")
        );
    }
}
