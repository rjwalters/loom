//! Test output parser for extracting test results from terminal output.
//!
//! This module provides functions to parse test results from various test runners
//! including Jest, pytest, cargo test, go test, and npm test.

use super::models::{LintResults, TestResults};
use regex::Regex;

/// Parse test results from terminal output.
///
/// Attempts to detect and parse output from common test runners:
/// - Jest (JavaScript/TypeScript)
/// - pytest (Python)
/// - cargo test (Rust)
/// - go test (Go)
/// - npm test (generic, often wraps other runners)
///
/// Returns `None` if no recognizable test output is found.
pub fn parse_test_results(output: &str) -> Option<TestResults> {
    // Try each parser in order of specificity
    parse_jest(output)
        .or_else(|| parse_pytest(output))
        .or_else(|| parse_cargo_test(output))
        .or_else(|| parse_go_test(output))
}

/// Parse Jest test output.
///
/// Jest format examples:
/// ```text
/// Tests:       42 passed, 3 failed, 2 skipped, 47 total
/// Tests:       10 passed, 10 total
/// ```
fn parse_jest(output: &str) -> Option<TestResults> {
    // Match: Tests: N passed, M failed, O skipped, P total
    // or: Tests: N passed, P total
    let re = Regex::new(
        r"Tests:\s+(?:(\d+)\s+passed)?(?:,\s*)?(?:(\d+)\s+failed)?(?:,\s*)?(?:(\d+)\s+skipped)?",
    )
    .ok()?;

    if let Some(caps) = re.captures(output) {
        let passed = caps
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let failed = caps
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let skipped = caps
            .get(3)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);

        // Only return if we found at least one number
        if passed > 0 || failed > 0 || skipped > 0 {
            return Some(TestResults {
                passed,
                failed,
                skipped,
                runner: Some("jest".to_string()),
            });
        }
    }

    None
}

/// Parse pytest output.
///
/// pytest format examples:
/// ```text
/// ====== 42 passed, 3 failed, 2 skipped in 1.23s ======
/// ====== 10 passed in 0.50s ======
/// = 5 passed, 1 warning in 0.10s =
/// ```
fn parse_pytest(output: &str) -> Option<TestResults> {
    // Match: N passed, M failed, O skipped in Xs
    let re = Regex::new(
        r"=+\s*(?:(\d+)\s+passed)?(?:,\s*)?(?:(\d+)\s+failed)?(?:,\s*)?(?:(\d+)\s+skipped)?(?:,\s*\d+\s+\w+)*\s+in\s+[\d.]+s\s*=+",
    )
    .ok()?;

    if let Some(caps) = re.captures(output) {
        let passed = caps
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let failed = caps
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let skipped = caps
            .get(3)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);

        if passed > 0 || failed > 0 || skipped > 0 {
            return Some(TestResults {
                passed,
                failed,
                skipped,
                runner: Some("pytest".to_string()),
            });
        }
    }

    None
}

/// Parse cargo test output.
///
/// cargo test format examples:
/// ```text
/// test result: ok. 42 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out
/// test result: FAILED. 5 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out
/// ```
fn parse_cargo_test(output: &str) -> Option<TestResults> {
    // Match: test result: ok/FAILED. N passed; M failed; O ignored
    let re = Regex::new(
        r"test result:\s+(?:ok|FAILED)\.\s+(\d+)\s+passed;\s+(\d+)\s+failed;\s+(\d+)\s+ignored",
    )
    .ok()?;

    if let Some(caps) = re.captures(output) {
        let passed: i32 = caps.get(1)?.as_str().parse().ok()?;
        let failed: i32 = caps.get(2)?.as_str().parse().ok()?;
        let skipped: i32 = caps.get(3)?.as_str().parse().ok()?;

        return Some(TestResults {
            passed,
            failed,
            skipped,
            runner: Some("cargo".to_string()),
        });
    }

    None
}

