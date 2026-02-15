//! File system operations for Loom initialization
//!
//! Provides copy, merge, clean, and verify operations for directories.
//! These operations support both fresh installations and reinstallations
//! with various merge strategies.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use super::templates::{substitute_template_variables, LoomMetadata};
use super::InitReport;

/// Template context for verification
///
/// When source files contain template variables (e.g., `{{REPO_OWNER}}`), the installed
/// files will have these substituted with actual values. Verification must apply the same
/// substitution to source content before comparing, otherwise every template-substituted
/// file reports a false-positive content mismatch.
pub struct TemplateContext {
    pub repo_owner: Option<String>,
    pub repo_name: Option<String>,
    pub loom_metadata: LoomMetadata,
}

/// Copy directory recursively
///
/// Creates the destination directory and copies all files and subdirectories.
#[cfg(test)]
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

/// Merge directory recursively
///
/// Copies files from src to dst, but only if they don't already exist in dst.
/// This allows merging new files from defaults while preserving existing customizations.
#[cfg(test)]
pub fn merge_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            // Recursively merge subdirectories
            merge_dir_recursive(&src_path, &dst_path)?;
        } else if !dst_path.exists() {
            // Only copy if destination file doesn't exist
            fs::copy(&src_path, &dst_path)?;
        }
        // If dst_path exists, skip (preserve existing file)
    }

    Ok(())
}

/// Copy directory recursively with reporting
///
/// Like `copy_dir_recursive` but tracks which files were added.
pub fn copy_dir_with_report(
    src: &Path,
    dst: &Path,
    prefix: &str,
    report: &mut InitReport,
) -> io::Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);
        let rel_path = format!("{}/{}", prefix, file_name.to_string_lossy());

        if file_type.is_dir() {
            copy_dir_with_report(&src_path, &dst_path, &rel_path, report)?;
        } else {
            let existed = dst_path.exists();
            fs::copy(&src_path, &dst_path)?;
            if existed {
                report.updated.push(rel_path);
            } else {
                report.added.push(rel_path);
            }
        }
    }

    Ok(())
}

/// Merge directory recursively with reporting
///
/// Like `merge_dir_recursive` but tracks which files were added vs preserved.
pub fn merge_dir_with_report(
    src: &Path,
    dst: &Path,
    prefix: &str,
    report: &mut InitReport,
) -> io::Result<()> {
    fs::create_dir_all(dst)?;

    // Collect files in source for comparison
    let src_files: HashSet<_> = fs::read_dir(src)?
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name())
        .collect();

    // Check for files in dst that aren't in src (custom files)
    if dst.exists() {
        for entry in fs::read_dir(dst)? {
            let entry = entry?;
            let file_name = entry.file_name();
            if !src_files.contains(&file_name) {
                let rel_path = format!("{}/{}", prefix, file_name.to_string_lossy());
                // This is a custom file not in defaults - preserved
                if entry.file_type()?.is_file() {
                    report.preserved.push(rel_path);
                }
            }
        }
    }

    // Process source files
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);
        let rel_path = format!("{}/{}", prefix, file_name.to_string_lossy());

        if file_type.is_dir() {
            merge_dir_with_report(&src_path, &dst_path, &rel_path, report)?;
        } else if dst_path.exists() {
            // File exists - preserve it (don't overwrite)
            report.preserved.push(rel_path);
        } else {
            // File doesn't exist - add it
            fs::copy(&src_path, &dst_path)?;
            report.added.push(rel_path);
        }
    }

    Ok(())
}

/// Clean a managed directory by removing all files before re-copying from defaults
///
/// Removes all files (and subdirectories) in the destination directory, recording
/// each removed file in `report.removed`. The directory itself is preserved (it gets
/// repopulated by the subsequent copy step). This ensures stale files that no longer
/// exist in defaults are cleaned up on reinstall.
pub fn clean_managed_dir(dst: &Path, prefix: &str, report: &mut InitReport) -> io::Result<()> {
    if !dst.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(dst)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let file_name = entry.file_name();
        let entry_path = entry.path();
        let rel_path = format!("{}/{}", prefix, file_name.to_string_lossy());

        if file_type.is_dir() {
            clean_managed_dir(&entry_path, &rel_path, report)?;
            // Use remove_dir_all for robustness: handles unexpected files
            // (e.g., .DS_Store, git metadata) that the recursive clean may miss
            fs::remove_dir_all(&entry_path)?;
        } else {
            fs::remove_file(&entry_path)?;
            report.removed.push(rel_path);
        }
    }

    Ok(())
}

