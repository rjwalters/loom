//! Role validation module for Loom daemon
//!
//! This module validates that all configured roles have their dependencies
//! properly configured, preventing silent failures where work gets stuck.
//!
//! # Role Dependencies
//!
//! Roles have dependencies on other roles to handle specific label transitions:
//!
//! | Role | Creates Label | Requires Role | To Handle |
//! |------|---------------|---------------|-----------|
//! | Champion | `loom:changes-requested` | Doctor | Address PR feedback |
//! | Builder | `loom:review-requested` | Judge | Review PRs |
//! | Judge | `loom:pr` | Champion | Auto-merge approved PRs |
//! | Judge | `loom:changes-requested` | Doctor | Address feedback |
//!
//! # Usage
//!
//! ```ignore
//! use loom_daemon::role_validation::{validate_role_completeness, ValidationMode};
//!
//! let config_json = r#"{"terminals": [...]}"#;
//! let result = validate_role_completeness(config_json, ValidationMode::Warn);
//!
//! for warning in &result.warnings {
//!     println!("Warning: {} -> {}: {}",
//!         warning.role, warning.missing_dependency, warning.message);
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Validation mode for role completeness checks
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValidationMode {
    /// Skip validation entirely
    Ignore,
    /// Log warnings but continue (default)
    #[default]
    Warn,
    /// Fail startup if any warnings
    Strict,
}

impl std::str::FromStr for ValidationMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "ignore" => Ok(Self::Ignore),
            "warn" => Ok(Self::Warn),
            "strict" => Ok(Self::Strict),
            _ => Err(format!("Unknown validation mode: {s}")),
        }
    }
}

/// A warning about a missing role dependency
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleWarning {
    /// The role that has the dependency
    pub role: String,
    /// The missing dependency role
    pub missing_dependency: String,
    /// Human-readable message explaining the issue
    pub message: String,
}

/// Result of role completeness validation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    /// Whether the validation passed (no errors)
    pub valid: bool,
    /// List of configured roles found
    pub configured_roles: Vec<String>,
    /// Warnings about missing dependencies
    pub warnings: Vec<RoleWarning>,
    /// Errors that prevent startup
    pub errors: Vec<String>,
}

/// Role dependency definition
struct RoleDependency {
    role: &'static str,
    dependency: &'static str,
    message: &'static str,
}

/// All known role dependencies
const ROLE_DEPENDENCIES: &[RoleDependency] = &[
    RoleDependency {
        role: "champion",
        dependency: "doctor",
        message: "Champion can set loom:changes-requested, but Doctor is not configured to handle it",
    },
    RoleDependency {
        role: "builder",
        dependency: "judge",
        message: "Builder creates PRs with loom:review-requested, but Judge is not configured to review them",
    },
    RoleDependency {
        role: "judge",
        dependency: "doctor",
        message: "Judge can request changes with loom:changes-requested, but Doctor is not configured to address them",
    },
    RoleDependency {
        role: "judge",
        dependency: "champion",
        message: "Judge approves PRs with loom:pr, but Champion is not configured to merge them",
    },
    RoleDependency {
        role: "curator",
        dependency: "champion",
        message: "Curator marks issues loom:curated, but no Champion configured to auto-promote them",
    },
];

