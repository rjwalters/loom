//! The single resolver for the llm-monitor (formerly claude-monitor) data
//! directory (issue #8849).
//!
//! claude-monitor was renamed **llm-monitor** in 2.0 and moved its data dir
//! from `~/.claude-monitor` to `~/.llm-monitor` (leaving a compatibility
//! symlink at the old path). Every Loom consumer of that directory —
//! `ranking.json` ([`super::monitor`]), `accounts.env`
//! ([`super::bootstrap`]), and `usage.db` ([`super::monitor_db`],
//! [`crate::limit_calibration`]) — resolves it through [`claude_monitor_dir`]
//! so they always agree on one directory, and Loom keeps working on a 1.x
//! host that only has `~/.claude-monitor` (no symlink).
//!
//! Lives in its own module rather than `monitor.rs` because that file is
//! frozen by the file-size ratchet (`scripts/check-file-size-budget.sh`).

use std::path::{Path, PathBuf};

/// Preferred override for the monitor data directory (llm-monitor 2.0).
/// Wins over [`CLAUDE_MONITOR_DIR_VAR`].
pub const LLM_MONITOR_DIR_VAR: &str = "LOOM_LLM_MONITOR_DIR";
/// Deprecated override (claude-monitor 1.x name), still honored when
/// [`LLM_MONITOR_DIR_VAR`] is unset or blank.
pub const CLAUDE_MONITOR_DIR_VAR: &str = "LOOM_CLAUDE_MONITOR_DIR";
/// llm-monitor 2.0's data directory; selected when it exists as a directory.
pub const DEFAULT_LLM_MONITOR_DIR: &str = "~/.llm-monitor";
/// claude-monitor 1.x's data directory; the fallback for a host not yet
/// upgraded (and the answer when neither default exists).
pub const DEFAULT_CLAUDE_MONITOR_DIR: &str = "~/.claude-monitor";

/// Resolve the llm-monitor data directory. Precedence:
///
/// 1. nonblank `$LOOM_LLM_MONITOR_DIR` (tilde expanded),
/// 2. nonblank `$LOOM_CLAUDE_MONITOR_DIR` (deprecated, tilde expanded),
/// 3. `~/.llm-monitor` when it exists **as a directory**,
/// 4. `~/.claude-monitor` (a 1.x host, or when neither default exists).
///
/// An explicit override is authoritative even when the path does not exist —
/// it never falls through to another source. Blank / whitespace-only values
/// count as unset. Every consumer joins its own file onto this one directory,
/// so a selected directory missing one file leaves that consumer on its
/// existing "unavailable" path rather than mixing files from two directories.
///
/// The `claude_` name is kept for API compatibility.
#[must_use]
pub fn claude_monitor_dir() -> PathBuf {
    let llm = std::env::var(LLM_MONITOR_DIR_VAR).ok();
    let claude = std::env::var(CLAUDE_MONITOR_DIR_VAR).ok();
    resolve_monitor_dir(llm.as_deref(), claude.as_deref(), dirs::home_dir().as_deref())
}

/// Pure core of [`claude_monitor_dir`]: env values and `$HOME` are injected
/// so the precedence matrix is testable without touching process state.
fn resolve_monitor_dir(
    llm_override: Option<&str>,
    claude_override: Option<&str>,
    home: Option<&Path>,
) -> PathBuf {
    for raw in [llm_override, claude_override].into_iter().flatten() {
        if !raw.trim().is_empty() {
            return expand_tilde(raw, home);
        }
    }
    let new_default = expand_tilde(DEFAULT_LLM_MONITOR_DIR, home);
    if new_default.is_dir() {
        return new_default;
    }
    expand_tilde(DEFAULT_CLAUDE_MONITOR_DIR, home)
}

fn expand_tilde(raw: &str, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    } else if raw == "~" {
        if let Some(home) = home {
            return home.to_path_buf();
        }
    }
    PathBuf::from(raw)
}

