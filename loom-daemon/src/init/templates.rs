//! Template variable substitution for Loom installation
//!
//! Handles substitution of template variables in installed files like CLAUDE.md.

use chrono::Local;

/// Loom installation metadata for template variable substitution
#[derive(Default)]
pub struct LoomMetadata {
    /// Loom version from `LOOM_VERSION` env var (e.g., "0.1.0")
    pub version: Option<String>,
    /// Loom commit hash from `LOOM_COMMIT` env var (e.g., "d6cf9ac")
    pub commit: Option<String>,
    /// Installation date (generated at runtime)
    pub install_date: String,
}

impl LoomMetadata {
    /// Create metadata by reading from environment variables, falling back to
    /// the values compiled into this binary.
    ///
    /// The shell installers (`install.sh`, `scripts/install-loom.sh`) export
    /// `LOOM_VERSION`/`LOOM_COMMIT` describing the source tree being installed
    /// *from*, so a non-empty env value always wins — the machine-level
    /// `~/.local/bin/loom-daemon` binary (#3922) can be older than that source
    /// tree. When the env vars are unset or empty (the direct `loom-daemon
    /// init` path, which no wrapper sets them on), fall back to the binary's
    /// own compiled-in version/commit rather than the literal `"unknown"` that
    /// previously leaked into `.loom/CLAUDE.md` and `install-metadata.json`
    /// (#4050).
    pub fn from_env() -> Self {
        Self {
            version: env_or_compiled("LOOM_VERSION", env!("CARGO_PKG_VERSION")),
            commit: env_or_compiled("LOOM_COMMIT", crate::self_update::BUILT_COMMIT),
            install_date: Local::now().format("%Y-%m-%d").to_string(),
        }
    }
}

/// Resolve a template value from `$var`, falling back to a compiled-in constant.
///
/// A non-empty env value wins (the installer's source-tree description). An
/// unset or empty env var falls back to `compiled`, and if that itself is empty
/// or the literal `"unknown"` (e.g. a tarball build with no `.git`) the result
/// is `None` so the caller's own `"unknown"` fallback applies.
fn env_or_compiled(var: &str, compiled: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => {
            if compiled.is_empty() || compiled == "unknown" {
                None
            } else {
                Some(compiled.to_string())
            }
        }
    }
}

/// Replace template variables in a string
///
/// Replaces the following template variables:
/// - `{{REPO_OWNER}}`: Repository owner from git remote
/// - `{{REPO_NAME}}`: Repository name from git remote
/// - `{{LOOM_VERSION}}`: Loom version from environment
/// - `{{LOOM_COMMIT}}`: Loom commit hash from environment
/// - `{{INSTALL_DATE}}`: Current date (YYYY-MM-DD format)
///
/// If repo info is not available (non-GitHub remote or no remote),
/// falls back to generic placeholders. If Loom metadata is not available,
/// falls back to "unknown" placeholders.
pub fn substitute_template_variables(
    content: &str,
    repo_owner: Option<&str>,
    repo_name: Option<&str>,
    loom_metadata: &LoomMetadata,
) -> String {
    let owner = repo_owner.unwrap_or("OWNER");
    let name = repo_name.unwrap_or("REPO");
    let version = loom_metadata.version.as_deref().unwrap_or("unknown");
    let commit = loom_metadata.commit.as_deref().unwrap_or("unknown");

    content
        .replace("{{REPO_OWNER}}", owner)
        .replace("{{REPO_NAME}}", name)
        .replace("{{LOOM_VERSION}}", version)
        .replace("{{LOOM_COMMIT}}", commit)
        .replace("{{INSTALL_DATE}}", &loom_metadata.install_date)
}

