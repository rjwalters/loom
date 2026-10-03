//! Policy resolution (machine wins, never merged) and the v1 schema reader.
//!
//! # Resolution order
//!
//! 1. `$LOOM_FORGE_EGRESS_POLICY` (`origin: env`) — an explicit path; when set
//!    it is authoritative even if the file is missing (that is exit 2, never a
//!    fall-through to something narrower).
//! 2. `/etc/loom/forge-egress/policy.json` (`origin: machine`) when it exists.
//! 3. `.loom/config.json` `forge.egress.policyPath` (`origin: repo`), relative
//!    paths resolved against the repo root.
//! 4. None of the above ⇒ **unconfigured**: exit 0, no behaviour change.
//!
//! The first candidate that is *present* wins outright; the rest are reported
//! under `policy.ignored`, so a repo-local policy can never weaken a machine
//! one — it is not merged, it is ignored, and the report says so.
//!
//! # Schema
//!
//! `policy.schema.json` is vendored byte-identical from 2am
//! (`infra/github-proxy/policy.schema.json`) and validated with the same
//! keyword subset 2am's dependency-free validator implements. A keyword
//! outside that subset is a test failure ([`SUPPORTED_KEYWORDS`]), never a
//! silently unenforced constraint.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;

use super::report::{looks_like_secret, Finding, SCHEMA_VERSION};

/// Env override naming the policy document explicitly.
pub const POLICY_ENV: &str = "LOOM_FORGE_EGRESS_POLICY";

/// The machine-owned default location. Outside every checkout.
pub const MACHINE_POLICY_PATH: &str = "/etc/loom/forge-egress/policy.json";

/// The repo-config key naming a repo-local policy (`origin: repo`).
pub const REPO_POLICY_KEY: &str = "forge.egress.policyPath";

/// The vendored v1 schema (byte-identical to 2am's).
pub const SCHEMA_JSON: &str = include_str!("policy.schema.json");

/// The JSON Schema keywords the subset validator enforces — the same set as
/// 2am's `SUPPORTED_KEYWORDS`.
pub const SUPPORTED_KEYWORDS: &[&str] = &[
    "$schema",
    "$id",
    "title",
    "description",
    "type",
    "const",
    "enum",
    "required",
    "properties",
    "additionalProperties",
    "items",
    "pattern",
    "format",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "maxItems",
];

/// Where the winning policy document came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Env,
    Machine,
    Repo,
}

impl Origin {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Machine => "machine",
            Self::Repo => "repo",
        }
    }

    /// Whether this origin is trusted to name a command the validator runs
    /// (`enforcement.negativeCanary`). A repo-local policy is not: a checkout
    /// must never be able to make the daemon execute something.
    #[must_use]
    pub fn may_run_canary(self) -> bool {
        matches!(self, Self::Env | Self::Machine)
    }
}

/// One present policy candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub origin: Origin,
    pub path: PathBuf,
}

/// The inputs to resolution — injectable so tests never touch `/etc` or the
/// process environment.
#[derive(Debug, Clone, Default)]
pub struct PolicySources {
    /// `$LOOM_FORGE_EGRESS_POLICY`, if set and non-empty.
    pub env_path: Option<PathBuf>,
    /// The machine path to probe (production: [`MACHINE_POLICY_PATH`]).
    pub machine_path: Option<PathBuf>,
    /// `forge.egress.policyPath`, already resolved against the repo root.
    pub repo_path: Option<PathBuf>,
}

impl PolicySources {
    /// Production sources for `repo_root` (or none for the repo tier).
    #[must_use]
    pub fn from_process(repo_root: Option<&Path>) -> Self {
        let env_path = std::env::var_os(POLICY_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let repo_path = repo_root.and_then(repo_policy_path);
        Self {
            env_path,
            machine_path: Some(PathBuf::from(MACHINE_POLICY_PATH)),
            repo_path,
        }
    }

    fn present(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        if let Some(p) = &self.env_path {
            out.push(Candidate {
                origin: Origin::Env,
                path: p.clone(),
            });
        }
        // The env and repo tiers are explicitly named, so they are always
        // candidates (a missing named file resolves `Unreadable`). The machine
        // tier is probed: only `NotFound` means absent. Any other error (e.g.
        // EACCES under a root-owned 0700 `/etc/loom/forge-egress/`) means the
        // policy is present-but-unreadable, so it still wins and resolves
        // `Unreadable` (exit 2) — never a fall-through to the repo tier or to
        // `unconfigured`. Matches 2am's `load_policy`, which skips only
        // `FileNotFoundError`.
        if let Some(p) = &self.machine_path {
            match std::fs::symlink_metadata(p) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                _ => out.push(Candidate {
                    origin: Origin::Machine,
                    path: p.clone(),
                }),
            }
        }
        if let Some(p) = &self.repo_path {
            out.push(Candidate {
                origin: Origin::Repo,
                path: p.clone(),
            });
        }
        out
    }
}

