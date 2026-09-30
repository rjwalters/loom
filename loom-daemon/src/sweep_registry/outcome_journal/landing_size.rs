//! The repo-owned **generated-path classification** and the landing-diff
//! hand-written size facts (`hw_lines_added`, `hw_lines_deleted`, `hw_files`,
//! `generated_lines`, `test_lines`) — Issue #9466.
//!
//! # Why one classifier
//!
//! `lines_added`/`lines_deleted` count everything, and in the silicon repos
//! the raw diff is dominated by simulator output (`~400k .log`, ~290k .json
//! records, ~200k .spice, ~70k .def lines over 329 landings). p99 "filtered
//! LOC" was 86k lines with a generic filter vs 3.4k hand-written-only. Every
//! consumer inventing its own filter is how that stayed invisible, so the
//! classification lives here once: a shipped default glob list, overridable
//! per repo by a `generatedPaths` glob list in `.loom/config.json` (read
//! through `config_resolver::resolve_effective_config`), shared by the
//! daemon, the `issue_effort`/`landed_size` rollups
//! (`defaults/observability/sweep-facts/`), and the storyline side.
//!
//! # Semantics
//!
//! Globs are `/`-separated path patterns supporting `*` (within a segment),
//! `?` (one character) and `**` (any number of path segments). A config list
//! **replaces** the default (an additive union would make a repo unable to
//! un-exclude a default). Test files are recognized by the documented
//! convention: any path segment named `test`, `tests`, `spec` or `specs`, or
//! a file named `test_*` / `*_test.*` / `*.spec.*` / `*.test.*`.
//!
//! Absent stays absent: when the diff's paths cannot be classified (no
//! worktree, git failed), every field is omitted — never a fabricated 0/100%.

/// The shipped default (Issue #9466): lockfiles, record/result/run output
/// trees, machine-readable dumps, simulator and netlist output, minified
/// bundles, vendored trees.
pub(crate) const DEFAULT_GENERATED_PATHS: &[&str] = &[
    // Dependency lockfiles — regenerated, never hand-edited.
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
    "Gemfile.lock",
    "composer.lock",
    "flake.lock",
    "bun.lockb",
    // Record/result/run output trees (the Loom records convention and the
    // silicon repos' sim-output convention).
    "records/**",
    "results/**",
    "runs/**",
    "output/**",
    "outputs/**",
    "out/**",
    // Build output and vendored/minified trees.
    "dist/**",
    "build/**",
    "vendor/**",
    "node_modules/**",
    // Simulator / EDA output: logs, netlists, physical design, waveforms.
    "*.log",
    "*.lst",
    "*.spice",
    "*.spi",
    "*.sp",
    "*.cir",
    "*.cdr",
    "*.def",
    "*.spef",
    "*.sdf",
    "*.gds",
    "*.gdsii",
    "*.gds2",
    "*.cdl",
    "*.ngspice",
    "*.lis",
    "*.mt0",
    "*.ma0",
    "*.ac0",
    "*.raw",
    "*.vcd",
    "*.fsdb",
    "*.csv",
    "*.tsv",
    // Minified web bundles.
    "*.min.js",
    "*.min.css",
    "*.map",
];

/// The `.loom/config.json` key carrying a repo's override list.
const CONFIG_KEY: &str = "generatedPaths";

/// A compiled pattern list; build with [`GeneratedMatcher::new`].
#[derive(Debug, Clone)]
pub(crate) struct GeneratedMatcher {
    regexes: Vec<regex::Regex>,
}

impl GeneratedMatcher {
    /// Compile `patterns` (empty ⇒ nothing is generated).
    ///
    /// Semantics are gitignore-like where it matters: a pattern with no `/`
    /// (`*.log`) matches at ANY depth, and `**` crosses segments. A config
    /// list **replaces** the default (an additive union would make a repo
    /// unable to un-exclude a default).
    ///
    /// An invalid pattern from repo config is *skipped* (logged), because a
    /// bad glob in `.loom/config.json` must not take down the outcome
    /// journal; the shipped default is compile-checked by this module's
    /// tests.
    #[must_use]
    pub fn new(patterns: &[String]) -> Self {
        let regexes = patterns
            .iter()
            .filter_map(|pattern| {
                // Gitignore-style depth rule: a bare `*.ext` names files at
                // any depth, not just the repo root.
                let effective = if pattern.contains('/') {
                    pattern.clone()
                } else {
                    format!("**/{pattern}")
                };
                match glob_to_regex(&effective) {
                    Ok(regex) => Some(regex),
                    Err(error) => {
                        log::warn!(
                            "landing_size: skipping invalid generatedPaths glob {pattern:?}: {error}"
                        );
                        None
                    }
                }
            })
            .collect();
        Self { regexes }
    }

