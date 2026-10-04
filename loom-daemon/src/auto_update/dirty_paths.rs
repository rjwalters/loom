//! The dirty-tree refusal's path detail (Issue #7608), moved verbatim out of
//! `auto_update.rs` (frozen by `.loom/docs/file-size-policy.md`) to make room for the
//! roll-window hooks (#9132).

/// Append the first three `paths` (plus a count of any remainder) to `reason`
/// (Issue #7608) — e.g. `"<reason> — dirty paths (2): foo.rs, bar.rs"`, or
/// `"... (5): a, b, c, +2 more"` past three. Returns `reason` unchanged when
/// `paths` is empty (no path detail available, e.g. the probe couldn't
/// re-resolve them).
#[must_use]
pub(super) fn with_dirty_paths(reason: &str, paths: Vec<String>) -> String {
    if paths.is_empty() {
        return reason.to_string();
    }
    let shown: Vec<&str> = paths.iter().take(3).map(String::as_str).collect();
    let remainder = paths.len().saturating_sub(shown.len());
    let suffix = if remainder > 0 {
        format!(", +{remainder} more")
    } else {
        String::new()
    };
    format!("{reason} — dirty paths ({}): {}{suffix}", paths.len(), shown.join(", "))
}
