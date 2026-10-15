//! Tests for the shared SSM read (2am#4367).
#![allow(clippy::unwrap_used)]

use super::*;
use serde_json::json;

fn args_of(cmd: &std::process::Command) -> Vec<String> {
    cmd.get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn env_of(cmd: &std::process::Command, key: &str) -> Option<Option<String>> {
    cmd.get_envs()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
}

#[test]
fn the_instance_role_read_removes_every_selector_and_blanks_the_shared_files() {
    let cmd = ssm_command("/p/k", "us-east-1", None);
    for key in AWS_IDENTITY_ENV {
        if AWS_FILE_ENV.contains(key) {
            continue;
        }
        assert_eq!(env_of(&cmd, key), Some(None), "{key} must be removed");
    }
    for key in [
        "AWS_ROLE_ARN",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    ] {
        assert!(AWS_IDENTITY_ENV.contains(&key), "{key}");
    }
    for key in AWS_FILE_ENV {
        assert_eq!(env_of(&cmd, key), Some(Some("/dev/null".to_string())), "{key}");
    }
    assert!(!args_of(&cmd).iter().any(|a| a.starts_with("--profile")));
}

#[test]
fn a_named_profile_is_one_token_and_reads_the_default_aws_files() {
    let cmd = ssm_command("/p/k", "us-east-1", Some("relay-reader"));
    assert!(args_of(&cmd).contains(&"--profile=relay-reader".to_string()));
    // The default ~/.aws files: the environment cannot redirect them.
    for key in AWS_FILE_ENV {
        assert_eq!(env_of(&cmd, key), Some(None), "{key}");
    }
    assert_eq!(env_of(&cmd, "AWS_PROFILE"), Some(None));
}

#[test]
fn a_profile_that_could_read_as_a_flag_is_refused() {
    for good in ["relay-reader", "a", "x.y_z-1", "0prof"] {
        assert!(valid_aws_profile(good), "{good}");
    }
    for bad in [
        "",
        "-x",
        "--region",
        ".hidden",
        "a b",
        "a=b",
        &"p".repeat(65),
    ] {
        assert!(!valid_aws_profile(bad), "{bad}");
    }
}

#[test]
fn an_invalid_profile_fails_closed() {
    let block = |v: serde_json::Value| v.as_object().unwrap().clone();
    let mut w = Vec::new();
    assert_eq!(parse_aws_profile(&block(json!({})), "forge.x", &mut w), AwsProfile::Instance);
    assert_eq!(
        parse_aws_profile(&block(json!({"awsProfile": "relay"})), "forge.x", &mut w),
        AwsProfile::Named("relay".into())
    );
    assert!(w.is_empty());
    for bad in [json!("-p"), json!("a b"), json!(7)] {
        let p = parse_aws_profile(&block(json!({ "awsProfile": bad })), "forge.x", &mut w);
        assert_eq!(p, AwsProfile::Invalid);
    }
    assert_eq!(w.len(), 3);
    assert!(w[0].contains("forge.x.awsProfile") && w[0].contains("refused"), "{w:?}");
    // Refused before any AWS call, and never with the default chain.
    let invalid = AwsProfile::Invalid;
    let err = ssm_get(
        "/p/k",
        &AwsAccess {
            region: Some("us-east-1"),
            profile: &invalid,
        },
        "forge.x",
    )
    .unwrap_err();
    assert!(err.contains("forge.x.awsProfile is invalid"), "{err}");
}

#[test]
fn a_missing_region_names_the_block_that_needs_it() {
    let err = ssm_get("/p/k", &AwsAccess::default(), "forge.mirror").unwrap_err();
    assert!(err.contains("forge.mirror.region") && !err.contains("gateway"), "{err}");
}
