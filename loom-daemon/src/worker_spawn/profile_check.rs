//! Report a model profile's resolvability without spawning anything: which
//! harness it binds, which variables it maps, and which of them are unset.
use super::{profiles, LaunchError};
use std::fmt::Write;

#[derive(clap::Args)]
pub struct WorkerArgs {
    #[command(subcommand)]
    command: WorkerCommand,
}

#[derive(clap::Subcommand)]
enum WorkerCommand {
    /// Report whether a model profile can be resolved here. Reads no secrets,
    /// contacts no provider and never launches a worker. Exits 78 when a bound
    /// harness is unresolvable (unknown profile, unmapped harness, unset vars).
    ProfileCheck {
        /// Profile name. Defaults to the configured/bundled default profile.
        #[arg(value_name = "NAME")]
        name: Option<String>,
        /// Restrict the report to one harness (`pi`, `opencode`).
        #[arg(long, value_name = "HARNESS")]
        runtime: Option<String>,
    },
}

fn report(name: Option<&str>, runtime: Option<&str>) -> Result<(String, bool), LaunchError> {
    let root = super::workspace(None)?;
    let config = crate::config_resolver::resolve_effective_config(&root);
    let (name, profile) = profiles::lookup(name, &config)?;
    let mut out = String::new();
    let _ = writeln!(out, "profile: {name}");
    let _ = writeln!(out, "model: {}", profile.model);
    if let Some(effort) = &profile.effort {
        let _ = writeln!(out, "effort: {effort}");
    }
    let harnesses: Vec<&String> = profile
        .providers
        .keys()
        .filter(|h| runtime.is_none_or(|r| r == h.as_str()))
        .collect();
    if harnesses.is_empty() {
        return Err(LaunchError::config("model profile has no provider binding for this harness"));
    }
    let mut resolvable = true;
    for harness in harnesses {
        let _ = writeln!(out, "harness {harness}: provider {}", profile.providers[harness]);
        let mapping = profiles::credentials(&profile, harness)?;
        for (source, target) in &mapping.pairs {
            let state = if std::env::var_os(source).is_none_or(|v| v.is_empty()) {
                "unset"
            } else {
                "present"
            };
            let _ = writeln!(out, "  credential {source} -> {target} ({state})");
        }
        for key in ["providerOptions", "providerDefinition"] {
            let field = if key == "providerOptions" {
                &profile.provider_options
            } else {
                &profile.provider_definition
            };
            if let Some(block) = field.get(harness).and_then(serde_json::Value::as_object) {
                let keys: Vec<&str> = block.keys().map(String::as_str).collect();
                let _ = writeln!(out, "  {key}: {}", keys.join(", "));
            }
        }
        let unset = profiles::missing(&mapping.required);
        if unset.is_empty() {
            let _ = writeln!(out, "  status: resolvable");
        } else {
            resolvable = false;
            let _ = writeln!(out, "  status: unresolvable, set {}", unset.join(", "));
        }
    }
    Ok((out, resolvable))
}

pub fn cli(args: WorkerArgs) -> anyhow::Result<()> {
    let WorkerCommand::ProfileCheck { name, runtime } = args.command;
    match report(name.as_deref(), runtime.as_deref()) {
        Ok((text, resolvable)) => {
            print!("{text}");
            if !resolvable {
                std::process::exit(78);
            }
        }
        Err(error) => {
            eprintln!("{}", error.message);
            std::process::exit(error.code);
        }
    }
    Ok(())
}
