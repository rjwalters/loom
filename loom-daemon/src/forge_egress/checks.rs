//! The Contract 4 checks, as pure functions over a policy and an
//! [`Observed`] snapshot.
//!
//! Nothing here spawns, reads a file, or looks at the process environment —
//! [`super::probe`] builds the [`Observed`] snapshot, and the vendored
//! fixture suite (`loom-daemon/tests/fixtures/forge-egress/`) builds it from
//! data. That split is what lets the two validators (2am's Python, this one)
//! stay in lockstep on shared fixture rows.
//!
//! Finding codes are 2am's (`scripts/lib/github_egress.py`) wherever 2am has
//! one (including `policy.unreadable`, emitted from [`super::run_with`]).
//! The Loom-only codes are listed in [`LOOM_ONLY_CODES`]; each covers a
//! surface 2am's validator cannot observe from its repository (Loom's own
//! `hosts.yml` publication, the canary run, Loom's OTLP exporter).
//!
//! The issue text (#9984) sketched `env.gh-host-mismatch` /
//! `git.rewrite-missing`; the shipped codes are 2am's actual ones
//! (`ghhost.*`, `ghrepo.host-qualified-conflict`, `git.missing-rewrite`) so
//! one alert rule (loom-ui#1015) matches both validators.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::policy::{dig, dig_str, expected_api_host};
use super::report::{Finding, Section};

/// Finding codes that exist only in Loom's validator. Everything else is a
/// 2am code, and the fixture suite asserts the Rust output restricted to the
/// non-Loom codes equals 2am's.
pub const LOOM_ONLY_CODES: &[&str] = &[
    "apiconfig.api-host-missing",
    "apiconfig.api-host-mismatch",
    "runtime.bypass-open",
    "telemetry.loom-exporter-not-otlp",
];

/// A parsed `MAJOR.MINOR.PATCH`.
pub type Version = (u64, u64, u64);

/// Parse the first dotted numeric run out of `text` (accepts `gh --version`'s
/// first line). `None` means *unknown* — never treated as zero.
#[must_use]
pub fn version_tuple(text: &str) -> Option<Version> {
    let re = regex::Regex::new(r"(\d+)\.(\d+)\.(\d+)").ok()?;
    let c = re.captures(text)?;
    Some((c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?))
}

/// Does this build route by `api_host` at all? The floor comes from the
/// policy; an unknown version is NOT assumed supported.
#[must_use]
pub fn api_host_supported(version: Option<Version>, policy: &Value) -> bool {
    let floor =
        version_tuple(dig_str(policy, &["toolchain", "ghMinimumVersion"])).unwrap_or((2, 100, 0));
    version.is_some_and(|v| v >= floor)
}

/// The effective `gh` build — the binary Loom will actually exec.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GhBuild {
    /// Resolved exec target (the `gh_invocation` resolver's ladder: policy
    /// launcher, else `$LOOM_GH_BIN`, else `gh` on `PATH`), if any.
    pub path: Option<PathBuf>,
    pub version: Option<Version>,
    /// The first line of `gh --version` (non-secret).
    pub raw: String,
}

/// What a profile's `hosts.yml` says about the logical host's `api_host`.
/// Only this summary is ever retained — never the file's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiHost {
    /// No `hosts.yml` in the profile directory.
    NoHostsFile,
    /// `hosts.yml` exists but has no entry for the logical host.
    NoHostEntry,
    /// The logical host's entry has no `api_host` key (the token-only shape).
    Missing,
    /// The logical host's entry carries this `api_host`.
    Present(String),
}

/// Why a profile directory is being checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSource {
    /// The process's exported `GH_CONFIG_DIR`.
    Env,
    /// `gh`'s default (`$XDG_CONFIG_HOME/gh` or `~/.config/gh`) — used when no
    /// `GH_CONFIG_DIR` is exported, because that is what `gh` then reads.
    Default,
    /// A profile Loom itself publishes (`.loom/gh-config`,
    /// `.loom/gh-config-by-owner/<owner>`).
    LoomOwned,
}

impl ProfileSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "environment GH_CONFIG_DIR",
            Self::Default => "gh default config dir",
            Self::LoomOwned => "Loom-published profile",
        }
    }
}

