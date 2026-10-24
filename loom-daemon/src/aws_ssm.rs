//! Reading one SSM parameter with the AWS CLI, as the host's own identity
//! (2AMLogic/2am#4367). Shared by every config block whose key refs may be
//! `ssm://` (`forge.gateway`, `forge.mirror`), so none depends on another.
//!
//! - **The identity is the instance role** unless the block names a profile:
//!   every credential selector in the daemon's environment
//!   ([`AWS_IDENTITY_ENV`]) is removed from the `aws` child, and the shared
//!   config and credentials files are pointed at `/dev/null`, so neither a
//!   worker's `AWS_PROFILE=sccache` nor a `default` profile in `~/.aws` can
//!   stand in for it.
//! - **A named profile** (`<block>.awsProfile`) is passed as
//!   `--profile=<name>` and must live in the default `~/.aws/config` /
//!   `~/.aws/credentials` of the daemon's user: the environment's
//!   `AWS_CONFIG_FILE` / `AWS_SHARED_CREDENTIALS_FILE` are removed too.
//! - **An invalid profile fails closed**: `ssm://` reads are refused rather
//!   than falling back to the default credential chain.
//! - Only names are on the command line, never a value; an error names the
//!   parameter, region, profile, exit code and the CLI's first stderr line,
//!   never stdout.

use std::time::Duration;

use serde_json::Value;

/// AWS credential selectors removed from the `aws` child.
pub const AWS_IDENTITY_ENV: &[&str] = &[
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_CONFIG_FILE",
    "AWS_ROLE_ARN",
    "AWS_ROLE_SESSION_NAME",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
];

/// The files pointed at `/dev/null` for an instance-role read.
pub const AWS_FILE_ENV: &[&str] = &["AWS_CONFIG_FILE", "AWS_SHARED_CREDENTIALS_FILE"];

/// Deadline for one SSM read.
pub const SSM_TIMEOUT: Duration = Duration::from_secs(30);

/// `<block>.awsProfile`, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AwsProfile {
    /// Unset: the instance role.
    #[default]
    Instance,
    /// A named profile in the default `~/.aws` files.
    Named(String),
    /// Set but unusable: `ssm://` reads are refused (fail closed).
    Invalid,
}

impl AwsProfile {
    /// The name, for a named profile.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Named(n) => Some(n),
            _ => None,
        }
    }
}

/// How an `ssm://` ref reaches AWS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwsAccess<'a> {
    /// The region (always explicit; `None` refuses the read).
    pub region: Option<&'a str>,
    /// The identity.
    pub profile: &'a AwsProfile,
}

static INSTANCE: AwsProfile = AwsProfile::Instance;

impl Default for AwsAccess<'_> {
    fn default() -> Self {
        Self {
            region: None,
            profile: &INSTANCE,
        }
    }
}

