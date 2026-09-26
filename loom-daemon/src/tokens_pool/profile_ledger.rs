//! The per-profile provisioning **ledger** (issue #8672).
//!
//! # What problem it solves
//!
//! Provisioning re-runs — on `accounts add`/`import`, on an explicit
//! `accounts provision`, and once per pooled profile at daemon start. Without
//! a record of what it last wrote it can only choose between two wrong
//! behaviours: overwrite unconditionally (destroying an operator's local edit
//! inside a pooled profile, forever, silently) or never overwrite (so a change
//! in the default profile never reaches the pool).
//!
//! The ledger makes the third, correct behaviour possible: **overwrite only
//! while the profile still holds exactly what Loom last wrote there.** The
//! moment a human edits a provisioned file or key, the recorded fingerprint
//! stops matching and that surface becomes theirs permanently — a user edit
//! inside a profile wins forever.
//!
//! # Shape
//!
//! `<profile>/.loom-profile.json`, Loom-owned, non-secret by construction: it
//! stores a SHA-256 for each copied file, a canonical JSON rendering of each
//! merged key's value, and each symlink's target. No credential is ever read,
//! so none can ever be recorded — the credential file is on the provider's
//! `never_share` list and the provisioner refuses to open it.
//!
//! There is deliberately **no timestamp field**. A ledger that changed on
//! every run would make the "second run is a no-op" property unobservable by
//! mtime, which is exactly how idempotency is verified.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The ledger's file name inside a pooled profile.
pub const LEDGER_FILE: &str = ".loom-profile.json";

/// Bumped only when an older ledger can no longer be interpreted. A ledger
/// whose version this build does not recognise is treated as absent, which
/// degrades to "never overwrite anything that already exists" — the safe
/// direction.
pub const LEDGER_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLedger {
    pub schema_version: u32,
    /// Provider whose sharing table produced this profile's content.
    #[serde(default)]
    pub provider: String,
    /// The default profile these surfaces were provisioned from.
    #[serde(default)]
    pub source: String,
    /// Surface path -> the symlink target Loom created.
    #[serde(default)]
    pub links: BTreeMap<String, String>,
    /// Surface path -> SHA-256 (hex) of the bytes Loom wrote.
    #[serde(default)]
    pub copied: BTreeMap<String, String>,
    /// Settings file -> dotted key -> canonical JSON of the value Loom wrote.
    #[serde(default)]
    pub merged: BTreeMap<String, BTreeMap<String, String>>,
}

impl ProfileLedger {
    #[must_use]
    pub fn new(provider: &str, source: &Path) -> Self {
        Self {
            schema_version: LEDGER_SCHEMA_VERSION,
            provider: provider.to_string(),
            source: source.display().to_string(),
            ..Self::default()
        }
    }

    /// Read a profile's ledger, or a fresh empty one.
    ///
    /// An unreadable, malformed, or future-versioned ledger is *not* an error:
    /// it yields the empty ledger, under which every pre-existing file is
    /// treated as operator-owned and left alone.
    #[must_use]
    pub fn load(profile: &Path, provider: &str, source: &Path) -> Self {
        let path = ledger_path(profile);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::new(provider, source);
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(ledger) if ledger.schema_version == LEDGER_SCHEMA_VERSION => ledger,
            _ => Self::new(provider, source),
        }
    }

    /// Persist the ledger — but only when its bytes would actually change.
    ///
    /// Returns `true` when a write happened. The no-write-when-unchanged rule
    /// is what lets an idempotency test assert on the ledger's own mtime.
    pub fn save_if_changed(&self, profile: &Path) -> Result<bool> {
        let path = ledger_path(profile);
        let mut rendered = serde_json::to_string_pretty(self)
            .context("failed to serialize the profile provisioning ledger")?;
        rendered.push('\n');
        if let Ok(existing) = std::fs::read_to_string(&path) {
            if existing == rendered {
                return Ok(false);
            }
        }
        write_private(&path, rendered.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(true)
    }

    /// The canonical value Loom last wrote at `dotted` in `file`, if any.
    #[must_use]
    pub fn merged_value(&self, file: &str, dotted: &str) -> Option<&str> {
        self.merged.get(file)?.get(dotted).map(String::as_str)
    }

    pub fn record_merged(&mut self, file: &str, dotted: &str, canonical: String) {
        self.merged
            .entry(file.to_string())
            .or_default()
            .insert(dotted.to_string(), canonical);
    }
}

#[must_use]
pub fn ledger_path(profile: &Path) -> PathBuf {
    profile.join(LEDGER_FILE)
}

/// Atomic, owner-only write: a temp file beside the target (same filesystem,
/// so the rename is atomic) created with mode `0600` before any byte is
/// written.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let staged = parent.join(format!(
        ".{}.loom-tmp-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("profile"),
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&staged)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result?;
    if let Err(error) = std::fs::rename(&staged, path) {
        let _ = std::fs::remove_file(&staged);
        return Err(error.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_ledger_loads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = ProfileLedger::load(tmp.path(), "codex", Path::new("/src"));
        assert_eq!(ledger.schema_version, LEDGER_SCHEMA_VERSION);
        assert!(ledger.copied.is_empty());
    }

    #[test]
    fn a_malformed_ledger_degrades_to_empty_rather_than_erroring() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(ledger_path(tmp.path()), "{not json").unwrap();
        let ledger = ProfileLedger::load(tmp.path(), "codex", Path::new("/src"));
        assert!(ledger.copied.is_empty());
    }

    #[test]
    fn a_future_schema_version_degrades_to_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut future = ProfileLedger::new("codex", Path::new("/src"));
        future.schema_version = LEDGER_SCHEMA_VERSION + 99;
        future.copied.insert("AGENTS.md".into(), "deadbeef".into());
        std::fs::write(ledger_path(tmp.path()), serde_json::to_string(&future).unwrap()).unwrap();
        assert!(ProfileLedger::load(tmp.path(), "codex", Path::new("/src"))
            .copied
            .is_empty());
    }

    #[test]
    fn save_is_a_no_op_when_nothing_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = ProfileLedger::new("codex", Path::new("/src"));
        ledger.copied.insert("AGENTS.md".into(), "abc".into());
        assert!(ledger.save_if_changed(tmp.path()).unwrap());
        assert!(!ledger.save_if_changed(tmp.path()).unwrap());
        ledger.copied.insert("AGENTS.md".into(), "def".into());
        assert!(ledger.save_if_changed(tmp.path()).unwrap());
    }

    #[test]
    fn the_ledger_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = ProfileLedger::new("codex", Path::new("/src"));
        ledger.links.insert("prompts".into(), "/src/prompts".into());
        ledger.record_merged("config.toml", "model", "\"gpt-5\"".into());
        ledger.save_if_changed(tmp.path()).unwrap();
        let reloaded = ProfileLedger::load(tmp.path(), "codex", Path::new("/src"));
        assert_eq!(reloaded, ledger);
        assert_eq!(reloaded.merged_value("config.toml", "model"), Some("\"gpt-5\""));
        assert_eq!(reloaded.merged_value("config.toml", "nope"), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_ledger_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        ProfileLedger::new("codex", Path::new("/src"))
            .save_if_changed(tmp.path())
            .unwrap();
        let mode = std::fs::metadata(ledger_path(tmp.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
