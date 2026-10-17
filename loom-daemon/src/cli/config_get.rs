//! `loom-daemon config get <key>` — read one key of the resolved effective
//! config (#12591).
//!
//! Thin clap wiring over [`loom_daemon::config_resolver`]; `main.rs` is frozen
//! by the file-size ratchet, so the variant there is one line.
//!
//! Prints one JSON object: `{"key", "found", "value", "tier", "source"}`.
//! `tier` names the highest-precedence tier that supplied the key (`local`,
//! `fleet-repo`, `project`, `committed`, `defaults`), `source` is that tier's
//! file. Exit codes: `0` key found, `1` key unset in every tier (the object is
//! still printed, `found: false`), `2` error (bad arguments).

use std::path::Path;

use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use serde_json::{json, Value};

use loom_daemon::config_resolver::{
    get_path, resolve_effective_config, source_of, FLEET_REPO_CONFIG_REL, LEGACY_CONFIG_REL,
    LOCAL_CONFIG_REL, PROJECT_CONFIG_REL,
};

/// `loom-daemon config` arguments.
#[derive(Args)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print the effective value of a dotted config key (e.g.
    /// `champion.standingAuthorizations`) for a repo as JSON, with the tier it
    /// came from. Read-only.
    Get {
        /// Dotted key path.
        key: String,
        /// Accepted for symmetry with other verbs; output is always JSON.
        #[arg(long)]
        json: bool,
        /// Repo root to resolve against. Defaults to the current directory.
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,
    },
}

pub(crate) fn dispatch(args: ConfigArgs) -> Result<()> {
    match args.command {
        ConfigCommand::Get { key, workspace, .. } => {
            let report = lookup(Path::new(&workspace), &key)?;
            println!("{report}");
            if report["found"] == Value::Bool(true) {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
    }
}

/// Name the tier a source file belongs to.
fn tier_name(repo_root: &Path, source: &Path) -> &'static str {
    let is = |rel: &str| source == repo_root.join(rel);
    if is(LOCAL_CONFIG_REL) {
        "local"
    } else if is(FLEET_REPO_CONFIG_REL) {
        "fleet-repo"
    } else if is(PROJECT_CONFIG_REL) {
        "project"
    } else if is(LEGACY_CONFIG_REL) {
        "committed"
    } else {
        "defaults"
    }
}

fn lookup(repo_root: &Path, key: &str) -> Result<Value> {
    if key.is_empty() || key.split('.').any(str::is_empty) {
        bail!("invalid config key {key:?}");
    }
    let effective = resolve_effective_config(repo_root);
    let value = get_path(&effective, key).filter(|v| !v.is_null());
    let source = value.and_then(|_| source_of(repo_root, key));
    Ok(json!({
        "key": key,
        "found": value.is_some(),
        "value": value,
        "tier": source.as_deref().map(|s| tier_name(repo_root, s)),
        "source": source.as_ref().map(|s| s.display().to_string()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn lookup_isolated(root: &Path, key: &str) -> Value {
        std::env::set_var(loom_daemon::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let v = lookup(root, key).unwrap();
        std::env::remove_var(loom_daemon::config_resolver::PRIVATE_DEFAULTS_ENV);
        v
    }

    #[test]
    #[serial(loom_config_env)]
    fn fleet_tier_only_class_is_returned_with_its_tier() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            FLEET_REPO_CONFIG_REL,
            r#"{"champion":{"standingAuthorizations":[{"id":"a"}]}}"#,
        );
        let v = lookup_isolated(tmp.path(), "champion.standingAuthorizations");
        assert_eq!(v["found"], true);
        assert_eq!(v["tier"], "fleet-repo");
        assert_eq!(v["value"][0]["id"], "a");
    }

    #[test]
    #[serial(loom_config_env)]
    fn committed_class_still_works_and_fleet_tier_wins() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            LEGACY_CONFIG_REL,
            r#"{"champion":{"standingAuthorizations":[{"id":"c"}]}}"#,
        );
        let v = lookup_isolated(tmp.path(), "champion.standingAuthorizations");
        assert_eq!(v["tier"], "committed");
        write(
            tmp.path(),
            FLEET_REPO_CONFIG_REL,
            r#"{"champion":{"standingAuthorizations":[{"id":"f"}]}}"#,
        );
        let v = lookup_isolated(tmp.path(), "champion.standingAuthorizations");
        assert_eq!(v["tier"], "fleet-repo");
        assert_eq!(v["value"][0]["id"], "f");
    }

    #[test]
    #[serial(loom_config_env)]
    fn unset_key_reports_not_found_and_bad_key_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let v = lookup_isolated(tmp.path(), "champion.standingAuthorizations");
        assert_eq!(v["found"], false);
        assert!(v["tier"].is_null());
        assert!(lookup(tmp.path(), "a..b").is_err());
    }

    #[test]
    #[serial(loom_config_env)]
    fn malformed_value_is_returned_verbatim_for_the_caller_to_reject() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            FLEET_REPO_CONFIG_REL,
            r#"{"champion":{"standingAuthorizations":"oops"}}"#,
        );
        let v = lookup_isolated(tmp.path(), "champion.standingAuthorizations");
        assert_eq!(v["value"], "oops");
    }
}