/// Rewrite repo-root-relative markdown link *targets* so they resolve
/// correctly once written to `.loom/CLAUDE.md` (or `.loom/AGENTS.md`) — a
/// destination one directory level deeper than the repo root the template's
/// links are authored against.
///
/// `defaults/.loom/CLAUDE.md` is authored with link targets relative to
/// **repo root** — the form that's correct for the short pointer injected
/// into a consumer's root `CLAUDE.md`, and for this repo's own
/// hand-maintained root `CLAUDE.md`. But the *full* template is also written
/// verbatim to `<workspace>/.loom/CLAUDE.md`, one directory level deeper, so
/// every root-relative link needs re-basing for that destination (issue
/// #5975). Two shapes appear in the shipped template, each needing a
/// different transform:
///
/// - `.loom/docs/...` — `docs/` is installed as a *sibling* of
///   `.loom/CLAUDE.md` (`sync_managed_dir(&defaults, &loom_path, "docs", ...)`
///   in `init/mod.rs`), so the leading `.loom/` segment is simply stripped:
///   `.loom/docs/foo.md` -> `docs/foo.md`. This generalizes to any
///   `.loom/`-nested target, not just `docs/`.
/// - `.github/...` (and any other repo-root directory that is NOT nested
///   under `.loom/`, e.g. `.github/CONFIGURATION.md`) — installed at repo
///   root, unchanged, so the target needs a `../` prefix to climb back out
///   of `.loom/`: `.github/CONFIGURATION.md` -> `../.github/CONFIGURATION.md`.
///
/// Only the markdown link *target* — the `(...)` portion — is rewritten, not
/// the bracketed display text, so a link can still visibly describe the
/// doc's repo-root-relative path (`` [`.loom/docs/foo.md`](docs/foo.md) ``)
/// while resolving correctly from `.loom/CLAUDE.md`'s own location. Call this
/// on content that is about to be written to a `.loom/`-nested destination
/// only — never on content destined for repo root, which needs the
/// original root-relative targets unchanged.
pub fn localize_dotloom_doc_links(content: &str) -> String {
    content
        .replace("](.loom/", "](")
        .replace("](.github/", "](../.github/")
}

/// Template variable placeholders that must be substituted before a file is written.
///
/// Used by [`assert_no_placeholders`] to fail-fast if a templated file is about to be
/// written with unsubstituted placeholders (defense-in-depth against the upgrade-path
/// regression in #3325, where the installer left literal `{{LOOM_VERSION}}` etc.
/// on disk).
pub const TEMPLATE_PLACEHOLDERS: &[&str] = &[
    "{{REPO_OWNER}}",
    "{{REPO_NAME}}",
    "{{LOOM_VERSION}}",
    "{{LOOM_COMMIT}}",
    "{{INSTALL_DATE}}",
];