/// Test-only environment isolation for the two override variables.
///
/// Existing fixtures historically set only `LOOM_CLAUDE_MONITOR_DIR`; with
/// `LOOM_LLM_MONITOR_DIR` now taking precedence, an inherited value of the
/// new variable would silently beat the fixture. Every in-process test that
/// points the resolver at a fixture goes through this guard, which captures
/// both variables, sets exactly one (or neither), and restores both on drop.
/// Callers must still be `#[serial]` — the guard restores, it does not lock.
#[cfg(test)]
pub(crate) mod test_env {
    use std::ffi::OsString;
    use std::path::Path;

    use super::{CLAUDE_MONITOR_DIR_VAR, LLM_MONITOR_DIR_VAR};

    pub(crate) struct MonitorDirEnvGuard {
        llm: Option<OsString>,
        claude: Option<OsString>,
    }

    impl MonitorDirEnvGuard {
        fn capture() -> Self {
            let guard = Self {
                llm: std::env::var_os(LLM_MONITOR_DIR_VAR),
                claude: std::env::var_os(CLAUDE_MONITOR_DIR_VAR),
            };
            std::env::remove_var(LLM_MONITOR_DIR_VAR);
            std::env::remove_var(CLAUDE_MONITOR_DIR_VAR);
            guard
        }

        /// Point the resolver at `dir` through the deprecated legacy
        /// variable only, with the new variable cleared.
        pub(crate) fn legacy(dir: impl AsRef<Path>) -> Self {
            let guard = Self::capture();
            std::env::set_var(CLAUDE_MONITOR_DIR_VAR, dir.as_ref());
            guard
        }

        /// Point the resolver at `dir` through the new variable only.
        pub(crate) fn llm(dir: impl AsRef<Path>) -> Self {
            let guard = Self::capture();
            std::env::set_var(LLM_MONITOR_DIR_VAR, dir.as_ref());
            guard
        }
    }