/// `forge.egress.policyPath` from the repo's effective config, resolved
/// against `repo_root` when relative.
#[must_use]
pub fn repo_policy_path(repo_root: &Path) -> Option<PathBuf> {
    let config = crate::config_resolver::resolve_effective_config(repo_root);
    let raw = crate::config_resolver::get_path(&config, REPO_POLICY_KEY)?.as_str()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(raw);
    Some(if p.is_absolute() {
        p
    } else {
        repo_root.join(p)
    })
}

/// A loaded policy plus where it came from.
#[derive(Debug, Clone)]
pub struct PolicyDoc {
    pub data: Value,
    pub path: PathBuf,
    pub origin: Origin,
    /// Lower-precedence candidates that were present and ignored.
    pub ignored: Vec<Candidate>,
}

/// The outcome of resolution.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// No candidate is present: every entry point is a no-op returning 0.
    Unconfigured,
    Loaded(PolicyDoc),
    /// The winning candidate could not be read or parsed. Exit 2 — never a
    /// fall-through to a cached, default or narrower policy.
    Unreadable {
        candidate: Candidate,
        error: String,
        ignored: Vec<Candidate>,
    },
}

/// Resolve the policy from `sources`.
#[must_use]
pub fn resolve(sources: &PolicySources) -> Resolution {
    let mut present = sources.present().into_iter();
    let Some(winner) = present.next() else {
        return Resolution::Unconfigured;
    };
    let ignored: Vec<Candidate> = present.collect();
    let text = match std::fs::read_to_string(&winner.path) {
        Ok(t) => t,
        Err(e) => {
            return Resolution::Unreadable {
                candidate: winner,
                error: format!("{:?}", e.kind()),
                ignored,
            }
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(data) => Resolution::Loaded(PolicyDoc {
            data,
            path: winner.path,
            origin: winner.origin,
            ignored,
        }),
        Err(e) => Resolution::Unreadable {
            candidate: winner,
            // Category + position only: a parse error message can quote input.
            error: format!("JSONDecodeError ({:?} at line {})", e.classify(), e.line()),
            ignored,
        },
    }
}

/// Walk `keys` into `obj`.
#[must_use]
pub fn dig<'a>(obj: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().try_fold(obj, |cur, k| cur.get(*k))
}

/// [`dig`] to a string, or `""`.
#[must_use]
pub fn dig_str<'a>(obj: &'a Value, keys: &[&str]) -> &'a str {
    dig(obj, keys).and_then(Value::as_str).unwrap_or("")
}

/// The bare hostname stock `gh`'s `api_host` must carry.
#[must_use]
pub fn expected_api_host(policy: &Value) -> String {
    let origin = dig_str(policy, &["github", "apiOrigin"]);
    let host = origin.strip_prefix("https://").unwrap_or(origin);
    let host = host.trim_end_matches('/');
    host.split('/').next().unwrap_or("").to_string()
}

/// `enforcement.api`: `true` for `observe` in a policy whose `schemaVersion`
/// this validator understands; anything else (an unknown version, a missing
/// or out-of-enum value) is treated as `required` — fail closed.
#[must_use]
pub fn is_observe_only(policy: &Value) -> bool {
    policy.get("schemaVersion").and_then(Value::as_u64) == Some(SCHEMA_VERSION)
        && dig_str(policy, &["enforcement", "api"]) == "observe"
}