/// Assert that `content` contains none of the known template placeholders.
///
/// Returns `Err` with a descriptive message naming the offending placeholders and the
/// `file_label` (e.g. `"CLAUDE.md"`) so callers can surface a clear install-time error.
/// Returns `Ok(())` when no placeholders remain.
///
/// This is a defense-in-depth check intended to be called immediately before writing
/// a file that was supposed to have been template-substituted. It does **not** itself
/// perform substitution — that's [`substitute_template_variables`]'s job.
pub fn assert_no_placeholders(content: &str, file_label: &str) -> Result<(), String> {
    let leaked: Vec<&str> = TEMPLATE_PLACEHOLDERS
        .iter()
        .copied()
        .filter(|p| content.contains(p))
        .collect();
    if leaked.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Refusing to write {file_label}: unsubstituted template placeholder(s) detected: {}. \
             This indicates a bug in the installer's template handling — please report it.",
            leaked.join(", ")
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_substitute_template_variables() {
        let content = r"
**Loom Version**: {{LOOM_VERSION}}
**Loom Commit**: {{LOOM_COMMIT}}
**Installation Date**: {{INSTALL_DATE}}
**Repository**: {{REPO_OWNER}}/{{REPO_NAME}}
";

        // Test with all values provided
        let metadata = LoomMetadata {
            version: Some("1.2.3".to_string()),
            commit: Some("abc1234".to_string()),
            install_date: "2024-01-15".to_string(),
        };

        let result =
            substitute_template_variables(content, Some("myorg"), Some("myrepo"), &metadata);

        assert!(result.contains("**Loom Version**: 1.2.3"));
        assert!(result.contains("**Loom Commit**: abc1234"));
        assert!(result.contains("**Installation Date**: 2024-01-15"));
        assert!(result.contains("**Repository**: myorg/myrepo"));

        // Test with missing values (should use fallbacks)
        let metadata_empty = LoomMetadata {
            version: None,
            commit: None,
            install_date: "2024-01-15".to_string(),
        };

        let result_fallback = substitute_template_variables(content, None, None, &metadata_empty);

        assert!(result_fallback.contains("**Loom Version**: unknown"));
        assert!(result_fallback.contains("**Loom Commit**: unknown"));
        assert!(result_fallback.contains("**Repository**: OWNER/REPO"));
    }

    #[test]
    fn test_localize_dotloom_doc_links_rewrites_link_targets() {
        // Issue #5975: a link authored for repo-root resolution must become
        // repo-relative-to-.loom/ once written to .loom/CLAUDE.md.
        let content = "See [`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md) \
                        and [build-gate](.loom/docs/build-gate.md#section).";
        let result = localize_dotloom_doc_links(content);

        assert!(
            result.contains("](docs/daemon-reference.md)"),
            "link target must be rewritten to docs/, got: {result}"
        );
        assert!(
            result.contains("](docs/build-gate.md#section)"),
            "anchor suffix must be preserved through the rewrite, got: {result}"
        );
        // No dangling `.loom/docs/` link targets should remain.
        assert!(!result.contains("](.loom/docs/"), "got: {result}");
    }

    #[test]
    fn test_localize_dotloom_doc_links_preserves_display_text() {
        // Only the link TARGET (the `(...)` portion) is rewritten — the
        // bracketed display text still describes the repo-root-relative
        // path, which remains a useful, accurate description for a reader.
        let content = "[`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md)";
        let result = localize_dotloom_doc_links(content);
        assert_eq!(result, "[`.loom/docs/daemon-reference.md`](docs/daemon-reference.md)");
    }

    #[test]
    fn test_localize_dotloom_doc_links_rebases_github_links() {
        // Issue #5975 follow-on: `.github/CONFIGURATION.md` is the same bug
        // class as `.loom/docs/...` — it's installed at repo root (NOT
        // nested under `.loom/`), so from `.loom/CLAUDE.md`'s location the
        // link needs a `../` prefix rather than a stripped `.loom/` prefix.
        let content = "[`.github/CONFIGURATION.md`](.github/CONFIGURATION.md)";
        let result = localize_dotloom_doc_links(content);
        assert_eq!(result, "[`.github/CONFIGURATION.md`](../.github/CONFIGURATION.md)");
    }

    #[test]
    fn test_localize_dotloom_doc_links_leaves_prose_and_other_paths_untouched() {
        // A bare backtick-quoted prose mention (not a markdown link target)
        // must be left alone — it correctly describes the repo-root-relative
        // convention and isn't a link that resolves anywhere.
        let content = "Reference-tier detail lives in `.loom/docs/*`, installed alongside \
                        this file. Full role definitions: `.loom/roles/*.md`.";
        let result = localize_dotloom_doc_links(content);
        assert_eq!(result, content, "non-link prose must be unchanged");
    }

    #[test]
    fn test_localize_dotloom_doc_links_noop_on_already_correct_content() {
        // Idempotent: content with no `.loom/docs/` link targets is returned
        // unchanged (e.g. AGENTS.md, which has none as of #5975).
        let content = "# AGENTS.md\n\nNo doc links here, just prose about `.loom/CLAUDE.md`.";
        assert_eq!(localize_dotloom_doc_links(content), content);
    }

    #[test]
    fn test_assert_no_placeholders_passes_on_clean_content() {
        // Substituted content should pass — no `{{...}}` markers left.
        let clean = "**Loom Version**: 0.8.0\n**Installation Date**: 2026-05-26";
        assert!(assert_no_placeholders(clean, "CLAUDE.md").is_ok());

        // Empty content trivially passes.
        assert!(assert_no_placeholders("", "CLAUDE.md").is_ok());

        // Content that happens to contain "{{" but not a known placeholder is fine.
        assert!(assert_no_placeholders("see {{custom}} for details", "CLAUDE.md").is_ok());
    }

    #[test]
    fn test_assert_no_placeholders_rejects_leaked_placeholders() {
        // Bare LOOM_VERSION placeholder must be detected.
        let leaky = "**Loom Version**: {{LOOM_VERSION}}";
        let err = assert_no_placeholders(leaky, "CLAUDE.md").unwrap_err();
        assert!(err.contains("CLAUDE.md"));
        assert!(err.contains("{{LOOM_VERSION}}"));

        // Multiple leaks should all be enumerated.
        let multi = "{{LOOM_VERSION}} {{INSTALL_DATE}} {{REPO_OWNER}}";
        let err = assert_no_placeholders(multi, "root.md").unwrap_err();
        assert!(err.contains("{{LOOM_VERSION}}"));
        assert!(err.contains("{{INSTALL_DATE}}"));
        assert!(err.contains("{{REPO_OWNER}}"));
    }

    #[test]
    fn test_loom_metadata_from_env() {
        // Test with environment variables set — a non-empty env value wins over
        // the compiled-in constant (the installer's source-tree description,
        // which may be newer than this binary; see #4050).
        std::env::set_var("LOOM_VERSION", "0.5.0");
        std::env::set_var("LOOM_COMMIT", "def5678");

        let metadata = LoomMetadata::from_env();

        assert_eq!(metadata.version, Some("0.5.0".to_string()));
        assert_eq!(metadata.commit, Some("def5678".to_string()));
        // install_date should be today's date in YYYY-MM-DD format
        assert!(metadata.install_date.len() == 10);
        assert!(metadata.install_date.contains('-'));

        // With the env vars unset, from_env() falls back to the binary's own
        // compiled-in version (never the literal "unknown"). This is the
        // direct `loom-daemon init` path the #4050 fix repairs. Done in the
        // SAME test as the env-set case so the two never race under parallel
        // test execution (the templates.rs env-mutation caveat).
        std::env::remove_var("LOOM_VERSION");
        std::env::remove_var("LOOM_COMMIT");

        let fallback = LoomMetadata::from_env();
        assert_eq!(
            fallback.version.as_deref(),
            Some(env!("CARGO_PKG_VERSION")),
            "unset LOOM_VERSION must fall back to the compiled crate version"
        );
        assert_ne!(fallback.version.as_deref(), Some("unknown"));
        // Commit is present unless this binary was built without git metadata
        // (a tarball build ⇒ BUILT_COMMIT == "unknown" ⇒ None here).
        if crate::self_update::BUILT_COMMIT != "unknown" {
            assert_eq!(fallback.commit.as_deref(), Some(crate::self_update::BUILT_COMMIT));
        }
    }

    #[test]
    fn test_env_or_compiled_prefers_nonempty_env() {
        // Use a unique var name so this never races with LOOM_VERSION/COMMIT.
        let var = "LOOM_TEST_ENV_OR_COMPILED_A";
        std::env::set_var(var, "from-env");
        assert_eq!(env_or_compiled(var, "compiled"), Some("from-env".to_string()));
        std::env::remove_var(var);
    }

    #[test]
    fn test_env_or_compiled_empty_env_falls_back() {
        let var = "LOOM_TEST_ENV_OR_COMPILED_B";
        // An empty env value must NOT win — it falls through to the compiled value.
        std::env::set_var(var, "");
        assert_eq!(env_or_compiled(var, "compiled"), Some("compiled".to_string()));
        std::env::remove_var(var);
    }

    #[test]
    fn test_env_or_compiled_unset_uses_compiled() {
        let var = "LOOM_TEST_ENV_OR_COMPILED_UNSET";
        std::env::remove_var(var);
        assert_eq!(env_or_compiled(var, "0.15.0"), Some("0.15.0".to_string()));
        // A compiled value of "unknown" or "" degrades to None so the caller's
        // own "unknown" placeholder applies rather than double-substituting.
        assert_eq!(env_or_compiled(var, "unknown"), None);
        assert_eq!(env_or_compiled(var, ""), None);
    }
}