/// Parse go test output.
///
/// go test format examples:
/// ```text
/// ok      github.com/user/pkg    1.234s
/// FAIL    github.com/user/pkg    1.234s
/// --- PASS: TestFoo (0.00s)
/// --- FAIL: TestBar (0.00s)
/// --- SKIP: TestBaz (0.00s)
/// ```
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn parse_go_test(output: &str) -> Option<TestResults> {
    // Count individual test results
    let pass_re = Regex::new(r"---\s+PASS:").ok()?;
    let fail_re = Regex::new(r"---\s+FAIL:").ok()?;
    let skip_re = Regex::new(r"---\s+SKIP:").ok()?;

    let passed = pass_re.find_iter(output).count() as i32;
    let failed = fail_re.find_iter(output).count() as i32;
    let skipped = skip_re.find_iter(output).count() as i32;

    // Only return if we found at least one test result indicator
    if passed > 0 || failed > 0 || skipped > 0 {
        return Some(TestResults {
            passed,
            failed,
            skipped,
            runner: Some("go".to_string()),
        });
    }

    // Also check for summary lines
    let ok_count = Regex::new(r"^ok\s+").ok()?.find_iter(output).count() as i32;
    let fail_count = Regex::new(r"^FAIL\s+").ok()?.find_iter(output).count() as i32;

    if ok_count > 0 || fail_count > 0 {
        // In this case, we can't get individual test counts, but we know something ran
        return Some(TestResults {
            passed: ok_count,
            failed: fail_count,
            skipped: 0,
            runner: Some("go".to_string()),
        });
    }

    None
}

/// Parse lint results from terminal output.
///
/// Detects output from common linters:
/// - `ESLint`
/// - cargo clippy (Rust)
/// - rustfmt
/// - Prettier
/// - Black (Python)
pub fn parse_lint_results(output: &str) -> Option<LintResults> {
    parse_eslint(output)
        .or_else(|| parse_clippy(output))
        .or_else(|| parse_rustfmt(output))
        .or_else(|| parse_prettier(output))
        .or_else(|| parse_black(output))
}

/// Parse `ESLint` output.
///
/// `ESLint` format examples:
/// ```text
/// 5 problems (3 errors, 2 warnings)
/// ```
fn parse_eslint(output: &str) -> Option<LintResults> {
    let re = Regex::new(r"(\d+)\s+problems?\s*\((\d+)\s+errors?,\s*(\d+)\s+warnings?\)").ok()?;

    if let Some(caps) = re.captures(output) {
        let errors: i32 = caps.get(2)?.as_str().parse().ok()?;

        return Some(LintResults {
            lint_errors: errors,
            format_errors: 0,
        });
    }

    None
}

/// Parse cargo clippy output.
///
/// clippy format examples:
/// ```text
/// warning: `project` (lib) generated 5 warnings
/// warning: `project` (lib) generated 3 warnings (2 duplicates)
/// error: could not compile `project` due to 2 previous errors
/// warning: unused variable: `x`
/// error[E0425]: cannot find value `x` in this scope
/// ```
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn parse_clippy(output: &str) -> Option<LintResults> {
    // Count explicit warning and error patterns
    let warning_summary_re = Regex::new(r"generated\s+(\d+)\s+warnings?").ok()?;
    let error_summary_re = Regex::new(r"due to\s+(\d+)\s+previous\s+errors?").ok()?;

    // Try to get counts from summary lines first
    let mut warnings = 0i32;
    let mut errors = 0i32;

    for cap in warning_summary_re.captures_iter(output) {
        if let Some(m) = cap.get(1) {
            warnings = warnings.saturating_add(m.as_str().parse().unwrap_or(0));
        }
    }

    for cap in error_summary_re.captures_iter(output) {
        if let Some(m) = cap.get(1) {
            errors = errors.saturating_add(m.as_str().parse().unwrap_or(0));
        }
    }

    // If no summary found, count individual warning/error lines
    // Use string matching instead of regex look-ahead (not supported by regex crate)
    if warnings == 0 && errors == 0 {
        warnings = output
            .lines()
            .filter(|line| {
                line.starts_with("warning:")
                    && !line.contains("generated")
                    && !line.contains("(lib)")
                    && !line.contains("(bin)")
            })
            .count() as i32;

        let error_line_re = Regex::new(r"^error(?:\[E\d+\])?:").ok()?;
        errors = output
            .lines()
            .filter(|line| error_line_re.is_match(line))
            .count() as i32;
    }

    if warnings > 0 || errors > 0 {
        return Some(LintResults {
            lint_errors: errors.saturating_add(warnings),
            format_errors: 0,
        });
    }

    None
}

