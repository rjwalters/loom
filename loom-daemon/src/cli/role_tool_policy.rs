//! `loom-daemon role-tool-policy` — the shell-facing half of the per-role
//! tool-restriction allowlist (Issue #8322, for #8256).
//!
//! [`loom_daemon::role_tool_policy`] is the mechanism; this is the surface the
//! two `contract`-category spawn scripts call instead of re-implementing it in
//! `jq` and bash. See that module's doc for the parity requirements. This one
//! documents the CLI contract those scripts depend on.
//!
//! # Verbs
//!
//! ```sh
//! # spawn-claude.sh: the --disallowedTools spec list, one per line.
//! loom-daemon role-tool-policy deny-specs \
//!     --workspace "$WORKSPACE" --roles-dir "$_script_dir/../roles"
//!
//! # spawn-codex.sh: does this role have a restrictive declaration that ONLY
//! # the guard hook can enforce? Exit 0 => emit the "enforcement NOT active"
//! # warning; stdout is the role JSON to name in it.
//! loom-daemon role-tool-policy restricted \
//!     --workspace "$WORKSPACE" --roles-dir "$_SCRIPT_DIR/../roles"
//!
//! # The canonical role-file basename, aliases resolved.
//! loom-daemon role-tool-policy resolve-name development-worker   # -> builder
//! ```
//!
//! The role argument is optional and defaults to `$LOOM_ROLE`, because that is
//! what every caller passes and an empty positional is an easy way to hand the
//! wrong thing to a security control.
//!
//! # Exit-code contract
//!
//! | Verb | `0` | `1` |
//! |---|---|---|
//! | `resolve-name` | canonical name on stdout | the name is not usable as a role-file basename |
//! | `deny-specs` | at least one spec on stdout | no restriction applies — emit nothing |
//! | `restricted` | restrictive declaration; role JSON path on stdout | unrestricted, undeclared, or unresolvable |
//!
//! **`1` is an answer, not an error**, and that is what makes the call sites
//! safe. Both scripts' degradation contract is "degrade to a byte-for-byte
//! no-op if the binary is unavailable or the call fails" — so a `1` for "no
//! restriction applies" lands on exactly the same branch as a missing binary,
//! and a restriction that could not be computed never stops a worker from
//! starting. The guard-hook backstop enforces the same declaration either way.
//!
//! `--json` prints the full record on **both** exit codes, for a caller that
//! wants the reason rather than just the verdict.

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::role_tool_policy::{resolve_role_name, RoleToolPolicy};

/// No restriction applies, or the role could not be resolved. Not an error.
pub(crate) const EX_NO_RESTRICTION: i32 = 1;

#[derive(clap::Subcommand)]
pub(crate) enum RoleToolPolicyCommand {
    /// Canonical role-file basename for a `LOOM_ROLE` value: case/underscore
    /// folded, the three daemon dispatch aliases resolved, and `/`/`.`-bearing
    /// names rejected. Exit 1 = not usable as a role-file basename.
    ResolveName(ResolveNameArgs),

    /// The `--disallowedTools` specs denying every capability the role does
    /// not declare, one per line. Empty + exit 1 when no restriction applies.
    DenySpecs(PolicyArgs),

    /// Whether the role declares a restrictive allowlist (an array with no
    /// `"*"`). Exit 0 + the role JSON path on stdout when it does.
    Restricted(PolicyArgs),
}

#[derive(clap::Args)]
pub(crate) struct ResolveNameArgs {
    /// The raw role name. Defaults to `$LOOM_ROLE`.
    #[arg(value_name = "ROLE")]
    pub role: Option<String>,
}

#[derive(clap::Args)]
pub(crate) struct PolicyArgs {
    /// The raw role name. Defaults to `$LOOM_ROLE`.
    #[arg(value_name = "ROLE")]
    pub role: Option<String>,

    /// Workspace whose `.loom/roles/<role>.json` is consulted first — the
    /// same first candidate `spawn-claude.sh` checks. Defaults to
    /// `$WORKSPACE`, then the current directory.
    #[arg(long, value_name = "PATH")]
    pub workspace: Option<PathBuf>,

    /// A roles directory to fall back to, repeatable, consulted in the order
    /// given after `--workspace`. The spawn scripts pass their own
    /// `<script dir>/../roles`.
    #[arg(long = "roles-dir", value_name = "PATH")]
    pub roles_dir: Vec<PathBuf>,

