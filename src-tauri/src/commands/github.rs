use std::process::Command;

/// Helper structs for JSON parsing
#[derive(serde::Deserialize)]
struct ForgeEntity {
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

/// Reset label state machine by transitioning deprecated labels on open PRs.
///
/// Dispatches through `loom-forge` CLI for forge-agnostic support (GitHub and Gitea).
#[tauri::command]
pub fn reset_github_labels() -> Result<LabelResetResult, String> {
    let mut result = LabelResetResult {
        issues_cleaned: 0,
        errors: Vec::new(),
    };

    // Replace loom:reviewing with loom:review-requested on all open PRs.
    // Uses loom-forge CLI for forge-agnostic dispatch.
    let prs_output = Command::new("loom-forge")
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
        .map_err(|e| format!("Failed to list PRs via loom-forge: {e}"))?;

    if prs_output.status.success() {
        let prs: Vec<ForgeEntity> = serde_json::from_slice(&prs_output.stdout)
            .map_err(|e| format!("Failed to parse PR JSON: {e}"))?;

        for pr in prs {
            let pr_num = pr.number.to_string();

            let edit_output = Command::new("loom-forge")
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
                .map_err(|e| format!("Failed to edit PR via loom-forge: {e}"))?;

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