    /// Whether `path` (a repo-relative, `/`-separated diff path) is
    /// generated-classified.
    #[must_use]
    pub fn is_generated(&self, path: &str) -> bool {
        let normalized = path.trim_start_matches("./");
        self.regexes.iter().any(|regex| regex.is_match(normalized))
    }
}

/// The effective pattern list for a workspace: the repo's `generatedPaths`
/// config when it is a non-empty array of strings, else the shipped default.
#[must_use]
pub(crate) fn generated_patterns(workspace_root: &std::path::Path) -> Vec<String> {
    let configured = crate::config_resolver::resolve_effective_config(workspace_root)
        .get(CONFIG_KEY)
        .and_then(|value| value.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if configured.is_empty() {
        DEFAULT_GENERATED_PATHS
            .iter()
            .map(ToString::to_string)
            .collect()
    } else {
        configured
    }
}

/// The documented test-file convention (Issue #9466's `test_lines`): a
/// `test`/`spec` segment, or a `test_*` / `*_test.` / `.spec.` / `.test.`
/// file name.
#[must_use]
pub(crate) fn is_test_path(path: &str) -> bool {
    let normalized = path.trim_start_matches("./");
    let (dirs, file) = match normalized.rsplit_once('/') {
        Some((dirs, file)) => (Some(dirs), file),
        None => (None, normalized),
    };
    if dirs.is_some_and(|dirs| {
        dirs.split('/')
            .any(|segment| matches!(segment, "test" | "tests" | "spec" | "specs" | "__tests__"))
    }) {
        return true;
    }
    let lower = file.to_ascii_lowercase();
    lower.starts_with("test_")
        || lower.contains("_test.")
        || lower.contains(".spec.")
        || lower.contains(".test.")
        || lower.starts_with("test-")
        || lower.ends_with("_test")
}

/// One landing-diff row — [`crate::git_utils::GitDiffRow`] re-imported so
/// the classifier and the numstat reader share one shape.
pub(crate) use crate::git_utils::GitDiffRow as NumStat;

/// The classified split of a landing diff (Issue #9466). Every field is
/// hand-written-or-test vs generated along [`NumStat`] rows; binary rows
/// (`-\t-\tpath`) arrive pre-skipped by the numstat reader.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LandingSize {
    pub hw_lines_added: i64,
    pub hw_lines_deleted: i64,
    pub hw_files: i64,
    pub generated_lines: i64,
    pub test_lines: i64,
}

/// Classify numstat rows into the landing-size split.
#[must_use]
pub(crate) fn classify_numstat(rows: &[NumStat], matcher: &GeneratedMatcher) -> LandingSize {
    let mut size = LandingSize::default();
    for row in rows {
        if matcher.is_generated(&row.path) {
            size.generated_lines += row.added + row.deleted;
            continue;
        }
        size.hw_lines_added += row.added;
        size.hw_lines_deleted += row.deleted;
        size.hw_files += 1;
        if is_test_path(&row.path) {
            size.test_lines += row.added + row.deleted;
        }
    }
    size
}