    /// Print the full resolution record as JSON instead of the bare answer.
    /// Printed on both exit codes.
    #[arg(long)]
    pub json: bool,
}

impl PolicyArgs {
    /// Resolve the acting role's policy, or `None` when the role name itself
    /// is unusable.
    fn resolve(&self) -> Option<RoleToolPolicy> {
        let raw = self
            .role
            .clone()
            .or_else(|| std::env::var("LOOM_ROLE").ok())
            .unwrap_or_default();
        let role = resolve_role_name(&raw)?;

        let workspace = self
            .workspace
            .clone()
            .or_else(|| std::env::var_os("WORKSPACE").map(PathBuf::from))
            .or_else(|| std::env::current_dir().ok());

        let candidates =
            RoleToolPolicy::candidate_paths(&role, workspace.as_deref(), &self.roles_dir);
        Some(RoleToolPolicy::load(&role, &candidates))
    }
}

/// The JSON record both policy verbs emit under `--json`.
///
/// `declared` is reported separately from `allowed` on purpose: an empty
/// `allowed` with `declared: true` is a fully-restricted role, and with
/// `declared: false` an unrestricted one. A consumer that reads only `allowed`
/// cannot tell those apart — the exact conflation this port exists to avoid.
fn record(policy: Option<&RoleToolPolicy>, raw_role: &str) -> serde_json::Value {
    match policy {
        None => serde_json::json!({
            "role": serde_json::Value::Null,
            "rawRole": raw_role,
            "roleJson": serde_json::Value::Null,
            "declared": false,
            "allowed": [],
            "denied": [],
            "specs": [],
            "restricted": false,
        }),
        Some(p) => serde_json::json!({
            "role": p.role,
            "rawRole": raw_role,
            "roleJson": p.source.as_ref().map(|s| s.display().to_string()),
            "declared": p.allowlist.is_declared(),
            "allowed": p.allowlist.capabilities(),
            "denied": p.denied_capabilities(),
            "specs": p.deny_specs(),
            "restricted": p.is_restricted(),
        }),
    }
}

fn raw_role_of(explicit: Option<&String>) -> String {
    explicit
        .cloned()
        .or_else(|| std::env::var("LOOM_ROLE").ok())
        .unwrap_or_default()
}

