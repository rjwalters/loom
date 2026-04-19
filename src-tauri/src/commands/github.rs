use std::path::Path;
use std::process::Command;

/// Helper structs for JSON parsing
#[derive(serde::Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(serde::Deserialize)]
struct GhIssue {
    number: u32,
}

#[derive(serde::Serialize)]
pub struct LabelResetResult {
    pub issues_cleaned: usize,
    pub errors: Vec<String>,
}

/// Extract hostname from a git remote URL.
///
/// Supports both SSH (`git@host:owner/repo.git`) and HTTPS (`https://host/owner/repo`) formats.
fn parse_host_from_url(url: &str) -> Option<String> {
    // SSH format: git@host:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some(colon_pos) = rest.find(':') {
            return Some(rest[..colon_pos].to_string());
        }
    }

    // HTTPS format: https://host/owner/repo
    if url.starts_with("http://") || url.starts_with("https://") {
        let without_scheme = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))?;
        let host = without_scheme.split('/').next()?;
        if !host.is_empty() {
            return Some(host.to_string());
        }
    }

    None
}

/// Read the configured Gitea host from `.loom/config.json`, if any.
///
/// Looks for `forge.gitea.url` and extracts the hostname.
fn get_configured_gitea_host() -> Option<String> {
    // Find the workspace root by looking for .loom/config.json relative to git toplevel
    let toplevel_output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;

    if !toplevel_output.status.success() {
        return None;
    }

    let toplevel = String::from_utf8_lossy(&toplevel_output.stdout)
        .trim()
        .to_string();
    let config_path = Path::new(&toplevel).join(".loom/config.json");

    let content = std::fs::read_to_string(config_path).ok()?;
    let config: serde_json::Value = serde_json::from_str(&content).ok()?;

    let gitea_url = config.get("forge")?.get("gitea")?.get("url")?.as_str()?;

    // Extract host from the URL
    let without_scheme = gitea_url
        .strip_prefix("https://")
        .or_else(|| gitea_url.strip_prefix("http://"))?;
    let host = without_scheme.split('/').next()?;
    if host.is_empty() {
        return None;
    }
    Some(host.to_string())
}

/// Check whether the workspace has a recognized forge remote (GitHub or configured Gitea).
///
/// Resolution:
/// 1. Get the origin remote URL via `git remote get-url origin`
/// 2. Parse the hostname from the URL
/// 3. Return `true` if the host is `github.com` or matches the Gitea host from `.loom/config.json`
/// 4. Fall back to `false` for unrecognized hosts
#[tauri::command]
pub fn check_github_remote() -> Result<bool, String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .map_err(|e| format!("Failed to run git remote get-url: {e}"))?;

    if !output.status.success() {
        return Ok(false);
    }

    let remote_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let host = match parse_host_from_url(&remote_url) {
        Some(h) => h,
        None => return Ok(false),
    };

    // GitHub is always recognized
    if host == "github.com" {
        return Ok(true);
    }

    // Check if host matches configured Gitea URL
    if let Some(gitea_host) = get_configured_gitea_host() {
        if host == gitea_host {
            return Ok(true);
        }
    }

    // Check LOOM_FORGE_TYPE env var override
    if let Ok(forge_type) = std::env::var("LOOM_FORGE_TYPE") {
        let ft = forge_type.to_lowercase();
        if ft == "github" || ft == "gitea" {
            return Ok(true);
        }
    }

    Ok(false)
}

#[tauri::command]
pub fn check_label_exists(name: &str) -> Result<bool, String> {
    let output = Command::new("gh")
        .args(["label", "list", "--json", "name"])
        .output()
        .map_err(|e| format!("Failed to run gh label list: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh label list failed: {stderr}"));
    }

    // Parse JSON in Rust instead of using jq to prevent injection
    let labels: Vec<GhLabel> = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse label JSON: {e}"))?;

    Ok(labels.iter().any(|l| l.name == name))
}

#[tauri::command]
pub fn create_github_label(name: &str, description: &str, color: &str) -> Result<(), String> {
    let output = Command::new("gh")
        .args([
            "label",
            "create",
            name,
            "--description",
            description,
            "--color",
            color,
        ])
        .output()
        .map_err(|e| format!("Failed to run gh label create: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh label create failed: {stderr}"));
    }

    Ok(())
}