/// Parse rustfmt output.
///
/// rustfmt typically outputs file paths for files that need formatting.
/// When files need formatting, rustfmt exits with code 1.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn parse_rustfmt(output: &str) -> Option<LintResults> {
    // Check for rustfmt diff output
    if output.contains("Diff in") || output.contains("rustfmt") && output.contains("error") {
        // Count "Diff in" occurrences as format errors
        let diff_count = output.matches("Diff in").count() as i32;
        if diff_count > 0 {
            return Some(LintResults {
                lint_errors: 0,
                format_errors: diff_count,
            });
        }
    }

    None
}

/// Parse Prettier output.
///
/// Prettier format examples when files need formatting:
/// ```text
/// Checking formatting...
/// [warn] src/file.ts
/// [warn] Code style issues found in 3 files. Run Prettier to fix.
/// ```
fn parse_prettier(output: &str) -> Option<LintResults> {
    let re = Regex::new(r"Code style issues found in (\d+) files?").ok()?;

    if let Some(caps) = re.captures(output) {
        let count: i32 = caps.get(1)?.as_str().parse().ok()?;
        return Some(LintResults {
            lint_errors: 0,
            format_errors: count,
        });
    }

    None
}

/// Parse Black (Python formatter) output.
///
/// Black format examples:
/// ```text
/// would reformat src/file.py
/// Oh no! 3 files would be reformatted.
/// All done! 10 files left unchanged.
/// ```
fn parse_black(output: &str) -> Option<LintResults> {
    let re = Regex::new(r"(\d+)\s+files?\s+would be reformatted").ok()?;

    if let Some(caps) = re.captures(output) {
        let count: i32 = caps.get(1)?.as_str().parse().ok()?;
        return Some(LintResults {
            lint_errors: 0,
            format_errors: count,
        });
    }

    None
}

