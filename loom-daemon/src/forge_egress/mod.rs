//! Forge egress policy reader + validator (#9984, C1 of epic #9983).
//!
//! Answers one question for this process: **will its `gh` reach the
//! mandated API origin?** — natively, so a Loom host's routing verdict never
//! depends on a sibling 2am checkout (ADR-0018). This is the Rust twin of
//! 2am's `scripts/lib/github_egress.py` (2am#1927): same policy schema
//! (vendored, [`policy::SCHEMA_JSON`]), same finding codes, same exit
//! taxonomy, same report shape. The two stay in lockstep by sharing the
//! fixture suite as data (`loom-daemon/tests/fixtures/forge-egress/`).
//!
//! | Verb | What | Network |
//! |------|------|---------|
//! | [`assert_for`] (`forge egress assert`) | policy + effective `gh` build + effective profiles | none |
//! | [`doctor_for`] (`forge egress doctor`) | + git, runtime canary, telemetry sections | canary only |
//!
//! **Exit taxonomy**: `0` aligned, `1` findings, `2` verification incomplete
//! (both 1 and 2 fail admission). The process exit code is the **routing**
//! verdict only; git/runtime/telemetry carry their own codes.
//!
//! **Unconfigured is a no-op**: no policy anywhere ⇒ every entry point
//! returns 0 and changes nothing (upstream users, Gitea deployments).
//!
//! Entry points wired in this module's [`gate`]: daemon startup + periodic
//! drift ([`gate::start`]), sweep dispatch ([`gate::dispatch_refusal`]),
//! worker spawn ([`gate::spawn_refusal`]); plus `Invariant::ForgeEgressAligned`
//! in `install_self_check` and the `Forge egress:` line of `loom-daemon
//! status`. See `defaults/docs/forge-egress.md`.

pub mod checks;
pub mod gate;
pub mod policy;
pub mod probe;
pub mod report;

use std::path::Path;

use serde_json::{json, Value};

use checks::{api_host_supported, ApiHost, Observed};
use policy::{dig, dig_str, expected_api_host, Candidate, PolicyDoc, PolicySources, Resolution};
use report::{dedupe, exit_code, section_json, Finding, Section, SCHEMA_VERSION};

/// Which verb produced a [`Report`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Routing only, no canary — the hot path.
    Assert,
    /// Every section; runs the canary when configured and trusted.
    Doctor,
}

/// The policy part of a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyState {
    Unconfigured,
    Loaded {
        origin: &'static str,
        path: String,
        epoch: Option<i64>,
        deployment: Option<String>,
        observe_only: bool,
    },
    Unreadable {
        origin: &'static str,
        path: String,
    },
}

/// A validator verdict.
#[derive(Debug, Clone)]
pub struct Report {
    pub policy: PolicyState,
    pub ignored: Vec<Candidate>,
    pub routing: Vec<Finding>,
    pub git: Vec<Finding>,
    pub runtime: Vec<Finding>,
    pub telemetry: Vec<Finding>,
    pub observed: Option<Value>,
    pub mode: Mode,
}

impl Report {
    /// The routing verdict — the process exit code.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        exit_code(&self.routing)
    }

    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.policy != PolicyState::Unconfigured
    }

    /// `enforcement.api = observe`. An unreadable policy is never observe-only.
    #[must_use]
    pub fn observe_only(&self) -> bool {
        matches!(
            self.policy,
            PolicyState::Loaded {
                observe_only: true,
                ..
            }
        )
    }

    /// Routing finding codes, in order.
    #[must_use]
    pub fn routing_codes(&self) -> Vec<String> {
        report::codes(&self.routing)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    #[must_use]
    pub fn section(&self, s: Section) -> &[Finding] {
        match s {
            Section::Routing => &self.routing,
            Section::Git => &self.git,
            Section::Runtime => &self.runtime,
            Section::Telemetry => &self.telemetry,
        }
    }

    /// The JSON report — 2am's `doctor --json` shape plus additive keys
    /// (`policy.ignored`, `policy.enforcement`, `observed.profiles`, …).
    #[must_use]
    pub fn to_json(&self) -> Value {
        let ignored: Vec<Value> = self
            .ignored
            .iter()
            .map(|c| json!({"origin": c.origin.as_str(), "path": c.path.display().to_string()}))
            .collect();
        let policy = match &self.policy {
            PolicyState::Unconfigured => {
                json!({"origin": "unconfigured", "path": null, "ignored": ignored})
            }
            PolicyState::Loaded {
                origin,
                path,
                epoch,
                deployment,
                observe_only,
            } => json!({
                "path": path,
                "origin": origin,
                "epoch": epoch,
                "deployment": deployment,
                "enforcement": if *observe_only { "observe" } else { "required" },
                "ignored": ignored,
            }),
            PolicyState::Unreadable { origin, path } => {
                json!({"path": path, "origin": origin, "ignored": ignored})
            }
        };
        let incomplete_section = || json!({"findings": [], "exit_code": 2});
        let unreadable = matches!(self.policy, PolicyState::Unreadable { .. });
        let mut out = json!({
            "schemaVersion": SCHEMA_VERSION,
            "mode": match self.mode { Mode::Assert => "assert", Mode::Doctor => "doctor" },
            "policy": policy,
            "routing": section_json(&self.routing),
            "git": section_json(&self.git),
            "runtime": if unreadable { incomplete_section() } else { section_json(&self.runtime) },
            "telemetry": if unreadable { incomplete_section() } else { section_json(&self.telemetry) },
            "exit_code": self.exit_code(),
        });
        if let Some(observed) = &self.observed {
            out["observed"] = observed.clone();
        }
        out
    }
}