/// Minimal config structure for extracting roles
#[derive(Debug, Deserialize)]
struct LoomConfig {
    terminals: Option<Vec<Terminal>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Terminal {
    role_config: Option<RoleConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoleConfig {
    role_file: Option<String>,
}

/// Extract role names from a config JSON string
///
/// Parses the config and extracts role names from terminal configurations.
/// Role names are derived from roleFile values (e.g., "judge.md" -> "judge").
pub fn extract_roles_from_config(config_json: &str) -> Result<Vec<String>, String> {
    let config: LoomConfig =
        serde_json::from_str(config_json).map_err(|e| format!("Failed to parse config: {e}"))?;

    let mut roles = Vec::new();

    if let Some(terminals) = config.terminals {
        for terminal in terminals {
            if let Some(role_config) = terminal.role_config {
                if let Some(role_file) = role_config.role_file {
                    // Extract role name from filename (e.g., "judge.md" -> "judge")
                    let role_name = role_file.trim_end_matches(".md").to_string();
                    if !role_name.is_empty() {
                        roles.push(role_name);
                    }
                }
            }
        }
    }

    roles.sort();
    roles.dedup();

    Ok(roles)
}

/// Validate that all role dependencies are satisfied
///
/// # Arguments
///
/// * `config_json` - JSON string containing the Loom config
/// * `mode` - Validation mode (Ignore, Warn, or Strict)
///
/// # Returns
///
/// Validation result with any warnings or errors found
pub fn validate_role_completeness(config_json: &str, mode: ValidationMode) -> ValidationResult {
    if mode == ValidationMode::Ignore {
        return ValidationResult {
            valid: true,
            configured_roles: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
        };
    }

    let configured_roles = match extract_roles_from_config(config_json) {
        Ok(roles) => roles,
        Err(e) => {
            return ValidationResult {
                valid: false,
                configured_roles: Vec::new(),
                warnings: Vec::new(),
                errors: vec![e],
            };
        }
    };

    let role_set: HashSet<&str> = configured_roles.iter().map(|s| s.as_str()).collect();
    let mut warnings = Vec::new();

    // Check each dependency
    for dep in ROLE_DEPENDENCIES {
        if role_set.contains(dep.role) && !role_set.contains(dep.dependency) {
            warnings.push(RoleWarning {
                role: dep.role.to_string(),
                missing_dependency: dep.dependency.to_string(),
                message: dep.message.to_string(),
            });
        }
    }

    ValidationResult {
        valid: true,
        configured_roles,
        warnings,
        errors: Vec::new(),
    }
}

/// Validate role completeness from a config file path
///
/// Convenience function that reads the config file and validates it.
pub fn validate_from_file(
    config_path: &std::path::Path,
    mode: ValidationMode,
) -> Result<ValidationResult, String> {
    let config_json =
        std::fs::read_to_string(config_path).map_err(|e| format!("Failed to read config: {e}"))?;

    Ok(validate_role_completeness(&config_json, mode))
}

/// Format validation result for console output
pub fn format_validation_result(result: &ValidationResult, verbose: bool) -> String {
    let mut output = String::new();

    if verbose {
        output.push_str(&format!(
            "Configured roles: {}\n",
            result.configured_roles.join(", ")
        ));
    }

    if !result.warnings.is_empty() {
        output.push_str("\nROLE CONFIGURATION WARNINGS:\n");
        for warning in &result.warnings {
            output.push_str(&format!(
                "  - {} -> {}: {}\n",
                warning.role.to_uppercase(),
                warning.missing_dependency.to_uppercase(),
                warning.message
            ));
        }
        output.push_str("\nThe daemon will continue, but some workflows may get stuck.\n");
        output.push_str("Consider adding the missing roles to .loom/config.json\n");
    } else if verbose {
        output.push_str("All role dependencies are satisfied.\n");
    }

    if !result.errors.is_empty() {
        output.push_str("\nROLE CONFIGURATION ERRORS:\n");
        for error in &result.errors {
            output.push_str(&format!("  - {error}\n"));
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_roles_from_config() {
        let config = r#"{
            "terminals": [
                {
                    "id": "terminal-1",
                    "name": "Judge",
                    "roleConfig": {
                        "roleFile": "judge.md"
                    }
                },
                {
                    "id": "terminal-2",
                    "name": "Builder",
                    "roleConfig": {
                        "roleFile": "builder.md"
                    }
                }
            ]
        }"#;

        let roles = extract_roles_from_config(config).unwrap();
        assert_eq!(roles, vec!["builder", "judge"]);
    }

    #[test]
    fn test_validate_missing_doctor() {
        let config = r#"{
            "terminals": [
                {
                    "id": "terminal-1",
                    "name": "Judge",
                    "roleConfig": {
                        "roleFile": "judge.md"
                    }
                },
                {
                    "id": "terminal-2",
                    "name": "Champion",
                    "roleConfig": {
                        "roleFile": "champion.md"
                    }
                }
            ]
        }"#;

        let result = validate_role_completeness(config, ValidationMode::Warn);

        assert!(result.valid);
        assert!(!result.warnings.is_empty());

        // Should warn about missing doctor for both judge and champion
        let doctor_warnings: Vec<_> = result
            .warnings
            .iter()
            .filter(|w| w.missing_dependency == "doctor")
            .collect();
        assert_eq!(doctor_warnings.len(), 2);
    }

    #[test]
    fn test_validate_all_dependencies_satisfied() {
        let config = r#"{
            "terminals": [
                {"roleConfig": {"roleFile": "judge.md"}},
                {"roleConfig": {"roleFile": "champion.md"}},
                {"roleConfig": {"roleFile": "doctor.md"}},
                {"roleConfig": {"roleFile": "builder.md"}},
                {"roleConfig": {"roleFile": "curator.md"}}
            ]
        }"#;

        let result = validate_role_completeness(config, ValidationMode::Warn);

        assert!(result.valid);
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn test_validate_ignore_mode() {
        let config = r#"{"terminals": []}"#;

        let result = validate_role_completeness(config, ValidationMode::Ignore);

        assert!(result.valid);
        assert!(result.configured_roles.is_empty());
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn test_format_validation_result() {
        let result = ValidationResult {
            valid: true,
            configured_roles: vec!["judge".to_string(), "champion".to_string()],
            warnings: vec![RoleWarning {
                role: "champion".to_string(),
                missing_dependency: "doctor".to_string(),
                message: "Test warning".to_string(),
            }],
            errors: Vec::new(),
        };

        let output = format_validation_result(&result, true);
        assert!(output.contains("ROLE CONFIGURATION WARNINGS"));
        assert!(output.contains("CHAMPION"));
        assert!(output.contains("DOCTOR"));
    }
}