/// One profile directory that is (or will be) active for some `gh` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub path: PathBuf,
    pub source: ProfileSource,
    pub api_host: ApiHost,
}

/// Result of running `enforcement.negativeCanary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryOutcome {
    /// The canary's direct-egress attempt failed: the boundary held.
    Blocked,
    /// The canary's direct-egress attempt succeeded: bypass is open.
    Open,
    /// The canary could not be run to completion (timeout, spawn failure,
    /// or a repo-origin policy that may not name a command).
    NotRun(&'static str),
}

/// Everything the checks look at, gathered by [`super::probe`] (production)
/// or built from a fixture row (tests).
#[derive(Debug, Clone, Default)]
pub struct Observed {
    pub gh_host: Option<String>,
    pub gh_repo: Option<String>,
    /// The exported `GH_CONFIG_DIR`, if any.
    pub gh_config_dir: Option<PathBuf>,
    /// The `gh` Loom itself execs — what the version floor measures.
    pub gh: GhBuild,
    /// The `gh` an agent's plain `gh` resolves to: bare `gh` on `PATH`,
    /// bypassing the resolver. What `toolchain.launcher-not-first` measures
    /// (2am semantics) — kept apart from [`Self::gh`] because once the daemon
    /// execs the policy launcher directly (#9995), the exec target says
    /// nothing about what agents' shells run.
    pub path_gh: Option<PathBuf>,
    /// Whether `toolchain.launcherPath` exists on this host.
    pub launcher_exists: bool,
    /// Every active profile, in check order (env/default first).
    pub profiles: Vec<Profile>,
    /// Number of `insteadOf`/`pushInsteadOf` rewrites for the logical host.
    /// A count only — rewrite URLs can embed credentials.
    pub git_rewrites: usize,
    /// `None` when no canary is configured (or `assert`, which never runs it).
    pub canary: Option<CanaryOutcome>,
    /// Whether Loom's own observability exporter list includes `otlp`.
    pub loom_otlp_exporter: bool,
}

/// `GH_HOST` / `GH_REPO` against the LOGICAL host — never against apiOrigin.
#[must_use]
pub fn assert_gh_host(policy: &Value, obs: &Observed) -> Vec<Finding> {
    let mut findings = Vec::new();
    let logical = dig_str(policy, &["github", "logicalHost"]);
    let origin_host = expected_api_host(policy);
    if let Some(gh_host) = obs.gh_host.as_deref().filter(|h| !h.is_empty()) {
        if gh_host != logical {
            let (code, remedy) = if !origin_host.is_empty() && gh_host == origin_host {
                (
                    "ghhost.points-at-gateway",
                    "unset GH_HOST: pointing it at the gateway makes gh treat the destination as \
                     Enterprise Server (/api/v3 REST prefix) and makes remote-inferred commands \
                     refuse. Route with api_host, not GH_HOST"
                        .to_string(),
                )
            } else {
                ("ghhost.unapproved", format!("unset GH_HOST, or set it to {logical}"))
            };
            findings.push(
                Finding::new(code, "GH_HOST agrees with the deployment's logical GitHub host")
                    .expected(format!("unset, or {logical}"))
                    .observed(gh_host)
                    .source("environment GH_HOST")
                    .remedy(remedy),
            );
        }
    }
    if let Some(gh_repo) = obs.gh_repo.as_deref() {
        if gh_repo.matches('/').count() >= 2 {
            let qualified = gh_repo.split('/').next().unwrap_or("");
            if qualified != logical {
                findings.push(
                    Finding::new(
                        "ghrepo.host-qualified-conflict",
                        "a host-qualified GH_REPO names the logical host",
                    )
                    .expected(logical)
                    .observed(qualified)
                    .source("environment GH_REPO")
                    .remedy(format!("use an unqualified owner/repo, or qualify it with {logical}")),
                );
            }
        }
    }
    findings
}

