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

#[tauri::command]
pub fn check_github_remote() -> Result<bool, String> {
    let output = Command::new("git")
        .args(["remote", "-v"])
        .output()
        .map_err(|e| format!("Failed to run git remote: {e}"))?;

    if !output.status.success() {
        return Ok(false);
    }

    let remotes = String::from_utf8_lossy(&output.stdout);
    Ok(remotes.contains("github.com"))
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

    // Step 1: Remove loom:in-progress from all open issues
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
