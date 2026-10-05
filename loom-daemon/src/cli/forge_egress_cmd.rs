//! `loom-daemon forge egress assert|doctor|policy` (#9984) — the CLI half of
//! [`loom_daemon::forge_egress`]. Arg docs live here, beside the verbs, so
//! the frozen `main.rs` / `forge_action.rs` pay one variant line each.
//!
//! Exit taxonomy (the house one, 2am#1911): **0 aligned, 1 findings, 2
//! verification incomplete**; both 1 and 2 fail routing admission. The exit
//! code is the ROUTING verdict only. No policy anywhere ⇒ exit 0, no output
//! change beyond the report saying `unconfigured`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use loom_daemon::forge_egress::{
    self,
    policy::{self, PolicySources, Resolution},
    report::{redact_value, Finding, Section, Severity},
};

/// `loom-daemon forge egress …` verbs.
#[derive(Subcommand)]
pub(crate) enum EgressAction {
    /// The cheap hot-path check: policy resolution + the effective `gh`
    /// build + every effective `GH_CONFIG_DIR` profile. No network. What
    /// dispatch and spawn run before forge work.
    Assert {
        /// Exit status only; print nothing.
        #[arg(long)]
        quiet: bool,
    },
    /// The full report: routing, git, runtime (runs
    /// `enforcement.negativeCanary` when the policy is env/machine-owned),
    /// telemetry — each with its own exit code; the process exits with the
    /// routing one.
    Doctor {
        /// Machine-readable report (2am `github-egress doctor --json` shape).
        #[arg(long)]
        json: bool,
    },
    /// Print the resolved policy (path, origin, ignored candidates and the
    /// document with anything token-shaped redacted). Always exits 0 unless
    /// the policy is unreadable (2).
    Policy,
}

fn workspace() -> PathBuf {
    loom_daemon::repo_root::find_repo_root_from_cwd()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn print_findings(findings: &[Finding], indent: &str) {
    for f in findings {
        let j = f.to_json();
        let field = |k: &str| j[k].as_str().unwrap_or("").to_string();
        let mark = match f.severity {
            Severity::Notice => "NOTICE    ",
            Severity::Finding => "FINDING   ",
            Severity::Incomplete => "INCOMPLETE",
        };
        println!("{indent}{mark} {}", f.code);
        println!("{indent}  invariant: {}", f.invariant);
        for (label, key) in [
            ("expected: ", "expected"),
            ("observed: ", "observed"),
            ("source:   ", "source"),
            ("fix:      ", "remedy"),
        ] {
            let v = field(key);
            if !v.is_empty() {
                println!("{indent}  {label} {v}");
            }
        }
    }
}

fn verdict(code: i32) -> &'static str {
    match code {
        0 => "aligned",
        1 => "findings",
        _ => "verification incomplete",
    }
}

/// The installer / `loom update` entry point (`loom-daemon init`, #9984):
/// run `doctor` for `workspace` once the install is on disk. Silent when no
/// policy resolves. Otherwise prints the routing verdict and findings, and
/// returns an error (non-zero exit) when routing is not aligned under
/// `enforcement.api = required`; `observe` prints and succeeds.
pub(crate) fn post_install_doctor(workspace: &std::path::Path) -> Result<()> {
    let report = forge_egress::doctor_for(workspace);
    post_install_verdict(&report)
}

fn post_install_verdict(report: &forge_egress::Report) -> Result<()> {
    let code = report.exit_code();
    if !report.is_configured() {
        if report.routing.is_empty() {
            return Ok(());
        }
        println!("\nForge egress (loom-daemon forge egress doctor): no policy resolves");
        print_findings(&report.routing, "  ");
        if code == 0 {
            return Ok(());
        }
        anyhow::bail!(
            "forge egress policy is unconfigured on a host declared managed (exit {code}): [{}]",
            report.routing_codes().join(", ")
        );
    }
    println!("\nForge egress (loom-daemon forge egress doctor): routing {}", verdict(code));
    print_findings(&report.routing, "  ");
    if code == 0 || report.observe_only() {
        return Ok(());
    }
    anyhow::bail!(
        "forge egress routing is not aligned (exit {code}, enforcement.api=required): [{}] — \
         run `loom-daemon forge egress doctor` for the full report",
        report.routing_codes().join(", ")
    )
}

/// Whether non-quiet `assert` prints its findings. Only a failing verdict
/// does: the `policy.unconfigured` notice rides every generic install and
/// exits 0, and `assert` stays silent there (sweep-run-hygiene contract).
fn assert_prints_findings(report: &forge_egress::Report) -> bool {
    report.exit_code() != 0 && !report.routing.is_empty()
}

