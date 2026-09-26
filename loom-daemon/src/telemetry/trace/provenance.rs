//! Which Loom produced a span — trace identity policy
//! (`.loom/docs/trace-identity.md`).
//!
//! Every span records the version and full git SHA of the binary that created
//! it ([`stamp`]). An execution's root span also records the installed Loom
//! surface it ran against and a digest of the exact prompt files
//! ([`workspace`]): the sweep child runs the workspace's installed
//! `.claude/commands/loom/` and `.loom/roles/`, not the daemon binary, so the
//! binary's SHA alone does not say which prompts ran.

use super::TraceAttributes;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// `CARGO_PKG_VERSION` of the binary that created the span.
pub const DAEMON_VERSION: &str = "loom.daemon.version";
/// Full git SHA the binary was built from (`unknown` for a tarball build).
pub const DAEMON_REVISION: &str = "loom.daemon.revision";
/// `loom_version` from the workspace's `.loom/install-metadata.json`.
pub const INSTALL_VERSION: &str = "loom.install.version";
/// `loom_commit` from the workspace's `.loom/install-metadata.json`.
pub const INSTALL_REVISION: &str = "loom.install.revision";
/// `sha256:<hex>` over every installed prompt file, path and content.
pub const PROMPTS_DIGEST: &str = "loom.prompts.digest";

/// The allowlisted provenance attribute keys.
pub const KEYS: &[&str] = &[
    DAEMON_VERSION,
    DAEMON_REVISION,
    INSTALL_VERSION,
    INSTALL_REVISION,
    PROMPTS_DIGEST,
];

/// Workspace-relative directories whose files are the prompts a sweep runs.
const PROMPT_ROOTS: &[&str] = &[".claude/commands/loom", ".loom/roles"];

/// Record the creating binary on `attributes`. Never overwrites: a span
/// restored from a queue keeps the binary that created it, not the one
/// exporting it.
pub fn stamp(attributes: &mut TraceAttributes) {
    attributes
        .entry(DAEMON_VERSION.into())
        .or_insert_with(|| env!("CARGO_PKG_VERSION").into());
    attributes
        .entry(DAEMON_REVISION.into())
        .or_insert_with(|| env!("LOOM_DAEMON_GIT_SHA").into());
}

/// The installed Loom surface and prompt digest for `root`. Best-effort: an
/// unreadable metadata file or an empty prompt tree omits that attribute
/// rather than inventing a value.
#[must_use]
pub fn workspace(root: &Path) -> TraceAttributes {
    let mut attributes = TraceAttributes::new();
    let metadata = std::fs::read(root.join(".loom/install-metadata.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    for (key, field) in [
        (INSTALL_VERSION, "loom_version"),
        (INSTALL_REVISION, "loom_commit"),
    ] {
        if let Some(value) = metadata
            .as_ref()
            .and_then(|m| m.get(field))
            .and_then(serde_json::Value::as_str)
        {
            attributes.insert(key.into(), value.into());
        }
    }
    if let Some(digest) = prompts_digest(root) {
        attributes.insert(PROMPTS_DIGEST.into(), digest);
    }
    attributes
}

/// `sha256:<hex>` over each prompt file's workspace-relative path and bytes,
/// in sorted path order. `None` when no prompt file exists.
#[must_use]
pub fn prompts_digest(root: &Path) -> Option<String> {
    let mut files = Vec::new();
    for prompt_root in PROMPT_ROOTS {
        collect_files(&root.join(prompt_root), &mut files);
    }
    let mut files: Vec<(String, PathBuf)> = files
        .into_iter()
        .filter_map(|path| {
            let relative = path
                .strip_prefix(root)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            Some((relative, path))
        })
        .collect();
    if files.is_empty() {
        return None;
    }
    files.sort();
    let mut hasher = Sha256::new();
    for (relative, path) in files {
        let bytes = std::fs::read(&path).ok()?;
        hasher.update(relative.as_bytes());
        hasher.update([0_u8]);
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(&bytes);
    }
    Some(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_files(&path, files),
            Ok(kind) if kind.is_file() => files.push(path),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_records_the_creating_binary_without_overwriting() {
        let mut attributes = TraceAttributes::new();
        stamp(&mut attributes);
        assert_eq!(attributes[DAEMON_VERSION], env!("CARGO_PKG_VERSION"));
        assert_eq!(attributes[DAEMON_REVISION], env!("LOOM_DAEMON_GIT_SHA"));

        let mut restored = TraceAttributes::new();
        restored.insert(DAEMON_REVISION.into(), "older".into());
        stamp(&mut restored);
        assert_eq!(restored[DAEMON_REVISION], "older");
    }

    #[test]
    fn workspace_records_install_metadata_and_a_content_sensitive_prompt_digest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".loom/roles")).unwrap();
        std::fs::create_dir_all(root.join(".claude/commands/loom")).unwrap();
        std::fs::write(
            root.join(".loom/install-metadata.json"),
            r#"{"loom_version":"0.19.401","loom_commit":"93df55164"}"#,
        )
        .unwrap();
        std::fs::write(root.join(".loom/roles/builder.md"), "build").unwrap();
        std::fs::write(root.join(".claude/commands/loom/sweep.md"), "sweep").unwrap();

        let first = workspace(root);
        assert_eq!(first[INSTALL_VERSION], "0.19.401");
        assert_eq!(first[INSTALL_REVISION], "93df55164");
        assert!(first[PROMPTS_DIGEST].starts_with("sha256:"));
        assert_eq!(workspace(root), first, "digest is deterministic");

        std::fs::write(root.join(".loom/roles/builder.md"), "build!").unwrap();
        assert_ne!(workspace(root)[PROMPTS_DIGEST], first[PROMPTS_DIGEST]);
    }

    #[test]
    fn workspace_without_loom_omits_rather_than_invents() {
        let dir = tempfile::tempdir().unwrap();
        assert!(workspace(dir.path()).is_empty());
    }
}