/// A named AWS profile: letters, digits, `-`, `_`, `.`, starting with a
/// letter or digit (so it can never read as a CLI flag).
#[must_use]
pub fn valid_aws_profile(p: &str) -> bool {
    (1..=64).contains(&p.len())
        && p.as_bytes()[0].is_ascii_alphanumeric()
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// `<config_key>.awsProfile` from `block`. A value that is not a valid name
/// is [`AwsProfile::Invalid`] with a warning: the block's `ssm://` refs are
/// then refused, never read with the default chain.
pub fn parse_aws_profile(
    block: &serde_json::Map<String, Value>,
    config_key: &str,
    warnings: &mut Vec<String>,
) -> AwsProfile {
    match block.get("awsProfile") {
        None | Some(Value::Null) => AwsProfile::Instance,
        Some(Value::String(p)) if valid_aws_profile(p.trim()) => {
            AwsProfile::Named(p.trim().to_string())
        }
        Some(other) => {
            warnings.push(format!(
                "{config_key}.awsProfile must be a profile name, not {other}; its ssm:// key \
                 refs are refused until it is fixed"
            ));
            AwsProfile::Invalid
        }
    }
}

/// The region an `ssm://` ref is read in: the block's `region`, else
/// `AWS_REGION`, else `AWS_DEFAULT_REGION` (the AWS CLI's own order), each
/// only when it is a plausible region name. `None` refuses `ssm://` refs.
#[must_use]
pub fn resolve_region(
    configured: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    configured
        .map(str::to_string)
        .or_else(|| env("AWS_REGION"))
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .map(|r| r.trim().to_string())
        .filter(|r| valid_region(r))
}

/// `us-east-1`-shaped: lowercase letters, digits and dashes.
#[must_use]
pub fn valid_region(r: &str) -> bool {
    (4..=32).contains(&r.len())
        && r.contains('-')
        && r.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The `aws` argv for reading `parameter`.
#[must_use]
pub fn ssm_args(parameter: &str, region: &str, profile: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = [
        "ssm",
        "get-parameter",
        "--name",
        parameter,
        "--with-decryption",
        "--region",
        region,
        "--query",
        "Parameter.Value",
        "--output",
        "text",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    if let Some(p) = profile {
        // One token: a name can never be taken for a separate flag.
        args.push(format!("--profile={p}"));
    }
    args
}

/// The `aws ssm get-parameter` command: [`ssm_args`], with every
/// [`AWS_IDENTITY_ENV`] selector removed and, for the instance role, the
/// shared files pointed at `/dev/null`.
#[must_use]
pub fn ssm_command(parameter: &str, region: &str, profile: Option<&str>) -> std::process::Command {
    let mut cmd = std::process::Command::new("aws");
    cmd.args(ssm_args(parameter, region, profile))
        .stdin(std::process::Stdio::null());
    for key in AWS_IDENTITY_ENV {
        cmd.env_remove(key);
    }
    if profile.is_none() {
        for key in AWS_FILE_ENV {
            cmd.env(key, "/dev/null");
        }
    }
    cmd
}

/// The first non-empty line of `text`, at most 240 characters.
#[must_use]
pub fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no stderr)");
    line.chars().take(240).collect()
}

/// Read `parameter`, decrypted, under `aws`. `config_key` names the block in
/// the errors that tell an operator what to fix.
///
/// # Errors
///
/// No region, an invalid profile, or the CLI failing (never its stdout).
pub fn ssm_get(parameter: &str, aws: &AwsAccess<'_>, config_key: &str) -> Result<String, String> {
    let profile = match aws.profile {
        AwsProfile::Instance => None,
        AwsProfile::Named(n) => Some(n.as_str()),
        AwsProfile::Invalid => {
            return Err(format!(
                "{config_key}.awsProfile is invalid, so ssm:// key refs are refused (the read \
                 never falls back to the default credential chain)"
            ))
        }
    };
    // An explicit region always: a daemon's environment often has none, and
    // the AWS CLI would then fail or guess.
    let region = aws.region.ok_or_else(|| {
        format!(
            "an ssm:// key ref needs a region: set {config_key}.region, or AWS_REGION / \
             AWS_DEFAULT_REGION in the daemon's environment"
        )
    })?;
    match crate::proc_exec::run_bounded(ssm_command(parameter, region, profile), SSM_TIMEOUT) {
        Ok(crate::proc_exec::Completion::Exited(out)) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Ok(crate::proc_exec::Completion::Exited(out)) => Err(format!(
            "aws ssm get-parameter {parameter} ({region}{}) exited {}: {}",
            profile
                .map(|p| format!(", profile {p}"))
                .unwrap_or_default(),
            out.status.code().unwrap_or(-1),
            first_line(&String::from_utf8_lossy(&out.stderr)),
        )),
        Ok(_) => Err(format!("aws ssm get-parameter {parameter} ({region}) timed out")),
        Err(e) => Err(format!("aws ssm get-parameter {parameter}: {e}")),
    }
}

#[cfg(test)]
#[path = "aws_ssm_tests.rs"]
mod tests;