/// Dispatch one `forge egress` verb; exits the process with the verdict.
pub(crate) fn handle(action: EgressAction) -> Result<()> {
    let ws = workspace();
    let code = match action {
        EgressAction::Assert { quiet } => {
            let report = forge_egress::assert_for(&ws);
            if !quiet && assert_prints_findings(&report) {
                eprintln!("forge-egress: managed routing admission failed");
                print_findings(&report.routing, "  ");
            }
            report.exit_code()
        }
        EgressAction::Doctor { json } => {
            let report = forge_egress::doctor_for(&ws);
            if json {
                println!("{}", serde_json::to_string_pretty(&report.to_json())?);
            } else {
                let j = report.to_json();
                let p = &j["policy"];
                println!(
                    "policy:        {} (origin: {})",
                    p["path"].as_str().unwrap_or("-"),
                    p["origin"].as_str().unwrap_or("-")
                );
                for ig in p["ignored"].as_array().into_iter().flatten() {
                    println!(
                        "ignored:       {} (origin: {}) — a higher-precedence policy wins",
                        ig["path"].as_str().unwrap_or("-"),
                        ig["origin"].as_str().unwrap_or("-")
                    );
                }
                if let Some(o) = j.get("observed") {
                    println!("logical host:  {}", o["logicalHost"].as_str().unwrap_or("-"));
                    println!("api origin:    {}", o["expectedApiHost"].as_str().unwrap_or("-"));
                    println!(
                        "gh:            {}",
                        o["ghVersionLine"].as_str().unwrap_or("not found")
                    );
                    let honoured = o["apiHostHonoured"].as_bool() == Some(true);
                    println!(
                        "api_host:      {}",
                        if honoured {
                            "honoured"
                        } else {
                            "NOT honoured by this build"
                        }
                    );
                }
                if report.is_configured() || !report.routing.is_empty() {
                    for s in Section::ALL {
                        let findings = report.section(s);
                        println!(
                            "\n{}: {}",
                            s.as_str(),
                            verdict(j[s.as_str()]["exit_code"].as_i64().unwrap_or(0) as i32)
                        );
                        print_findings(findings, "  ");
                    }
                }
                println!("\nrouting verdict: exit {}", report.exit_code());
            }
            report.exit_code()
        }
        EgressAction::Policy => match policy::resolve(&PolicySources::from_process(Some(&ws))) {
            Resolution::Unconfigured => {
                println!("{}", serde_json::json!({"origin": "unconfigured", "path": null}));
                0
            }
            Resolution::Unreadable {
                candidate, error, ..
            } => {
                println!(
                    "{}",
                    serde_json::json!({
                        "origin": candidate.origin.as_str(),
                        "path": candidate.path.display().to_string(),
                        "error": error,
                    })
                );
                2
            }
            Resolution::Loaded(doc) => {
                let ignored: Vec<_> = doc
                        .ignored
                        .iter()
                        .map(|c| serde_json::json!({"origin": c.origin.as_str(), "path": c.path.display().to_string()}))
                        .collect();
                let out = serde_json::json!({
                    "origin": doc.origin.as_str(),
                    "path": doc.path.display().to_string(),
                    "ignored": ignored,
                    "policy": redact_value(&doc.data),
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
                0
            }
        },
    };
    std::process::exit(code);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use loom_daemon::forge_egress::checks::{GhBuild, Observed};
    use loom_daemon::forge_egress::policy::{Origin, PolicyDoc};
    use loom_daemon::forge_egress::{evaluate, run_with, Mode};

    fn report_for(api: &str, gh: &str) -> forge_egress::Report {
        // `{root}` is the fixture suite's temp-dir placeholder; the schema
        // requires absolute profile paths.
        let mut data: serde_json::Value = serde_json::from_str(
            &include_str!("../../tests/fixtures/forge-egress/policy.fixture.json")
                .replace("{root}", "/tmp/fixture-root"),
        )
        .unwrap();
        data["enforcement"]["api"] = api.into();
        let doc = PolicyDoc {
            data,
            path: PathBuf::from("/etc/loom/forge-egress/policy.json"),
            origin: Origin::Machine,
            ignored: vec![],
        };
        let obs = Observed {
            gh: GhBuild {
                path: Some(PathBuf::from("/usr/local/bin/gh")),
                version: loom_daemon::forge_egress::checks::version_tuple(gh),
                raw: format!("gh version {gh}"),
            },
            path_gh: Some(PathBuf::from("/usr/local/bin/gh")),
            launcher_exists: true,
            ..Observed::default()
        };
        evaluate(&doc, &obs, Mode::Assert)
    }

    #[test]
    fn assert_is_silent_when_unconfigured_and_prints_on_failure() {
        let r = run_with(&PolicySources::default(), std::path::Path::new("/x"), Mode::Assert);
        assert_eq!(r.exit_code(), 0);
        assert!(!assert_prints_findings(&r), "{:?}", r.routing_codes());
        let failing = report_for("required", "2.97.0");
        assert!(assert_prints_findings(&failing));
    }

    #[test]
    fn post_install_doctor_is_silent_ok_when_unconfigured() {
        let r = run_with(&PolicySources::default(), std::path::Path::new("/x"), Mode::Doctor);
        assert!(post_install_verdict(&r).is_ok());
    }

    #[test]
    fn post_install_doctor_fails_only_under_required() {
        let required = report_for("required", "2.97.0");
        let err = post_install_verdict(&required).unwrap_err().to_string();
        assert!(err.contains("toolchain.below-api-host-floor"), "{err}");
        assert!(post_install_verdict(&report_for("observe", "2.97.0")).is_ok());
        let aligned = report_for("required", "2.102.0");
        assert!(post_install_verdict(&aligned).is_ok(), "{:?}", aligned.routing_codes());
    }
}