    impl Drop for MonitorDirEnvGuard {
        fn drop(&mut self) {
            for (var, saved) in [
                (LLM_MONITOR_DIR_VAR, &self.llm),
                (CLAUDE_MONITOR_DIR_VAR, &self.claude),
            ] {
                match saved {
                    Some(value) => std::env::set_var(var, value),
                    None => std::env::remove_var(var),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_env::MonitorDirEnvGuard;
    use super::*;
    use serial_test::serial;
    use std::fs;

    fn home_with(llm: bool, claude: bool) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        if llm {
            fs::create_dir(home.path().join(".llm-monitor")).unwrap();
        }
        if claude {
            fs::create_dir(home.path().join(".claude-monitor")).unwrap();
        }
        home
    }

    #[test]
    fn llm_override_beats_legacy_override() {
        let home = home_with(true, true);
        let got = resolve_monitor_dir(Some("/x/llm"), Some("/x/claude"), Some(home.path()));
        assert_eq!(got, PathBuf::from("/x/llm"));
    }

    #[test]
    fn llm_override_alone_and_legacy_override_alone() {
        let home = home_with(true, true);
        assert_eq!(
            resolve_monitor_dir(Some("/x/llm"), None, Some(home.path())),
            PathBuf::from("/x/llm")
        );
        assert_eq!(
            resolve_monitor_dir(None, Some("/x/claude"), Some(home.path())),
            PathBuf::from("/x/claude"),
            "the deprecated override still wins over both default directories"
        );
    }

    #[test]
    fn blank_overrides_are_ignored() {
        let home = home_with(false, false);
        // Blank new override falls through to the legacy override.
        assert_eq!(
            resolve_monitor_dir(Some("  "), Some("/x/claude"), Some(home.path())),
            PathBuf::from("/x/claude")
        );
        // Both blank: default resolution.
        assert_eq!(
            resolve_monitor_dir(Some(""), Some("\t"), Some(home.path())),
            home.path().join(".claude-monitor")
        );
    }

    #[test]
    fn overrides_expand_tilde() {
        let home = home_with(false, false);
        assert_eq!(
            resolve_monitor_dir(Some("~/mon"), None, Some(home.path())),
            home.path().join("mon")
        );
        assert_eq!(
            resolve_monitor_dir(None, Some("~/legacy"), Some(home.path())),
            home.path().join("legacy")
        );
        assert_eq!(resolve_monitor_dir(Some("~"), None, Some(home.path())), home.path());
    }

    #[test]
    fn nonexistent_explicit_override_is_authoritative() {
        // Both defaults exist, but an explicit override naming an absent path
        // must not fall through to either of them.
        let home = home_with(true, true);
        let absent = home.path().join("does-not-exist");
        let got = resolve_monitor_dir(Some(absent.to_str().unwrap()), None, Some(home.path()));
        assert_eq!(got, absent);
        let got = resolve_monitor_dir(None, Some(absent.to_str().unwrap()), Some(home.path()));
        assert_eq!(got, absent);
    }

    #[test]
    fn both_defaults_prefer_llm_monitor() {
        let home = home_with(true, true);
        assert_eq!(
            resolve_monitor_dir(None, None, Some(home.path())),
            home.path().join(".llm-monitor")
        );
    }

    #[test]
    fn new_default_only() {
        let home = home_with(true, false);
        assert_eq!(
            resolve_monitor_dir(None, None, Some(home.path())),
            home.path().join(".llm-monitor")
        );
    }

    #[test]
    fn legacy_only_host_without_symlink_uses_claude_monitor() {
        // A 1.x host: only a real `~/.claude-monitor` directory, no
        // `~/.llm-monitor` and no compatibility symlink.
        let home = home_with(false, true);
        assert!(!home.path().join(".llm-monitor").exists());
        assert_eq!(
            resolve_monitor_dir(None, None, Some(home.path())),
            home.path().join(".claude-monitor")
        );
    }

    #[test]
    fn neither_default_returns_legacy_path() {
        let home = home_with(false, false);
        assert_eq!(
            resolve_monitor_dir(None, None, Some(home.path())),
            home.path().join(".claude-monitor")
        );
    }

    #[test]
    fn regular_file_at_new_default_is_not_selected() {
        let home = home_with(false, true);
        fs::write(home.path().join(".llm-monitor"), "not a dir").unwrap();
        assert_eq!(
            resolve_monitor_dir(None, None, Some(home.path())),
            home.path().join(".claude-monitor")
        );
    }

    #[test]
    #[serial]
    fn env_reader_honors_precedence_and_guard_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let before_llm = std::env::var_os(LLM_MONITOR_DIR_VAR);
        let before_claude = std::env::var_os(CLAUDE_MONITOR_DIR_VAR);
        {
            let _g = MonitorDirEnvGuard::legacy(tmp.path().join("legacy"));
            assert_eq!(claude_monitor_dir(), tmp.path().join("legacy"));
        }
        {
            let _g = MonitorDirEnvGuard::llm(tmp.path().join("new"));
            std::env::set_var(CLAUDE_MONITOR_DIR_VAR, tmp.path().join("legacy"));
            assert_eq!(claude_monitor_dir(), tmp.path().join("new"));
        }
        assert_eq!(std::env::var_os(LLM_MONITOR_DIR_VAR), before_llm);
        assert_eq!(std::env::var_os(CLAUDE_MONITOR_DIR_VAR), before_claude);
    }

    /// Every default consumer — ranking, bootstrap accounts, credential-import
    /// DB, calibration DB — lands on the ONE selected directory, and a file
    /// missing there is reported unavailable rather than borrowed from the
    /// other (fully populated) monitor directory. Explicit import options keep
    /// their precedence.
    #[test]
    #[serial]
    fn default_consumers_share_one_dir_without_per_file_fallback() {
        use super::super::bootstrap::{bootstrap_tokens, BootstrapOptions, HOME_ACCOUNTS_ENV_VAR};
        use super::super::monitor::build_monitor_accounts;
        use super::super::monitor_db::{
            import_from_monitor, monitor_db_path, ImportOptions, MonitorImportError,
        };

        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("legacy");
        let new = tmp.path().join("new");
        let tokens_dir = tmp.path().join("pool");
        // Legacy dir: every file present (fresh ranking, accounts, a db file).
        fs::create_dir_all(&legacy).unwrap();
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let ranking = serde_json::json!({"schema": 1, "generated_at": now, "accounts": [
            {"email": "legacy@example.com", "status": "available", "utilization": {"7d": 0.1}}]});
        fs::write(legacy.join("ranking.json"), ranking.to_string()).unwrap();
        fs::write(
            legacy.join("accounts.env"),
            "ACCOUNT_EMAIL_1=legacy@example.com\nACCOUNT_KEY_1=sk-ant-oat01-synthetic-l\n",
        )
        .unwrap();
        fs::write(legacy.join("usage.db"), "synthetic").unwrap();
        // New dir: accounts.env only.
        fs::create_dir_all(&new).unwrap();
        fs::write(
            new.join("accounts.env"),
            "ACCOUNT_EMAIL_1=new@example.com\nACCOUNT_KEY_1=sk-ant-oat01-synthetic-n\n",
        )
        .unwrap();
        fs::create_dir_all(&tokens_dir).unwrap();
        fs::write(
            tokens_dir.join("index.json"),
            r#"{"version":2,"accounts":[{"name":"legacy","email":"legacy@example.com"}]}"#,
        )
        .unwrap();

        let saved_home_env = std::env::var_os(HOME_ACCOUNTS_ENV_VAR);
        std::env::set_var(HOME_ACCOUNTS_ENV_VAR, "");
        let _g = MonitorDirEnvGuard::llm(&new);
        std::env::set_var(CLAUDE_MONITOR_DIR_VAR, &legacy); // lower precedence

        assert_eq!(claude_monitor_dir(), new);
        assert_eq!(monitor_db_path(None), new.join("usage.db"));
        assert_eq!(crate::limit_calibration::default_monitor_db_path(), new.join("usage.db"));
        // Ranking: no ranking.json in the selected dir → unavailable, even
        // though the legacy dir's ranking is fresh and usable.
        assert!(build_monitor_accounts(&tokens_dir, None, None).is_none());
        assert!(build_monitor_accounts(&tokens_dir, Some(&legacy), None).is_some());
        // Bootstrap: the monitor source is the selected dir's accounts.env.
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".loom")).unwrap();
        let result = bootstrap_tokens(&BootstrapOptions {
            repo_root: repo.path().to_path_buf(),
            env_path: None,
            home_env_path: Some(None),
            force: false,
            dry_run: true,
            tokens_dir: tokens_dir.clone(),
        })
        .unwrap();
        assert_eq!(result.monitor_env, Some(new.join("accounts.env")));
        let emails: Vec<&str> = result.effective.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(emails, vec!["new@example.com"]);
        // Credential import: the default DB is the selected dir's (absent)
        // usage.db — never the legacy one — and the hint names the new var.
        let import = |db_path: Option<&Path>, monitor_dir: Option<&Path>| {
            import_from_monitor(&ImportOptions {
                tokens_dir: &tokens_dir,
                db_path,
                monitor_dir,
                force: false,
                dry_run: true,
                prune: false,
            })
        };
        match import(None, None) {
            Err(MonitorImportError::DbUnavailable(msg)) => {
                assert!(msg.contains(&new.join("usage.db").display().to_string()), "{msg}");
                assert!(msg.contains("LOOM_LLM_MONITOR_DIR"), "{msg}");
            }
            other => panic!("expected DbUnavailable for the new dir, got {other:?}"),
        }
        // Explicit options keep their precedence over the resolver.
        let explicit_db = tmp.path().join("explicit.db");
        match import(Some(&explicit_db), Some(&legacy)) {
            Err(MonitorImportError::DbUnavailable(msg)) => {
                assert!(msg.contains(&explicit_db.display().to_string()), "{msg}");
            }
            other => panic!("expected DbUnavailable for --db, got {other:?}"),
        }
        assert_eq!(monitor_db_path(Some(&legacy)), legacy.join("usage.db"));

        match saved_home_env {
            Some(v) => std::env::set_var(HOME_ACCOUNTS_ENV_VAR, v),
            None => std::env::remove_var(HOME_ACCOUNTS_ENV_VAR),
        }
    }
}