fn dotted(v: Version) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// The effective `gh` BUILD, not the presence of a setting.
#[must_use]
pub fn assert_toolchain(policy: &Value, obs: &Observed) -> Vec<Finding> {
    let floor = dig_str(policy, &["toolchain", "ghMinimumVersion"]);
    let pinned = dig_str(policy, &["toolchain", "ghPinnedVersion"]);
    let Some(version) = obs.gh.version else {
        return vec![Finding::new(
            "toolchain.gh-unresolvable",
            "the effective gh build is identifiable",
        )
        .expected(format!("gh >= {floor}"))
        .observed(if obs.gh.raw.is_empty() {
            "no gh on PATH".to_string()
        } else {
            obs.gh.raw.clone()
        })
        .source("gh --version")
        .remedy("provision the managed launcher and the pinned upstream gh")
        .incomplete()];
    };
    let mut findings = Vec::new();
    let floor_v = version_tuple(floor);
    if floor_v.is_some_and(|f| version < f) {
        let target = if pinned.is_empty() { floor } else { pinned };
        findings.push(
            Finding::new(
                "toolchain.below-api-host-floor",
                "the effective gh routes by api_host at all",
            )
            .expected(format!("gh >= {floor}"))
            .observed(dotted(version))
            .source("gh --version")
            .remedy(format!(
                "upgrade to the pinned gh {target}. Below {floor} api_host does not exist: `gh \
                     config set api_host` succeeds, `gh config get` echoes it back, and traffic \
                     still goes to the canonical API"
            )),
        );
    } else if !pinned.is_empty() && version_tuple(pinned) != Some(version) {
        findings.push(
            Finding::new(
                "toolchain.unpinned-version",
                "the effective gh is the qualified pinned version",
            )
            .expected(pinned)
            .observed(dotted(version))
            .source("gh --version")
            .remedy(format!(
                "provision gh {pinned}; this build is above the routing floor but unqualified"
            ))
            .incomplete(),
        );
    }
    let launcher = dig_str(policy, &["toolchain", "launcherPath"]);
    if !launcher.is_empty() {
        if let Some(resolved) = &obs.path_gh {
            if !same_path(resolved, Path::new(launcher)) {
                let f = Finding::new(
                    "toolchain.launcher-not-first",
                    "the `gh` that resolves on PATH is the managed launcher",
                )
                .expected(launcher)
                .observed(resolved.display().to_string())
                .source("PATH resolution")
                .remedy(
                    "put the managed launcher ahead of the unmanaged gh on PATH (provisioning, \
                     not a shell rc file)",
                );
                findings.push(if obs.launcher_exists {
                    f
                } else {
                    f.incomplete()
                });
            }
        }
    }
    findings
}