/// Force-merge directory recursively with reporting
///
/// Like `merge_dir_with_report` but OVERWRITES files from defaults while
/// still preserving custom files (files in dst that don't exist in src).
/// This is used for reinstallation to update Loom files while keeping
/// project-specific customizations.
pub fn force_merge_dir_with_report(
    src: &Path,
    dst: &Path,
    prefix: &str,
    report: &mut InitReport,
) -> io::Result<()> {
    fs::create_dir_all(dst)?;

    // Collect files in source for comparison
    let src_files: HashSet<_> = fs::read_dir(src)?
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name())
        .collect();

    // Check for files in dst that aren't in src (custom files) - these are preserved
    if dst.exists() {
        for entry in fs::read_dir(dst)? {
            let entry = entry?;
            let file_name = entry.file_name();
            if !src_files.contains(&file_name) {
                let rel_path = format!("{}/{}", prefix, file_name.to_string_lossy());
                // This is a custom file not in defaults - preserved
                if entry.file_type()?.is_file() {
                    report.preserved.push(rel_path);
                }
            }
        }
    }

    // Process source files - overwrite existing files from defaults
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);
        let rel_path = format!("{}/{}", prefix, file_name.to_string_lossy());

        if file_type.is_dir() {
            force_merge_dir_with_report(&src_path, &dst_path, &rel_path, report)?;
        } else {
            let existed = dst_path.exists();
            // Always copy from source (overwrite if exists)
            fs::copy(&src_path, &dst_path)?;
            if existed {
                report.updated.push(rel_path);
            } else {
                report.added.push(rel_path);
            }
        }
    }

    Ok(())
}