/// Evaluate a loaded policy against an observation. Pure — the fixture suite
/// drives this directly.
#[must_use]
pub fn evaluate(doc: &PolicyDoc, obs: &Observed, mode: Mode) -> Report {
    let policy = &doc.data;
    let routing = dedupe(checks::assert_routing(policy, obs));
    let (git, runtime, telemetry) = match mode {
        Mode::Assert => (vec![], vec![], vec![]),
        Mode::Doctor => (
            dedupe(checks::assert_git_routing(policy, obs)),
            dedupe(checks::assert_runtime(policy, obs)),
            dedupe(checks::assert_telemetry(policy, obs)),
        ),
    };
    let observed = (mode == Mode::Doctor).then(|| observed_json(policy, obs));
    Report {
        policy: PolicyState::Loaded {
            origin: doc.origin.as_str(),
            path: doc.path.display().to_string(),
            epoch: policy.get("policyEpoch").and_then(Value::as_i64),
            deployment: policy
                .get("deployment")
                .and_then(Value::as_str)
                .map(str::to_string),
            observe_only: policy::is_observe_only(policy),
        },
        ignored: doc.ignored.clone(),
        routing,
        git,
        runtime,
        telemetry,
        observed,
        mode,
    }
}

fn observed_json(policy: &Value, obs: &Observed) -> Value {
    let profiles: Vec<Value> = obs
        .profiles
        .iter()
        .map(|p| {
            let api_host = match &p.api_host {
                ApiHost::Present(h) => h.clone(),
                ApiHost::Missing => "missing".to_string(),
                ApiHost::NoHostEntry => "no-host-entry".to_string(),
                ApiHost::NoHostsFile => "no-hosts-file".to_string(),
            };
            json!({"path": p.path.display().to_string(), "source": p.source.as_str(), "apiHost": api_host})
        })
        .collect();
    json!({
        "ghVersion": obs.gh.version.map(|(a, b, c)| format!("{a}.{b}.{c}")),
        "ghVersionLine": (!obs.gh.raw.is_empty()).then(|| obs.gh.raw.clone()),
        "ghPath": obs.gh.path.as_ref().map(|p| p.display().to_string()),
        "pathGhPath": obs.path_gh.as_ref().map(|p| p.display().to_string()),
        "apiHostHonoured": api_host_supported(obs.gh.version, policy),
        "expectedApiHost": expected_api_host(policy),
        "logicalHost": dig(policy, &["github", "logicalHost"]).cloned().unwrap_or(Value::Null),
        "ghConfigDir": obs.gh_config_dir.as_ref().map(|p| p.display().to_string()),
        "ghHost": obs.gh_host,
        "profiles": profiles,
        "gitRewrites": obs.git_rewrites,
        "gitRollout": dig_str(policy, &["github", "gitOrigin", "rollout"]),
    })
}

/// A report for an unconfigured or unreadable resolution.
fn non_loaded(resolution: Resolution, mode: Mode) -> Report {
    let (policy, ignored, routing) = match resolution {
        Resolution::Unreadable {
            candidate,
            error,
            ignored,
        } => {
            let path = candidate.path.display().to_string();
            let finding =
                Finding::new("policy.unreadable", "a policy document is readable right now")
                    .expected(path.clone())
                    .observed(error)
                    .source(format!("policy resolution (origin {})", candidate.origin.as_str()))
                    .remedy(format!(
                    "install a readable policy at {path} (or fix {}); never fall back to a cached \
                     or default policy",
                    policy::POLICY_ENV
                ))
                    .incomplete();
            (
                PolicyState::Unreadable {
                    origin: candidate.origin.as_str(),
                    path,
                },
                ignored,
                vec![finding],
            )
        }
        _ => (PolicyState::Unconfigured, vec![], vec![]),
    };
    Report {
        policy,
        ignored,
        routing,
        git: vec![],
        runtime: vec![],
        telemetry: vec![],
        observed: None,
        mode,
    }
}

/// Run `mode` for `workspace` against explicit `sources`.
#[must_use]
pub fn run_with(sources: &PolicySources, workspace: &Path, mode: Mode) -> Report {
    match policy::resolve(sources) {
        Resolution::Loaded(doc) => {
            let opts = probe::ProbeOptions {
                run_canary: mode == Mode::Doctor,
            };
            let obs = probe::observe(&doc, workspace, opts);
            evaluate(&doc, &obs, mode)
        }
        other => non_loaded(other, mode),
    }
}

/// The cheap hot-path assertion for this process + `workspace`.
#[must_use]
pub fn assert_for(workspace: &Path) -> Report {
    run_with(&PolicySources::from_process(Some(workspace)), workspace, Mode::Assert)
}

/// The full report for this process + `workspace`.
#[must_use]
pub fn doctor_for(workspace: &Path) -> Report {
    run_with(&PolicySources::from_process(Some(workspace)), workspace, Mode::Doctor)
}

#[cfg(test)]
mod tests;