/// Schema conformance plus the invariants a schema cannot express.
#[must_use]
pub fn assert_policy_shape(policy: &Value) -> Vec<Finding> {
    let version = policy.get("schemaVersion");
    if version.and_then(Value::as_u64) != Some(SCHEMA_VERSION) {
        return vec![Finding::new(
            "policy.schema-version",
            "the policy schemaVersion is one this validator understands",
        )
        .expected(SCHEMA_VERSION.to_string())
        .observed(version.map_or_else(|| "None".to_string(), Value::to_string))
        .source("policy document")
        .remedy("upgrade the validator or re-render the policy at the supported version")
        .incomplete()];
    }
    let mut findings: Vec<Finding> = schema_errors(policy, &schema(), "$")
        .into_iter()
        .map(|error| {
            Finding::new("policy.schema", "the policy document conforms to policy.schema.json")
                .observed(error)
                .source("policy.schema.json (vendored from 2am infra/github-proxy, v1)")
                .remedy("fix the policy field named above")
        })
        .collect();

    if looks_like_secret(dig_str(policy, &["principal", "credentialRef"])) {
        findings.push(
            Finding::new(
                "policy.inline-secret",
                "principal.credentialRef is an external reference, never a secret",
            )
            .expected("a file:/keychain:/secret-manager reference")
            .observed("<redacted: token-shaped value>")
            .source("policy document")
            .remedy(
                "replace the inline credential with a reference and rotate the exposed credential",
            ),
        );
    }
    let host = expected_api_host(policy);
    if host.contains(':') {
        findings.push(
            Finding::new(
                "policy.api-origin-port",
                "apiOrigin is reachable by stock gh, whose api_host cannot carry a port",
            )
            .expected("an https origin on the default port")
            .observed(host.clone())
            .source("policy github.apiOrigin")
            .remedy(
                "serve the gateway on 443, or accept that only the managed adapter and SDK \
                 facades can reach it",
            ),
        );
    }
    let logical = dig_str(policy, &["github", "logicalHost"]);
    if !host.is_empty() && !logical.is_empty() && host == logical {
        findings.push(
            Finding::new(
                "policy.origin-equals-logical-host",
                "the API origin is distinguishable from the logical host",
            )
            .expected(format!("apiOrigin host != {logical}"))
            .observed(host)
            .source("policy github")
            .remedy(
                "a gateway that IS the logical host cannot be told apart from an unproxied \
                 call; use a distinct hostname",
            ),
        );
    }
    findings
}

/// The parsed vendored schema.
#[must_use]
pub fn schema() -> Value {
    serde_json::from_str(SCHEMA_JSON).unwrap_or(Value::Null)
}

fn type_ok(value: &Value, spec: &Value) -> bool {
    let names: Vec<&str> = match spec {
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        Value::String(s) => vec![s.as_str()],
        _ => return true,
    };
    names.iter().any(|name| match *name {
        "null" => value.is_null(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        _ => false,
    })
}

/// Python-`repr`-ish rendering for error text parity with 2am.
fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => format!("'{s}'"),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        other => other.to_string(),
    }
}

/// Validate `value` against the supported keyword subset of `schema`.
#[must_use]
pub fn schema_errors(value: &Value, schema: &Value, at: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if let Some(c) = schema.get("const") {
        if value != c {
            errors.push(format!("{at}: expected {}, got {}", py_repr(c), py_repr(value)));
        }
    }
    if let Some(Value::Array(options)) = schema.get("enum") {
        if !options.contains(value) {
            errors.push(format!(
                "{at}: {} not one of {}",
                py_repr(value),
                Value::Array(options.clone())
            ));
        }
    }
    if let Some(t) = schema.get("type") {
        if !type_ok(value, t) {
            errors.push(format!("{at}: expected type {t}, got {}", json_type_name(value)));
            return errors;
        }
    }
    if let Value::String(s) = value {
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
            if !Regex::new(pattern).is_ok_and(|re| re.is_match(s)) {
                errors.push(format!("{at}: '{s}' does not match {pattern}"));
            }
        }
        let len = s.chars().count() as u64;
        if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
            if len < min {
                errors.push(format!("{at}: shorter than minLength {min}"));
            }
        }
        if let Some(max) = schema.get("maxLength").and_then(Value::as_u64) {
            if len > max {
                errors.push(format!("{at}: longer than maxLength {max}"));
            }
        }
    }
    if let Some(n) = value.as_i64().filter(|_| !value.is_f64()) {
        if let Some(min) = schema.get("minimum").and_then(Value::as_i64) {
            if n < min {
                errors.push(format!("{at}: below minimum {min}"));
            }
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_i64) {
            if n > max {
                errors.push(format!("{at}: above maximum {max}"));
            }
        }
    }
    if let Value::Array(items) = value {
        if let Some(max) = schema.get("maxItems").and_then(Value::as_u64) {
            if items.len() as u64 > max {
                errors.push(format!("{at}: more than maxItems {max}"));
            }
        }
        if let Some(item_schema) = schema.get("items").filter(|s| s.is_object()) {
            for (i, item) in items.iter().enumerate() {
                errors.extend(schema_errors(item, item_schema, &format!("{at}[{i}]")));
            }
        }
    }
    if let Value::Object(map) = value {
        let props = schema.get("properties").and_then(Value::as_object);
        if let Some(Value::Array(required)) = schema.get("required") {
            for key in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(key) {
                    errors.push(format!("{at}: missing required property '{key}'"));
                }
            }
        }
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            for key in map.keys() {
                if !props.is_some_and(|p| p.contains_key(key)) {
                    errors.push(format!("{at}: unexpected property '{key}'"));
                }
            }
        }
        if let Some(props) = props {
            for (key, sub) in props {
                if let (Some(v), true) = (map.get(key), sub.is_object()) {
                    errors.extend(schema_errors(v, sub, &format!("{at}.{key}")));
                }
            }
        }
    }
    errors
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