/// Translate one glob to a full-match [`regex::Regex`]. `*` matches within a
/// segment, `**` matches across segments, `?` matches one non-`/` character;
/// everything else is literal.
fn glob_to_regex(pattern: &str) -> Result<regex::Regex, String> {
    let mut regex = String::from("^");
    let segments: Vec<&str> = pattern.split('/').collect();
    let total = segments.len();
    for (index, segment) in segments.iter().enumerate() {
        let last = index + 1 == total;
        if *segment == "**" {
            if last {
                // `a/**` matches `a` itself and everything under it; a
                // leading `**/` or bare `**` matches everything.
                if regex.ends_with('/') {
                    regex.pop();
                    regex.push_str("(?:/.*)?");
                } else {
                    regex.push_str(".*");
                }
            } else {
                // A mid-pattern `**` crosses segments.
                regex.push_str("(?:.*/)?");
            }
            continue;
        }
        for character in segment.chars() {
            match character {
                '*' => regex.push_str("[^/]*"),
                '?' => regex.push_str("[^/]"),
                character => regex.push_str(&regex::escape(&character.to_string())),
            }
        }
        if !last {
            regex.push('/');
        }
    }
    regex.push('$');
    regex::Regex::new(&regex).map_err(|error| error.to_string())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests {
    use super::*;

    fn default_matcher() -> GeneratedMatcher {
        GeneratedMatcher::new(
            &DEFAULT_GENERATED_PATHS
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        )
    }

    fn rows(pairs: &[(&str, i64)]) -> Vec<NumStat> {
        pairs
            .iter()
            .map(|(path, lines)| NumStat {
                added: *lines,
                deleted: 0,
                path: (*path).to_string(),
            })
            .collect()
    }

    #[test]
    fn default_globs_compile_and_classify() {
        let matcher = default_matcher();
        assert!(matcher.is_generated("records/issue-42/run1.log"));
        assert!(matcher.is_generated("sim/out.netlist.spef"));
        assert!(matcher.is_generated("package-lock.json"));
        assert!(matcher.is_generated("results/sweep.csv"));
        assert!(!matcher.is_generated("src/main.rs"));
        assert!(!matcher.is_generated("docs/telemetry.md"));
        assert!(
            !matcher.is_generated("src/records.rs"),
            "a file named records.rs is not the records/ tree"
        );
    }

    #[test]
    fn doublestar_matches_directory_and_contents() {
        let matcher = GeneratedMatcher::new(&["a/**".to_string(), "**/b.txt".to_string()]);
        assert!(matcher.is_generated("a"));
        assert!(matcher.is_generated("a/z/c"));
        assert!(matcher.is_generated("x/y/b.txt"));
        assert!(matcher.is_generated("b.txt"));
        assert!(!matcher.is_generated("ab"));
    }

    #[test]
    fn invalid_config_glob_is_skipped_not_fatal() {
        let matcher = GeneratedMatcher::new(&["[".to_string(), "*.log".to_string()]);
        assert!(!matcher.is_generated("x["));
        assert!(matcher.is_generated("a.log"));
    }

    #[test]
    fn config_list_replaces_the_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let loom = dir.path().join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(
            loom.join("config.json"),
            serde_json::json!({"generatedPaths": ["gen/**"]}).to_string(),
        )
        .unwrap();
        let patterns = generated_patterns(dir.path());
        assert_eq!(patterns, vec!["gen/**".to_string()]);
        let matcher = GeneratedMatcher::new(&patterns);
        assert!(matcher.is_generated("gen/x.log"));
        assert!(!matcher.is_generated("package-lock.json"), "config replaces the default");
    }

    #[test]
    fn absent_config_falls_back_to_default() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(generated_patterns(dir.path()).first().unwrap(), "package-lock.json");
    }

    #[test]
    fn test_path_convention() {
        assert!(is_test_path("tests/test_main.rs"));
        assert!(is_test_path("src/foo_test.rs"));
        assert!(is_test_path("web/app.spec.ts"));
        assert!(is_test_path("src/test_util.rs"), "test_ prefix under src counts as test file");
        assert!(!is_test_path("src/main.rs"));
        assert!(!is_test_path("src/testing.rs"));
    }

    #[test]
    fn classify_splits_handwritten_generated_and_test() {
        let matcher = default_matcher();
        let size = classify_numstat(
            &rows(&[
                ("src/main.rs", 100),
                ("tests/test_main.rs", 40),
                ("records/run.out.log", 4_000),
                ("sim/out.spef", 2_000),
            ]),
            &matcher,
        );
        assert_eq!(
            size.hw_lines_added, 140,
            "test lines are hand-written work too — hw_lines includes them"
        );
        assert_eq!(size.hw_files, 2);
        assert_eq!(size.test_lines, 40);
        assert_eq!(size.generated_lines, 6_000);
    }

    #[test]
    fn empty_diff_is_a_zero_classify_not_an_absent_one() {
        // A real empty diff (nothing changed) classifies to all-zero; the
        // *absent* case is the caller's (no worktree / git failed → omit).
        let size = classify_numstat(&[], &default_matcher());
        assert_eq!(size, LandingSize::default());
    }
}
