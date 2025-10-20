use std::fs;
use std::path::Path;
use std::process::Command;

/// Helper function to initialize .loom directory in a new project
fn init_loom_directory(project_path: &Path) -> Result<(), String> {
    use crate::commands::workspace::resolve_defaults_path;

    let loom_dir = project_path.join(".loom");

    // Create .loom directory
    fs::create_dir_all(&loom_dir).map_err(|e| format!("Failed to create .loom directory: {e}"))?;

    // Copy default config from defaults directory
    let defaults_dir = resolve_defaults_path("defaults")?;

    // Copy entire defaults directory structure to .loom
    super::workspace::copy_dir_recursive(&defaults_dir, &loom_dir)
        .map_err(|e| format!("Failed to copy defaults: {e}"))?;

    // Copy .loom-README.md to .loom/README.md if it exists
    let loom_readme_src = defaults_dir.join(".loom-README.md");
    let loom_readme_dst = loom_dir.join("README.md");
    if loom_readme_src.exists() {
        fs::copy(&loom_readme_src, &loom_readme_dst)
            .map_err(|e| format!("Failed to copy .loom-README.md: {e}"))?;
    }

    // Setup repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .codex/)
    super::workspace::setup_repository_scaffolding(project_path, &defaults_dir)?;

    Ok(())
}

/// Generate license content based on license type
fn generate_license_content(license_type: &str, project_name: &str) -> Result<String, String> {
    use chrono::Datelike;
    let year = chrono::Local::now().year();

    match license_type {
        "MIT" => Ok(format!(
            r#"MIT License

Copyright (c) {year} {project_name}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"#
        )),
        "Apache-2.0" => Ok(format!(
            r#"                                 Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   Copyright {year} {project_name}

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.
"#
        )),
        _ => Err(format!("Unsupported license type: {license_type}")),
    }
}

#[tauri::command]
pub fn create_local_project(
    name: &str,
    location: &str,
    description: Option<String>,
    license: Option<String>,
) -> Result<String, String> {
    let project_path = Path::new(location).join(name);

    // Check if directory already exists
    if project_path.exists() {
        return Err(format!("Directory already exists: {}", project_path.display()));
    }

    // Create project directory
    fs::create_dir_all(&project_path)
        .map_err(|e| format!("Failed to create project directory: {e}"))?;

    // Initialize git repository
    let init_output = Command::new("git")
        .args(["init"])
        .current_dir(&project_path)
        .output()
        .map_err(|e| format!("Failed to run git init: {e}"))?;

    if !init_output.status.success() {
        let stderr = String::from_utf8_lossy(&init_output.stderr);
        return Err(format!("git init failed: {stderr}"));
    }

    // Create README.md
    let readme_content = if let Some(desc) = description {
        format!("# {name}\n\n{desc}\n")
    } else {
        format!("# {name}\n")
    };

    fs::write(project_path.join("README.md"), readme_content)
        .map_err(|e| format!("Failed to create README.md: {e}"))?;

    // Create LICENSE file if specified
    if let Some(license_type) = license {
        let license_content = generate_license_content(&license_type, name)?;
        fs::write(project_path.join("LICENSE"), license_content)
            .map_err(|e| format!("Failed to create LICENSE: {e}"))?;
    }

    // Initialize .loom directory with defaults
    init_loom_directory(&project_path)?;

    // Create initial .gitignore with Loom ephemeral file patterns
    let gitignore_content = "# Loom - AI Development Orchestration\n\
.loom/state.json\n\
.loom/worktrees/\n\
.loom/*.log\n\
.loom/*.sock\n";

    fs::write(project_path.join(".gitignore"), gitignore_content)
        .map_err(|e| format!("Failed to create .gitignore: {e}"))?;

    // Commit initial files
    Command::new("git")
        .args(["add", "."])
        .current_dir(&project_path)
        .output()
        .map_err(|e| format!("Failed to git add: {e}"))?;

    Command::new("git")
        .args(["commit", "-m", "Initial commit"])
        .current_dir(&project_path)
        .output()
        .map_err(|e| format!("Failed to git commit: {e}"))?;

    Ok(project_path.display().to_string())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn create_github_repository(
    project_path: &str,
    name: &str,
    description: Option<String>,
    is_private: bool,
) -> Result<String, String> {
    let project = Path::new(project_path);

    // Check if project directory exists
    if !project.exists() {
        return Err(format!("Project directory does not exist: {project_path}"));
    }

    // Check if gh CLI is available
    let which_output = Command::new("which")
        .arg("gh")
        .output()
        .map_err(|e| format!("Failed to check for gh CLI: {e}"))?;

    if !which_output.status.success() {
        return Err(
            "GitHub CLI (gh) is not installed. Please install it from https://cli.github.com/"
                .to_string(),
        );
    }

    // Check if gh is authenticated
    let auth_status = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .map_err(|e| format!("Failed to check gh auth status: {e}"))?;

    if !auth_status.status.success() {
        return Err(
            "Not authenticated with GitHub CLI. Please run 'gh auth login' first.".to_string()
        );
    }

    // Build gh repo create command
    let mut args = vec!["repo", "create", name];

    // Add description if provided
    let desc_arg;
    if let Some(ref desc) = description {
        desc_arg = format!("--description={desc}");
        args.push(&desc_arg);
    }

    // Add visibility flag
    if is_private {
        args.push("--private");
    } else {
        args.push("--public");
    }

    // Add --source flag to create from current directory
    args.push("--source=.");

    // Create repository
    let create_output = Command::new("gh")
        .args(&args)
        .current_dir(project)
        .output()
        .map_err(|e| format!("Failed to create GitHub repository: {e}"))?;

    if !create_output.status.success() {
        let stderr = String::from_utf8_lossy(&create_output.stderr);
        return Err(format!("Failed to create repository: {stderr}"));
    }

    // Get the repository URL from gh CLI
    let repo_output = Command::new("gh")
        .args(["repo", "view", "--json", "url", "-q", ".url"])
        .current_dir(project)
        .output()
        .map_err(|e| format!("Failed to get repository URL: {e}"))?;

    if repo_output.status.success() {
        Ok(String::from_utf8_lossy(&repo_output.stdout)
            .trim()
            .to_string())
    } else {
        Ok(format!("Repository created: {name}"))
    }
}
