//! The dependency-manifest allowlist, and the workflow version-pin carve-out
//! (#4765).
//!
//! This replaces criterion #3's critical-file *exclusion* for a trusted-bot
//! PR. It is an allowlist rather than a relaxed blocklist on purpose: the
//! qualifying set is closed and enumerable, so a file nobody anticipated
//! fails the check and drops the PR back to the strict criteria — the safe
//! direction. A relaxed blocklist fails the other way.

/// Exact basenames that are dependency manifests or lockfiles.
///
/// Matched on the **basename**, so a nested manifest (`mcp-loom/package.json`,
/// `dashboard/web/package-lock.json`) qualifies — nested manifests are exactly
/// where Loom's own Dependabot backlog accumulated (#7577).
const MANIFEST_BASENAMES: &[&str] = &[
    // Rust
    "Cargo.toml",
    "Cargo.lock",
    // JavaScript / TypeScript
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
    // Python
    "pyproject.toml",
    "poetry.lock",
    "uv.lock",
    "Pipfile",
    "Pipfile.lock",
    "setup.cfg",
    // Go
    "go.mod",
    "go.sum",
    // Ruby
    "Gemfile",
    "Gemfile.lock",
    // PHP
    "composer.json",
    "composer.lock",
    // JVM
    "gradle.lockfile",
];

/// Paths that look like manifests but are **config for the bot itself**, not a
/// dependency declaration. A bot PR that rewrites its own update policy is not
/// the reviewed-by-CI class this feature waives criteria for, so it is
/// excluded explicitly (issue #4765 names this one).
const EXPLICITLY_EXCLUDED: &[&str] = &[".github/dependabot.yml", ".github/dependabot.yaml"];

/// Basename of a repo-relative path, `""` for a path ending in `/`.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `requirements.txt`, `requirements-dev.txt`, `requirements_test.txt` — the
/// `requirements*.txt` family issue #4765 names.
fn is_requirements_txt(base: &str) -> bool {
    base.starts_with("requirements") && base.ends_with(".txt")
}

/// Is this path a dependency manifest or lockfile?
#[must_use]
pub fn is_dependency_manifest(path: &str) -> bool {
    let path = path.trim();
    if EXPLICITLY_EXCLUDED
        .iter()
        .any(|e| e.eq_ignore_ascii_case(path))
    {
        return false;
    }
    let base = basename(path);
    MANIFEST_BASENAMES.contains(&base) || is_requirements_txt(base)
}

/// Is this path a GitHub Actions workflow?
#[must_use]
pub fn is_workflow(path: &str) -> bool {
    let path = path.trim();
    (path.starts_with(".github/workflows/") || path.contains("/.github/workflows/"))
        && (path.ends_with(".yml") || path.ends_with(".yaml"))
}

/// Does every changed line in this workflow file's patch only move an action
/// **version pin**?
///
/// The operator ruling on #4765 (2026-09-14) extends the qualifying class to
/// "a workflow change confined to `uses:` version lines" — a `uses:
/// actions/checkout@v4` → `@v5` bump is the same generated, CI-gated shape as
/// a lockfile line, whereas *any other* workflow hunk changes what runs in CI
/// and must keep failing criterion #3.
///
/// Accepted changed lines:
/// - a `uses:` line whose value carries an `@` pin (`owner/repo@ref`),
/// - a comment-only line (Dependabot rewrites the `# v4.2.1` note beside a
///   SHA pin in the same hunk),
/// - a blank line.
///
/// Anything else — an added `run:`, a changed `env:`, a new job — returns
/// `false` and the PR falls back to the strict criteria.
#[must_use]
pub fn workflow_diff_is_version_pin_only(patch: &str) -> bool {
    let mut saw_change = false;
    for line in patch.lines() {
        // File headers, not content.
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        let Some(rest) = line.strip_prefix('+').or_else(|| line.strip_prefix('-')) else {
            continue; // context, hunk header, "\ No newline at end of file"
        };
        saw_change = true;
        if !is_version_pin_line(rest) {
            return false;
        }
    }
    // An empty patch proves nothing, so it cannot prove "pin-only". Fail
    // closed: the caller treats that as a disqualification, not a pass.
    saw_change
}

/// One changed line from a workflow patch, with its `+`/`-` marker stripped.
fn is_version_pin_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return true;
    }
    // `uses: owner/repo@ref` or `- uses: owner/repo@ref`, optionally with a
    // trailing ` # comment`. The `@` is required: a local `uses: ./.github/...`
    // composite action has no version to pin, so a change to it is a change to
    // what runs, not to a pin.
    let t = t.strip_prefix("- ").unwrap_or(t);
    let Some(value) = t.strip_prefix("uses:") else {
        return false;
    };
    let value = value.split('#').next().unwrap_or("").trim();
    let value = value.trim_matches(|c| c == '"' || c == '\'');
    match value.split_once('@') {
        Some((owner_repo, reference)) => !owner_repo.is_empty() && !reference.is_empty(),
        None => false,
    }
}

#[cfg(test)]
mod tests;