#[tauri::command]
pub fn update_github_label(name: &str, description: &str, color: &str) -> Result<(), String> {
    let output = Command::new("gh")
        .args([
            "label",
            "create",
            name,
            "--description",
            description,
            "--color",
            color,
            "--force",
        ])
        .output()
        .map_err(|e| format!("Failed to run gh label create --force: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh label update failed: {stderr}"));
    }

    Ok(())
}

#[tauri::command]
pub fn reset_github_labels() -> Result<LabelResetResult, String> {
    let mut result = LabelResetResult {
        issues_cleaned: 0,
        errors: Vec::new(),
    };

    // Step 1: Migration cleanup - Remove deprecated loom:in-progress label
    //
    // MIGRATION CODE: This handles cleanup of the deprecated `loom:in-progress` label.
    // Background: loom:in-progress was deprecated in favor of role-specific labels
    // (loom:building, loom:curating, loom:reviewing, loom:treating) in PR #808 (Dec 2025).
    //
    // This code can be removed after March 2026 when all installations have migrated.
    // See: Issue #791 (deprecation request), PR #808 (implementation), Issue #903 (this review)
    let issues_output = Command::new("gh")
        .args([
            "issue",
            "list",
            "--label",
            "loom:in-progress",
            "--state",
            "open",
            "--json",
            "number",
        ])
        .output()
        .map_err(|e| format!("Failed to list issues: {e}"))?;

    if issues_output.status.success() {
        // Parse JSON in Rust instead of using jq to prevent injection
        let issues: Vec<GhIssue> = serde_json::from_slice(&issues_output.stdout)
            .map_err(|e| format!("Failed to parse issue JSON: {e}"))?;

        for issue in issues {
            let issue_num = issue.number.to_string();

            let remove_output = Command::new("gh")
                .args([
                    "issue",
                    "edit",
                    &issue_num,
                    "--remove-label",
                    "loom:in-progress",
                ])
                .output()
                .map_err(|e| format!("Failed to remove label: {e}"))?;

            if remove_output.status.success() {
                result.issues_cleaned += 1;
            } else {
                let error = format!(
                    "Failed to remove loom:in-progress from issue {issue_num}: {}",
                    String::from_utf8_lossy(&remove_output.stderr)
                );
                result.errors.push(error);
            }
        }
    }

    // Step 2: Replace loom:reviewing with loom:review-requested on all open PRs
    let prs_output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--label",
            "loom:reviewing",
            "--state",
            "open",
            "--json",
            "number",
        ])
        .output()
        .map_err(|e| format!("Failed to list PRs: {e}"))?;

    if prs_output.status.success() {
        let prs: Vec<GhIssue> = serde_json::from_slice(&prs_output.stdout)
            .map_err(|e| format!("Failed to parse PR JSON: {e}"))?;

        for pr in prs {
            let pr_num = pr.number.to_string();

            let edit_output = Command::new("gh")
                .args([
                    "pr",
                    "edit",
                    &pr_num,
                    "--remove-label",
                    "loom:reviewing",
                    "--add-label",
                    "loom:review-requested",
                ])
                .output()
                .map_err(|e| format!("Failed to edit PR: {e}"))?;

            if edit_output.status.success() {
                result.issues_cleaned += 1;
            } else {
                let error = format!(
                    "Failed to update labels on PR {pr_num}: {}",
                    String::from_utf8_lossy(&edit_output.stderr)
                );
                result.errors.push(error);
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_github_ssh_url() {
        assert_eq!(
            parse_host_from_url("git@github.com:owner/repo.git"),
            Some("github.com".to_string())
        );
    }

    #[test]
    fn test_parse_github_https_url() {
        assert_eq!(
            parse_host_from_url("https://github.com/owner/repo.git"),
            Some("github.com".to_string())
        );
    }

    #[test]
    fn test_parse_gitea_ssh_url() {
        assert_eq!(
            parse_host_from_url("git@gitea.example.com:owner/repo.git"),
            Some("gitea.example.com".to_string())
        );
    }

    #[test]
    fn test_parse_gitea_https_url() {
        assert_eq!(
            parse_host_from_url("https://gitea.example.com/owner/repo"),
            Some("gitea.example.com".to_string())
        );
    }

    #[test]
    fn test_parse_http_url() {
        assert_eq!(
            parse_host_from_url("http://gitea.local:3000/owner/repo"),
            Some("gitea.local:3000".to_string())
        );
    }

    #[test]
    fn test_parse_unknown_format() {
        assert_eq!(parse_host_from_url("not-a-url"), None);
    }

    #[test]
    fn test_parse_empty_string() {
        assert_eq!(parse_host_from_url(""), None);
    }
}