/// Verify that copied files match their source after installation
///
/// Compares each file in the source directory with the corresponding file in the
/// destination directory. Files that don't match are added to the report's
/// `verification_failures` list.
///
/// If `template_ctx` is provided, source files containing template variables (`{{`)
/// are substituted before comparison so that template-expanded installed files
/// don't produce false-positive mismatches.
pub fn verify_copied_files(
    src: &Path,
    dst: &Path,
    prefix: &str,
    report: &mut InitReport,
    template_ctx: Option<&TemplateContext>,
) {
    if !src.exists() || !dst.exists() {
        return;
    }

    let Ok(entries) = fs::read_dir(src) else {
        return;
    };

    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);
        let rel_path = format!("{}/{}", prefix, file_name.to_string_lossy());

        if file_type.is_dir() {
            verify_copied_files(&src_path, &dst_path, &rel_path, report, template_ctx);
        } else if dst_path.exists() {
            // Compare file contents
            let src_contents = fs::read(&src_path);
            let dst_contents = fs::read(&dst_path);

            match (src_contents, dst_contents) {
                (Ok(src_data), Ok(dst_data)) => {
                    // If template context is provided and source contains template
                    // variables, apply substitution before comparing
                    let effective_src = if let Some(ctx) = template_ctx {
                        if let Ok(text) = std::str::from_utf8(&src_data) {
                            if text.contains("{{") {
                                substitute_template_variables(
                                    text,
                                    ctx.repo_owner.as_deref(),
                                    ctx.repo_name.as_deref(),
                                    &ctx.loom_metadata,
                                )
                                .into_bytes()
                            } else {
                                src_data
                            }
                        } else {
                            src_data
                        }
                    } else {
                        src_data
                    };

                    if effective_src != dst_data {
                        report.verification_failures.push(format!(
                            "{rel_path} (content mismatch: source {} bytes, installed {} bytes)",
                            effective_src.len(),
                            dst_data.len()
                        ));
                    }
                }
                (_, Err(e)) => {
                    report
                        .verification_failures
                        .push(format!("{rel_path} (cannot read installed file: {e})"));
                }
                (Err(e), _) => {
                    report
                        .verification_failures
                        .push(format!("{rel_path} (cannot read source file: {e})"));
                }
            }
        } else {
            report
                .verification_failures
                .push(format!("{rel_path} (missing from installation)"));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_copy_dir_recursive() {
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        // Create source structure
        fs::create_dir(&src).unwrap();
        fs::write(src.join("file1.txt"), "content1").unwrap();
        fs::create_dir(src.join("subdir")).unwrap();
        fs::write(src.join("subdir").join("file2.txt"), "content2").unwrap();

        // Copy recursively
        copy_dir_recursive(&src, &dst).unwrap();

        // Verify destination
        assert!(dst.exists());
        assert!(dst.join("file1.txt").exists());
        assert!(dst.join("subdir").exists());
        assert!(dst.join("subdir").join("file2.txt").exists());

        let content1 = fs::read_to_string(dst.join("file1.txt")).unwrap();
        assert_eq!(content1, "content1");

        let content2 = fs::read_to_string(dst.join("subdir").join("file2.txt")).unwrap();
        assert_eq!(content2, "content2");
    }

    #[test]
    fn test_merge_dir_recursive() {
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        // Create source structure
        fs::create_dir(&src).unwrap();
        fs::write(src.join("file1.txt"), "new content 1").unwrap();
        fs::write(src.join("file2.txt"), "new content 2").unwrap();
        fs::create_dir(src.join("subdir")).unwrap();
        fs::write(src.join("subdir").join("file3.txt"), "new content 3").unwrap();

        // Create destination with existing file
        fs::create_dir(&dst).unwrap();
        fs::write(dst.join("file1.txt"), "existing content").unwrap();
        fs::write(dst.join("existing.txt"), "preserve me").unwrap();

        // Merge directories
        merge_dir_recursive(&src, &dst).unwrap();

        // Verify: file1.txt should be preserved (not overwritten)
        let content1 = fs::read_to_string(dst.join("file1.txt")).unwrap();
        assert_eq!(content1, "existing content");

        // Verify: file2.txt should be copied (new file)
        assert!(dst.join("file2.txt").exists());
        let content2 = fs::read_to_string(dst.join("file2.txt")).unwrap();
        assert_eq!(content2, "new content 2");

        // Verify: subdir and file3.txt should be created
        assert!(dst.join("subdir").exists());
        assert!(dst.join("subdir").join("file3.txt").exists());
        let content3 = fs::read_to_string(dst.join("subdir").join("file3.txt")).unwrap();
        assert_eq!(content3, "new content 3");

        // Verify: existing.txt should still exist
        assert!(dst.join("existing.txt").exists());
        let existing = fs::read_to_string(dst.join("existing.txt")).unwrap();
        assert_eq!(existing, "preserve me");
    }

    #[test]
    fn test_force_merge_preserves_custom_files() {
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        // Create source (defaults) with some files
        fs::create_dir(&src).unwrap();
        fs::write(src.join("builder.md"), "new builder content").unwrap();
        fs::write(src.join("judge.md"), "new judge content").unwrap();

        // Create destination with existing files (some default, some custom)
        fs::create_dir(&dst).unwrap();
        fs::write(dst.join("builder.md"), "old builder content").unwrap();
        fs::write(dst.join("designer.md"), "custom designer content").unwrap(); // Custom file

        // Force-merge directories
        let mut report = super::super::InitReport::default();
        force_merge_dir_with_report(&src, &dst, "roles", &mut report).unwrap();

        // Verify: builder.md was UPDATED (overwritten from defaults)
        let builder = fs::read_to_string(dst.join("builder.md")).unwrap();
        assert_eq!(builder, "new builder content");

        // Verify: judge.md was ADDED (new file from defaults)
        let judge = fs::read_to_string(dst.join("judge.md")).unwrap();
        assert_eq!(judge, "new judge content");

        // Verify: designer.md was PRESERVED (custom file not in defaults)
        assert!(dst.join("designer.md").exists());
        let designer = fs::read_to_string(dst.join("designer.md")).unwrap();
        assert_eq!(designer, "custom designer content");

        // Verify report
        assert!(report.updated.contains(&"roles/builder.md".to_string()));
        assert!(report.added.contains(&"roles/judge.md".to_string()));
        assert!(report.preserved.contains(&"roles/designer.md".to_string()));
    }

    #[test]
    fn test_verify_copied_files_detects_mismatch() {
        // Post-copy verification should detect files that don't match their source
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        // Create matching file
        fs::write(src.join("good.sh"), "matching content").unwrap();
        fs::write(dst.join("good.sh"), "matching content").unwrap();

        // Create mismatched file
        fs::write(src.join("bad.sh"), "source content").unwrap();
        fs::write(dst.join("bad.sh"), "different content").unwrap();

        // Create missing file (in source but not in destination)
        fs::write(src.join("missing.sh"), "should exist").unwrap();

        let mut report = super::super::InitReport::default();
        verify_copied_files(&src, &dst, "scripts", &mut report, None);

        // Should have 2 failures: mismatch + missing
        assert_eq!(report.verification_failures.len(), 2);
        assert!(report
            .verification_failures
            .iter()
            .any(|f| f.contains("bad.sh")));
        assert!(report
            .verification_failures
            .iter()
            .any(|f| f.contains("missing.sh")));
    }

    #[test]
    fn test_verify_copied_files_passes_on_match() {
        // Post-copy verification should pass when all files match
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        fs::write(src.join("a.sh"), "content a").unwrap();
        fs::write(dst.join("a.sh"), "content a").unwrap();
        fs::write(src.join("b.sh"), "content b").unwrap();
        fs::write(dst.join("b.sh"), "content b").unwrap();

        let mut report = super::super::InitReport::default();
        verify_copied_files(&src, &dst, "scripts", &mut report, None);

        assert!(report.verification_failures.is_empty());
    }

    #[test]
    fn test_verify_copied_files_with_template_substitution() {
        // Files with template variables should pass verification when the installed
        // file has the templates substituted with actual values (issue #1918)
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        // Source has template variables, destination has substituted values
        fs::write(
            src.join("workflow.yml"),
            "url: https://github.com/{{REPO_OWNER}}/{{REPO_NAME}}/discussions",
        )
        .unwrap();
        fs::write(dst.join("workflow.yml"), "url: https://github.com/myowner/myrepo/discussions")
            .unwrap();

        // Non-template file should still match normally
        fs::write(src.join("plain.txt"), "no templates here").unwrap();
        fs::write(dst.join("plain.txt"), "no templates here").unwrap();

        let ctx = TemplateContext {
            repo_owner: Some("myowner".to_string()),
            repo_name: Some("myrepo".to_string()),
            loom_metadata: LoomMetadata::default(),
        };

        let mut report = super::super::InitReport::default();
        verify_copied_files(&src, &dst, ".github", &mut report, Some(&ctx));

        assert!(
            report.verification_failures.is_empty(),
            "Template-substituted files should not produce verification failures, got: {:?}",
            report.verification_failures
        );
    }

    #[test]
    fn test_verify_copied_files_template_still_detects_real_mismatch() {
        // Even with template context, genuine content mismatches should be detected
        let temp_dir = TempDir::new().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        // Source has template, but destination has wrong substitution
        fs::write(
            src.join("workflow.yml"),
            "url: https://github.com/{{REPO_OWNER}}/{{REPO_NAME}}/discussions",
        )
        .unwrap();
        fs::write(
            dst.join("workflow.yml"),
            "url: https://github.com/wrong-owner/wrong-repo/discussions plus extra",
        )
        .unwrap();

        let ctx = TemplateContext {
            repo_owner: Some("myowner".to_string()),
            repo_name: Some("myrepo".to_string()),
            loom_metadata: LoomMetadata::default(),
        };

        let mut report = super::super::InitReport::default();
        verify_copied_files(&src, &dst, ".github", &mut report, Some(&ctx));

        assert_eq!(
            report.verification_failures.len(),
            1,
            "Genuine mismatch should still be detected even with template context"
        );
        assert!(report.verification_failures[0].contains("workflow.yml"));
    }
}