impl RoleToolPolicyCommand {
    pub(crate) fn run(self) -> Result<()> {
        let code = match self {
            RoleToolPolicyCommand::ResolveName(args) => {
                let raw = raw_role_of(args.role.as_ref());
                match resolve_role_name(&raw) {
                    Some(name) => {
                        println!("{name}");
                        0
                    }
                    None => EX_NO_RESTRICTION,
                }
            }
            RoleToolPolicyCommand::DenySpecs(args) => {
                let policy = args.resolve();
                let specs = policy
                    .as_ref()
                    .map(RoleToolPolicy::deny_specs)
                    .unwrap_or_default();
                if args.json {
                    println!("{}", record(policy.as_ref(), &raw_role_of(args.role.as_ref())));
                } else {
                    for spec in &specs {
                        println!("{spec}");
                    }
                }
                if specs.is_empty() {
                    EX_NO_RESTRICTION
                } else {
                    0
                }
            }
            RoleToolPolicyCommand::Restricted(args) => {
                let policy = args.resolve();
                let restricted = policy.as_ref().is_some_and(RoleToolPolicy::is_restricted);
                if args.json {
                    println!("{}", record(policy.as_ref(), &raw_role_of(args.role.as_ref())));
                } else if restricted {
                    // The warning has to name the file that declared the
                    // policy, or an operator has nothing to act on.
                    if let Some(source) = policy.as_ref().and_then(|p| p.source.as_ref()) {
                        println!("{}", source.display());
                    }
                }
                if restricted {
                    0
                } else {
                    EX_NO_RESTRICTION
                }
            }
        };
        std::process::exit(code);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use loom_daemon::role_tool_policy::Allowlist;

    fn args(role: &str, dir: &std::path::Path) -> PolicyArgs {
        PolicyArgs {
            role: Some(role.to_string()),
            // A workspace that cannot contain a roles dir, so the test is
            // driven entirely by `--roles-dir` and never by the ambient
            // checkout the suite happens to run in.
            workspace: Some(dir.join("no-such-workspace")),
            roles_dir: vec![dir.to_path_buf()],
            json: false,
        }
    }

    fn write_role(dir: &std::path::Path, role: &str, body: &str) {
        std::fs::write(dir.join(format!("{role}.json")), body).unwrap();
    }

    #[test]
    fn resolves_through_a_dispatch_alias_to_the_target_roles_json() {
        let dir = tempfile::tempdir().unwrap();
        write_role(dir.path(), "builder", r#"{"toolPolicy":{"allowedCapabilities":["*"]}}"#);
        // `development-worker` has no role JSON of its own; the alias is what
        // makes it read builder.json.
        let p = args("development-worker", dir.path()).resolve().unwrap();
        assert_eq!(p.role, "builder");
        assert_eq!(p.allowlist, Allowlist::Declared(vec!["*".to_string()]));
        assert!(p.deny_specs().is_empty());
    }

    #[test]
    fn a_rejected_role_name_resolves_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(args("../doctor", dir.path()).resolve().is_none());
        assert!(args("", dir.path()).resolve().is_none());
    }

    #[test]
    fn a_declared_empty_role_emits_the_whole_spec_set() {
        let dir = tempfile::tempdir().unwrap();
        write_role(dir.path(), "curator", r#"{"toolPolicy":{"allowedCapabilities":[]}}"#);
        let p = args("Curator", dir.path()).resolve().unwrap();
        assert_eq!(p.deny_specs().len(), 38);
        assert!(p.is_restricted());
    }

    #[test]
    fn a_role_json_with_no_tool_policy_emits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write_role(dir.path(), "curator", r#"{"name":"curator"}"#);
        let p = args("curator", dir.path()).resolve().unwrap();
        assert!(p.deny_specs().is_empty());
        assert!(!p.is_restricted());
    }

    #[test]
    fn json_record_keeps_declared_separate_from_allowed() {
        let dir = tempfile::tempdir().unwrap();
        write_role(dir.path(), "curator", r#"{"toolPolicy":{"allowedCapabilities":[]}}"#);
        let restricted = args("curator", dir.path()).resolve().unwrap();
        let v = record(Some(&restricted), "curator");
        assert_eq!(v["declared"], serde_json::json!(true));
        assert_eq!(v["allowed"], serde_json::json!([]));
        assert_eq!(v["restricted"], serde_json::json!(true));
        assert_eq!(v["specs"].as_array().unwrap().len(), 38);

        write_role(dir.path(), "guide", r#"{"name":"guide"}"#);
        let undeclared = args("guide", dir.path()).resolve().unwrap();
        let v = record(Some(&undeclared), "guide");
        // Same empty `allowed`, opposite meaning — distinguishable only via
        // `declared`.
        assert_eq!(v["declared"], serde_json::json!(false));
        assert_eq!(v["allowed"], serde_json::json!([]));
        assert_eq!(v["restricted"], serde_json::json!(false));
        assert_eq!(v["specs"], serde_json::json!([]));
    }

    #[test]
    fn json_record_for_an_unresolvable_role_is_inert() {
        let v = record(None, "../doctor");
        assert_eq!(v["role"], serde_json::Value::Null);
        assert_eq!(v["rawRole"], serde_json::json!("../doctor"));
        assert_eq!(v["declared"], serde_json::json!(false));
        assert_eq!(v["restricted"], serde_json::json!(false));
        assert_eq!(v["specs"], serde_json::json!([]));
    }

    #[test]
    fn the_workspace_candidate_wins_over_a_roles_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let ws_roles = ws.join(".loom").join("roles");
        std::fs::create_dir_all(&ws_roles).unwrap();
        write_role(&ws_roles, "judge", r#"{"toolPolicy":{"allowedCapabilities":[]}}"#);
        write_role(dir.path(), "judge", r#"{"toolPolicy":{"allowedCapabilities":["*"]}}"#);

        let p = PolicyArgs {
            role: Some("judge".to_string()),
            workspace: Some(ws),
            roles_dir: vec![dir.path().to_path_buf()],
            json: false,
        }
        .resolve()
        .unwrap();
        assert_eq!(p.source.as_ref().unwrap(), &ws_roles.join("judge.json"));
        assert_eq!(p.deny_specs().len(), 38);
    }
}
