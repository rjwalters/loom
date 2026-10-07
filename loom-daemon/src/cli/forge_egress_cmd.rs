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
    /// Print the container `gh` credential arguments (#9987) — the whole
    /// decision, so `spawn-claude.sh` only appends lines. The first stdout
    /// line is always an explicit status, never empty-means-none:
    /// `loom-forge-egress: managed` (then the read-only launcher, upstream
    /// `gh`, policy and credential-reference `docker run` arguments, and no
    /// `~/.config/gh` / `GH_TOKEN` / `GITHUB_TOKEN`), or `unconfigured` /
    /// `observe-unmanaged` (no env/machine policy, or a valid `observe` one
    /// whose launcher is unusable — findings on stderr; then the pre-#9987
    /// arguments: the token variables forwarded by name, else the read-only
    /// `~/.config/gh` mount). One argument per line. A configured policy that
    /// cannot be honoured (unreadable, unsupported `schemaVersion`, invalid
    /// or missing/empty launcher under `required`) — or, with `--image`, an
    /// image `container-check` refuses — prints NOTHING on stdout and exits
    /// 78 with the named refusal on stderr.
    ContainerArgs {
        /// Also run `container-check` for this worker image first.
        #[arg(long)]
        image: Option<String>,
    },
    /// The container egress boundary (#9989, scope 3): under
    /// `enforcement.api = required`, start the egress sidecar, run the
    /// policy's negative canary inside the container's network namespace, and
    /// print `loom-forge-egress-network: isolated` followed by the `docker
    /// run` arguments (`--network container:…`, one per line) the worker must
    /// add. Anything not `required` (no policy, `observe`) prints
    /// `loom-forge-egress-network: none` and nothing else. A canary that
    /// reaches the API (`runtime.bypass-open`), one that cannot run, or a
    /// boundary that cannot be installed (`runtime.unverifiable`) prints
    /// NOTHING on stdout and exits 78: the spawn is aborted.
    ContainerNetwork {
        /// The worker image the container will run (also the canary's image).
        #[arg(long)]
        image: String,
        /// Map `host.docker.internal` (credential proxy path); takes `0`/`1`
        /// so `spawn-claude.sh` passes its flag value in one line.
        #[arg(
            long,
            action = clap::ArgAction::Set,
            value_parser = clap::builder::BoolishValueParser::new(),
            default_value_t = false
        )]
        add_host_gateway: bool,
        /// Remove the sidecar when this host pid exits.
        #[arg(long)]
        watch_pid: Option<u32>,
    },
    /// Admit `image` for a containerised worker under the managed launcher
    /// (#9987): under `enforcement.api = required`, refuse (exit 78) an image
    /// without `python3` (the launcher is Python 3) or whose `gh` does not
    /// resolve to the launcher. Silent, exit 0, with no policy.
    ContainerCheck {
        /// The worker image the container will run.
        image: String,
    },
    /// Classify one Bash command for the `loom:forge-egress` `PreToolUse`
    /// rule (#9989): under an enforcing policy, a typed bypass of the managed
    /// launcher prints the denial (`BLOCKED [routing.denied-by-guard]: …`) and
    /// exits 1. Silent exit 0 otherwise, including with no policy, an
    /// `observe` one, or `guards.forgeEgress=false`.
    Guard {
        /// The (masked) command text the hook is judging.
        #[arg(long = "for-command")]
        for_command: String,
    },
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

/// The first line `forge egress container-args` prints, before the status
/// (`spawn-claude.sh` matches it literally).
const CONTAINER_ARGS_STATUS_PREFIX: &str = "loom-forge-egress: ";

/// First line `forge egress container-network` prints (`spawn-claude.sh`
/// matches it literally; no line means refusal, never "no boundary").
const CONTAINER_NETWORK_STATUS_PREFIX: &str = "loom-forge-egress-network: ";

/// Print the refusal for `finding` and exit 78 (`EX_CONFIG`).
fn refuse(finding: &Finding) -> ! {
    eprintln!("{}", forge_egress::worker_env::refusal_message(finding));
    std::process::exit(78);
}

/// `container-check`: under `enforcement.api = required`, refuse (exit 78) an
/// image without `python3` or whose `gh` does not resolve to the launcher.
fn container_check(image: &str, egress: &forge_egress::worker_env::WorkerEgress) {
    use forge_egress::worker_env as we;
    use loom_daemon::worker_spawn::containment;
    if !egress.required {
        return;
    }
    if !containment::image_has_python3(image) {
        refuse(&we::python3_missing_finding(image));
    }
    let resolved = containment::image_resolved_gh(image, egress);
    if let Some(f) = we::container_launcher_finding(egress, image, resolved.as_deref()) {
        refuse(&f);
    }
}

/// Dispatch one `forge egress` verb; exits the process with the verdict.
pub(crate) fn handle(action: EgressAction) -> Result<()> {
    if let EgressAction::ContainerNetwork {
        image,
        add_host_gateway,
        watch_pid,
    } = &action
    {
        use loom_daemon::worker_spawn::egress_policy as ep;
        let admission = match forge_egress::worker_env::WorkerEgress::admit_process() {
            Ok(admission) => admission,
            Err(finding) => refuse(&finding),
        };
        let opts = ep::Options {
            add_host_gateway: *add_host_gateway,
            watch_pid: *watch_pid,
        };
        let sidecar = match admission.egress.as_ref() {
            Some(egress) => match ep::establish(egress, image, &opts) {
                Ok(sidecar) => sidecar,
                Err(finding) => refuse(&finding),
            },
            None => None,
        };
        match sidecar {
            Some(sidecar) => {
                eprintln!("{}", ep::verified_message(&sidecar));
                println!("{CONTAINER_NETWORK_STATUS_PREFIX}isolated");
                for arg in sidecar.network_args() {
                    println!("{arg}");
                }
            }
            None => println!("{CONTAINER_NETWORK_STATUS_PREFIX}none"),
        }
        return Ok(());
    }
    let image = match &action {
        EgressAction::ContainerArgs { image } => Some(image.clone()),
        EgressAction::ContainerCheck { image } => Some(Some(image.clone())),
        _ => None,
    };
    if let Some(image) = image {
        // A configured-but-failing policy is a named refusal (exit 78) with
        // nothing on stdout; an absent one says `unconfigured` explicitly, so
        // the caller never reads empty output as leave to restore ambient
        // credentials (#9987).
        let admission = match forge_egress::worker_env::WorkerEgress::admit_process() {
            Ok(admission) => admission,
            Err(finding) => refuse(&finding),
        };
        if let (Some(image), Some(egress)) = (image, &admission.egress) {
            container_check(&image, egress);
        }
        if matches!(action, EgressAction::ContainerArgs { .. }) {
            for warning in &admission.warnings {
                eprintln!("{}", forge_egress::worker_env::observe_message(warning));
            }
            println!("{CONTAINER_ARGS_STATUS_PREFIX}{}", admission.status());
            for arg in admission.docker_args(|k| std::env::var_os(k)) {
                println!("{arg}");
            }
        }
        return Ok(());
    }
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
        EgressAction::ContainerArgs { .. }
        | EgressAction::ContainerCheck { .. }
        | EgressAction::ContainerNetwork { .. } => 0, // above
        EgressAction::Guard { for_command } => {
            match forge_egress::guard::check_process(&for_command, &ws) {
                Some(reason) => {
                    println!("{reason}");
                    1
                }
                None => 0,
            }
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