/// The EFFECTIVE config profiles — enumeration (2am) plus the routing setting
/// each one actually carries (Loom: `hosts.yml` republication, scenario 17).
/// Only paths and the api_host verdict are ever reported.
#[must_use]
pub fn assert_api_routing(policy: &Value, obs: &Observed) -> Vec<Finding> {
    let declared: Vec<PathBuf> = dig(policy, &["principal", "ghConfigDirs"])
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    let expected = expected_api_host(policy);
    let logical = dig_str(policy, &["github", "logicalHost"]);
    let mut findings = Vec::new();
    for profile in &obs.profiles {
        let shown = profile.path.display().to_string();
        // Enumeration: 2am checks the exported GH_CONFIG_DIR; Loom also holds
        // its own published profiles to the same rule. gh's default dir is not
        // an enumeration subject (nothing exported it), only a routing one.
        if profile.source != ProfileSource::Default {
            if declared.is_empty() {
                findings.push(
                    Finding::new(
                        "apiconfig.profile-not-enumerated",
                        "every active GH_CONFIG_DIR profile is enumerated in policy",
                    )
                    .expected("policy.principal.ghConfigDirs lists this profile")
                    .observed(shown.clone())
                    .source(profile.source.as_str())
                    .remedy(
                        "add the profile to policy.principal.ghConfigDirs, or stop exporting \
                         GH_CONFIG_DIR",
                    )
                    .incomplete(),
                );
            } else if !declared.iter().any(|d| same_path(&profile.path, d)) {
                findings.push(
                    Finding::new(
                        "apiconfig.shadowed-profile",
                        "the active GH_CONFIG_DIR is an approved profile",
                    )
                    .expected(
                        declared
                            .iter()
                            .map(|d| d.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                    .observed(shown.clone())
                    .source(profile.source.as_str())
                    .remedy(
                        "an unenumerated profile supplies its own token and never consults \
                             the approved one; enumerate or unset it",
                    ),
                );
            }
        }
        match &profile.api_host {
            ApiHost::Present(h) if *h == expected => {}
            ApiHost::Present(h) => findings.push(
                Finding::new(
                    "apiconfig.api-host-mismatch",
                    "the profile's api_host is the mandated API origin",
                )
                .expected(expected.clone())
                .observed(format!("{shown}: api_host {h}"))
                .source(format!("{} hosts.yml ({logical} entry)", profile.source.as_str()))
                .remedy(format!("gh config set api_host {expected} --host {logical}")),
            ),
            other => {
                let why = match other {
                    ApiHost::NoHostsFile => "no hosts.yml",
                    ApiHost::NoHostEntry => "no entry for the logical host",
                    _ => "entry has no api_host",
                };
                findings.push(
                    Finding::new(
                        "apiconfig.api-host-missing",
                        "the profile's logical-host entry carries api_host",
                    )
                    .expected(format!("api_host {expected}"))
                    .observed(format!("{shown}: {why}"))
                    .source(format!("{} hosts.yml", profile.source.as_str()))
                    .remedy(format!(
                        "GH_CONFIG_DIR={shown} gh config set api_host {expected} --host \
                             {logical} (a token-only republication drops it; see #9986)"
                    )),
                );
            }
        }
    }
    findings
}

/// Git transport, gated on the declared rollout state.
#[must_use]
pub fn assert_git_routing(policy: &Value, obs: &Observed) -> Vec<Finding> {
    let rollout = match dig_str(policy, &["github", "gitOrigin", "rollout"]) {
        "" => "unqualified",
        r => r,
    };
    let host = dig(policy, &["github", "gitOrigin", "host"]).and_then(Value::as_str);
    let mut findings = Vec::new();
    match rollout {
        "unqualified" => {
            if obs.git_rewrites > 0 {
                findings.push(
                    Finding::new(
                        "git.premature-rewrite",
                        "no Git rewrite is installed before a destination serves the protocol",
                    )
                    .expected("no insteadOf/pushInsteadOf rewrite for the logical host")
                    .observed(format!("{} rewrite(s) present", obs.git_rewrites))
                    .source("git config (insteadOf/pushInsteadOf)")
                    .remedy("remove the rewrite until gitOrigin.rollout is at least `qualified`"),
                );
            }
            findings.push(
                Finding::new("git.unqualified", "the Git transport destination is qualified")
                    .expected("a qualified gitOrigin.host")
                    .observed("unqualified")
                    .source("policy github.gitOrigin.rollout")
                    .remedy(
                        "P5/#1570 owns qualification; this stays visible rather than being \
                         implied by the API rollout",
                    )
                    .incomplete()
                    .section(Section::Git),
            );
        }
        "enforced" => match host {
            None | Some("") => findings.push(
                Finding::new(
                    "git.enforced-without-host",
                    "enforced Git routing names a destination",
                )
                .expected("a gitOrigin.host")
                .observed("null")
                .source("policy github.gitOrigin")
                .remedy("set gitOrigin.host or drop the rollout back to qualified")
                .section(Section::Git),
            ),
            Some(h) if obs.git_rewrites == 0 => findings.push(
                Finding::new(
                    "git.missing-rewrite",
                    "enforced Git routing has an effective rewrite",
                )
                .expected(format!("insteadOf rewrite to {h}"))
                .observed("none")
                .source("git config (insteadOf/pushInsteadOf)")
                .remedy(
                    "install the rewrite for every spelling: HTTPS, SCP-style and ssh://, \
                         plus submodule and LFS paths",
                )
                .section(Section::Git),
            ),
            Some(_) => {}
        },
        _ => {}
    }
    findings
}

/// The runtime boundary — never reportable as verified from config alone.
#[must_use]
pub fn assert_runtime(policy: &Value, obs: &Observed) -> Vec<Finding> {
    let state = match dig_str(policy, &["enforcement", "runtimeEgress"]) {
        "" => "unverified",
        s => s,
    };
    let canary = dig(policy, &["enforcement", "negativeCanary"])
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty());
    if canary.is_none() {
        if state == "verified" {
            return vec![Finding::new(
                "runtime.verified-without-canary",
                "`verified` runtime egress is backed by a negative canary",
            )
            .expected("enforcement.negativeCanary set")
            .observed("null")
            .source("policy enforcement")
            .remedy(
                "config assertions cannot establish enforcement; supply the bounded \
                 direct-egress canary or drop the state",
            )
            .section(Section::Runtime)];
        }
        return vec![unverifiable(format!(
            "no canary configured (declared: {state})"
        ))];
    }
    match obs.canary {
        None | Some(CanaryOutcome::Blocked) => vec![],
        Some(CanaryOutcome::Open) => vec![Finding::new(
            "runtime.bypass-open",
            "direct GitHub API egress is blocked for this workload",
        )
        .expected("the negative canary's direct request fails")
        .observed("the canary's direct request succeeded")
        .source("enforcement.negativeCanary (run under the caller's own identity)")
        .remedy("install the host/runner network policy that blocks direct GitHub API egress (C6/#9989)")
        .section(Section::Runtime)],
        Some(CanaryOutcome::NotRun(why)) => vec![unverifiable(format!("canary not run: {why}"))],
    }
}

fn unverifiable(observed: String) -> Finding {
    Finding::new("runtime.unverifiable", "direct GitHub API egress is blocked for this workload")
        .expected("a bounded negative canary under the caller's own identity")
        .observed(observed)
        .source("policy enforcement")
        .remedy("P4 provisions the network policy and the canary; until then this is unverified, not aligned")
        .incomplete()
        .section(Section::Runtime)
}

/// Delivery readiness — its own section, never part of the routing verdict.
#[must_use]
pub fn assert_telemetry(policy: &Value, obs: &Observed) -> Vec<Finding> {
    let mut findings = Vec::new();
    let telemetry = policy.get("telemetry");
    let set = |k: &str| {
        telemetry
            .and_then(|t| t.get(k))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    };
    if !set("serviceName") {
        findings.push(
            Finding::new("telemetry.no-identity", "every signal has a stable emitter identity")
                .expected("telemetry.serviceName")
                .observed("unset")
                .source("policy telemetry")
                .remedy(
                    "set the gateway/adapter service identity after the live-store collision check",
                )
                .incomplete()
                .section(Section::Telemetry),
        );
    }
    if !set("otlpEndpoint") {
        findings.push(
            Finding::new("telemetry.no-endpoint", "an OTel edge is configured for this producer")
                .expected("telemetry.otlpEndpoint")
                .observed("unset")
                .source("policy telemetry")
                .remedy("point host producers at the loopback collector; Workers use the D37 path")
                .incomplete()
                .section(Section::Telemetry),
        );
    } else if !obs.loom_otlp_exporter {
        findings.push(
            Finding::new(
                "telemetry.loom-exporter-not-otlp",
                "Loom's own observability export reaches the policy's OTel edge",
            )
            .expected("observability.exporters includes otlp")
            .observed("no otlp exporter configured for this workspace")
            .source(".loom/config.json observability (loom-daemon/src/observability/otlp)")
            .remedy("add {\"kind\": \"otlp\"} to observability.exporters (requires the `otlp` build feature)")
            .incomplete()
            .section(Section::Telemetry),
        );
    }
    findings
}

/// The cheap hot-path assertion: shape + `GH_HOST` + toolchain + profiles.
/// Excludes git, the runtime canary and telemetry, exactly like 2am's
/// `assert_routing`.
#[must_use]
pub fn assert_routing(policy: &Value, obs: &Observed) -> Vec<Finding> {
    let mut findings = super::policy::assert_policy_shape(policy);
    findings.extend(assert_gh_host(policy, obs));
    findings.extend(assert_toolchain(policy, obs));
    findings.extend(assert_api_routing(policy, obs));
    findings
}