/// Detect if output indicates a build failure.
///
/// Returns `Some(false)` for detected failures, `Some(true)` for detected success,
/// or `None` if build status cannot be determined.
pub fn parse_build_status(output: &str) -> Option<bool> {
    // Common build failure indicators
    let failure_patterns = [
        "error: could not compile",
        "Build failed",
        "build failed",
        "ERROR: Build failed",
        "FAILED",
        "npm ERR!",
        "error: build failed",
        "error[E", // Rust compiler error
        "Error: ", // TypeScript/Node errors
        "BUILD FAILURE",
        "BUILD FAILED",
    ];

    for pattern in &failure_patterns {
        if output.contains(pattern) {
            return Some(false);
        }
    }

    // Common build success indicators
    let success_patterns = [
        "Finished release",
        "Finished dev",
        "Finished debug",
        "Build succeeded",
        "build succeeded",
        "Successfully compiled",
        "Compiled successfully",
        "BUILD SUCCESS",
        "BUILD SUCCESSFUL",
    ];

    for pattern in &success_patterns {
        if output.contains(pattern) {
            return Some(true);
        }
    }

    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_jest_full() {
        let output = "Tests:       42 passed, 3 failed, 2 skipped, 47 total";
        let result = parse_jest(output).unwrap();
        assert_eq!(result.passed, 42);
        assert_eq!(result.failed, 3);
        assert_eq!(result.skipped, 2);
        assert_eq!(result.runner, Some("jest".to_string()));
    }

    #[test]
    fn test_parse_jest_passed_only() {
        let output = "Tests:       10 passed, 10 total";
        let result = parse_jest(output).unwrap();
        assert_eq!(result.passed, 10);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn test_parse_pytest() {
        let output = "====== 42 passed, 3 failed, 2 skipped in 1.23s ======";
        let result = parse_pytest(output).unwrap();
        assert_eq!(result.passed, 42);
        assert_eq!(result.failed, 3);
        assert_eq!(result.skipped, 2);
        assert_eq!(result.runner, Some("pytest".to_string()));
    }

    #[test]
    fn test_parse_pytest_passed_only() {
        let output = "======== 10 passed in 0.50s ========";
        let result = parse_pytest(output).unwrap();
        assert_eq!(result.passed, 10);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn test_parse_cargo_test_success() {
        let output = "test result: ok. 42 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out";
        let result = parse_cargo_test(output).unwrap();
        assert_eq!(result.passed, 42);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 2);
        assert_eq!(result.runner, Some("cargo".to_string()));
    }

    #[test]
    fn test_parse_cargo_test_failure() {
        let output =
            "test result: FAILED. 5 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out";
        let result = parse_cargo_test(output).unwrap();
        assert_eq!(result.passed, 5);
        assert_eq!(result.failed, 3);
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn test_parse_go_test() {
        let output = r"
--- PASS: TestFoo (0.00s)
--- PASS: TestBar (0.00s)
--- FAIL: TestBaz (0.00s)
--- SKIP: TestQux (0.00s)
PASS
";
        let result = parse_go_test(output).unwrap();
        assert_eq!(result.passed, 2);
        assert_eq!(result.failed, 1);
        assert_eq!(result.skipped, 1);
        assert_eq!(result.runner, Some("go".to_string()));
    }

    #[test]
    fn test_parse_eslint() {
        let output = "  5 problems (3 errors, 2 warnings)";
        let result = parse_eslint(output).unwrap();
        assert_eq!(result.lint_errors, 3);
        assert_eq!(result.format_errors, 0);
    }

    #[test]
    fn test_parse_clippy_summary() {
        let output = r"
    Checking loom-daemon v0.1.0
warning: `loom-daemon` (lib) generated 5 warnings
    Finished `dev` profile [unoptimized + debuginfo] target
";
        let result = parse_clippy(output).unwrap();
        assert_eq!(result.lint_errors, 5);
        assert_eq!(result.format_errors, 0);
    }

    #[test]
    fn test_parse_clippy_errors() {
        let output = r"
error[E0425]: cannot find value `x` in this scope
  --> src/main.rs:10:5
   |
10 |     x
   |     ^ not found in this scope

error: could not compile `project` due to 2 previous errors
";
        let result = parse_clippy(output).unwrap();
        assert_eq!(result.lint_errors, 2);
        assert_eq!(result.format_errors, 0);
    }

    #[test]
    fn test_parse_clippy_warnings_and_errors() {
        let output = r"
warning: unused variable: `x`
  --> src/main.rs:5:9
   |
5  |     let x = 5;
   |         ^ help: if this is intentional, prefix it with an underscore: `_x`

warning: `myproject` (lib) generated 3 warnings
error: could not compile `myproject` due to 1 previous error
";
        let result = parse_clippy(output).unwrap();
        // 3 warnings + 1 error = 4 total lint errors
        assert_eq!(result.lint_errors, 4);
        assert_eq!(result.format_errors, 0);
    }

    #[test]
    fn test_parse_prettier() {
        let output = "[warn] Code style issues found in 3 files. Run Prettier to fix.";
        let result = parse_prettier(output).unwrap();
        assert_eq!(result.lint_errors, 0);
        assert_eq!(result.format_errors, 3);
    }

    #[test]
    fn test_parse_black() {
        let output = "Oh no! 3 files would be reformatted.";
        let result = parse_black(output).unwrap();
        assert_eq!(result.lint_errors, 0);
        assert_eq!(result.format_errors, 3);
    }

    #[test]
    fn test_parse_build_status_failure() {
        assert_eq!(parse_build_status("error: could not compile `myproject`"), Some(false));
        assert_eq!(parse_build_status("npm ERR! Build failed"), Some(false));
        assert_eq!(parse_build_status("error[E0425]: cannot find value"), Some(false));
    }

    #[test]
    fn test_parse_build_status_success() {
        assert_eq!(parse_build_status("   Finished release [optimized] target"), Some(true));
        assert_eq!(parse_build_status("Compiled successfully in 2.3s"), Some(true));
    }

    #[test]
    fn test_parse_build_status_unknown() {
        assert_eq!(parse_build_status("some random output"), None);
    }

    #[test]
    fn test_parse_test_results_auto_detect() {
        // Should detect Jest
        let jest_output = "Tests:       5 passed, 1 failed, 6 total";
        let result = parse_test_results(jest_output).unwrap();
        assert_eq!(result.runner, Some("jest".to_string()));

        // Should detect pytest
        let pytest_output = "== 10 passed, 2 failed in 1.0s ==";
        let result = parse_test_results(pytest_output).unwrap();
        assert_eq!(result.runner, Some("pytest".to_string()));

        // Should detect cargo
        let cargo_output = "test result: ok. 15 passed; 0 failed; 1 ignored; 0 measured";
        let result = parse_test_results(cargo_output).unwrap();
        assert_eq!(result.runner, Some("cargo".to_string()));
    }
}
