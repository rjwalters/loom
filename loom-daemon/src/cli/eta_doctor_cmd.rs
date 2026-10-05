//! `loom-daemon eta doctor` (#10391): read-only. Prints each link of the ETA
//! pipeline for this host (config, data source, fit, serving, snapshot feed,
//! outcomes) with the exact remedy for each WARN and FAIL.
//!
//! The verdicts are [`loom_daemon::eta::doctor`]; the facts they judge are
//! gathered by [`loom_daemon::eta::doctor_facts`]. This file only parses
//! arguments and prints.
//!
//! Exit codes: `0` no FAIL, `1` at least one FAIL, `2` it could not evaluate
//! (not a Loom workspace).

use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;

use loom_daemon::eta::doctor::{self, Check};

#[derive(clap::Args)]
pub(crate) struct EtaDoctorArgs {
    /// The Loom workspace to inspect. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Print the checks as a JSON array.
    #[arg(long)]
    pub json: bool,
}

impl EtaDoctorArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = super::eta_fleet_cmd::resolve_root(self.repo_root.clone());
        if !root.join(".loom").is_dir() {
            eprintln!(
                "eta doctor: {} is not a Loom workspace (no .loom directory)",
                root.display()
            );
            std::process::exit(2);
        }
        let host = loom_daemon::sweep_registry::host_identity();
        let facts = loom_daemon::eta::doctor_facts::gather(&root, &host, Utc::now());
        let checks = doctor::evaluate(&facts);
        if self.json {
            println!("{}", serde_json::to_string_pretty(&checks)?);
        } else {
            print!("{}", render(&host, &checks));
        }
        if doctor::has_fail(&checks) {
            std::process::exit(1);
        }
        Ok(())
    }
}

fn render(host: &str, checks: &[Check]) -> String {
    let mut out = format!("eta doctor: host {host}\n");
    for check in checks {
        out.push_str(&check.render());
        out.push('\n');
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {

    #[test]
    fn this_file_and_the_pure_half_are_read_only() {
        let sources = [
            (
                "cli/eta_doctor_cmd.rs",
                include_str!("eta_doctor_cmd.rs")
                    .split("#[cfg(test)]")
                    .next()
                    .unwrap(),
            ),
            (
                "eta/doctor.rs",
                include_str!("../eta/doctor.rs")
                    .split("#[cfg(test)]")
                    .next()
                    .unwrap(),
            ),
            ("eta/doctor_facts.rs", include_str!("../eta/doctor_facts.rs")),
        ];
        let needles = [
            concat!("gh", "_invocation"),
            concat!("Gh", "Invocation"),
            concat!("forge", "_listing"),
            concat!("Command", "::new"),
            concat!("arm_singleton", "_job"),
            concat!("fs::wr", "ite"),
            concat!("write", "_atomic"),
            concat!("create", "_dir"),
            concat!("File::cre", "ate"),
            concat!("remove", "_file"),
            concat!("fs::ren", "ame"),
        ];
        for (name, source) in sources {
            for needle in needles {
                assert!(!source.contains(needle), "{name} mentions `{needle}`");
            }
        }
    }
}
