//! Per-role tool-restriction policy: role-name resolution, allowlist parsing,
//! and the `--disallowedTools` deny-spec computation (Issue #8322, for #8256).
//!
//! # Why this lives in the daemon
//!
//! Issue #8256 gives every role a declared allowlist of **sensitive
//! capabilities** in its own role JSON:
//!
//! ```json
//! { "toolPolicy": { "allowedCapabilities": [] } }      // read-only roles
//! { "toolPolicy": { "allowedCapabilities": ["*"] } }   // builder/doctor/…
//! ```
//!
//! Two consumers turn that declaration into an actual restriction at
//! session-spawn time, and both are `contract`-category shell scripts:
//!
//! - `defaults/scripts/spawn-claude.sh` converts it into the
//!   `--disallowedTools` spec list the Claude CLI understands.
//! - `defaults/scripts/spawn-codex.sh` only needs the *predicate* — the Codex
//!   CLI has no `--disallowedTools` equivalent, so when the guard-hook
//!   backstop is not installed/trusted there is no enforcement on that path at
//!   all, and the script must say so at launch.
//!
//! Implementing that inline in both scripts added +110 lines of **portable
//! shell** in the `contract` category, which is exactly what epic #7810 is
//! retiring — `loom-daemon shell-budget --check` refused PR #8314 for it, with
//! no commit-trailer escape hatch (portable growth is deliberately not
//! overridable). This module is the gate's own recommended fix: the logic
//! lands once, natively, and both scripts shrink to a call-out.
//!
//! # Byte-for-byte parity is the requirement
//!
//! This is a security control. Under-restricting silently grants a persuaded
//! role `ssh`/`aws`/`gh secret`; over-restricting silently breaks a role that
//! legitimately needs them. Every behaviour below — the three dispatch
//! aliases, the `/`/`.` rejection, the first-readable-candidate rule, the
//! undeclared-vs-declared-empty distinction, the spec strings and their order
//! — mirrors the shell it replaces exactly, and the unit tests assert the
//! strings literally rather than deriving them.
//!
//! # The one deliberate asymmetry: two wildcard rules
//!
//! The two consumers do **not** agree on what counts as a wildcard, and this
//! module preserves that rather than quietly unifying it:
//!
//! - `spawn-claude.sh` space-joins the declared capabilities and tests
//!   `[[ "$caps" != *"*"* ]]` — a **substring** test, so any element
//!   *containing* `*` disables the restriction. [`RoleToolPolicy::deny_specs`]
//!   follows this.
//! - `spawn-codex.sh` asks jq for `index("*")` — an **exact element** match.
//!   [`RoleToolPolicy::is_restricted`] follows this.
//!
//! They differ only for a declared capability that contains `*` without being
//! `*` (say `"cloud-*"`), which is inert in the namespace either way. Changing
//! either rule here would be a behaviour change to a security control smuggled
//! into a port, so the port keeps both and names the difference instead.
//!
//! # Fails open on identity, closed on capability
//!
//! Nothing is restricted unless a role resolves **and** its JSON declares a
//! `toolPolicy.allowedCapabilities` array. An unresolvable role, a missing or
//! unreadable role file, malformed JSON, no `toolPolicy` key, or a wildcard
//! allowlist all yield "no restriction" — because restricting a role nobody
//! declared restricted would break consumer repos silently. But once an
//! allowlist *is* declared, it is an allowlist: every capability in the
//! namespace it does not name is denied, including one added later.

use std::path::{Path, PathBuf};

/// The capability namespace, in the order the guard documents it and the order
/// specs are emitted in.
///
/// A name outside this list can never be granted by a role JSON: an unknown
/// string in `allowedCapabilities` is inert, never a wildcard.
pub const CAPABILITY_NAMESPACE: [&str; 4] = [
    "remote-shell",
    "cloud-cli",
    "forge-secrets",
    "credential-store",
];

/// The three daemon dispatch aliases, resolved identically by
/// `spawn-claude.sh`'s `_loom_role_policy_name()`, `spawn-codex.sh`'s
/// `$_hook_role`, and `guard-destructive-generic.sh`'s `_role_policy_name()`.
///
/// They are kept in lockstep so one dispatch surface cannot land on a
/// different policy than another. `sweep-lifecycle` maps to `builder` because
/// a full `/loom:sweep` runs the Builder and Doctor phases in-process and
/// needs their capabilities.
const DISPATCH_ALIASES: [(&str, &str); 3] = [
    ("development-worker", "builder"),
    ("pr-fixer", "doctor"),
    ("sweep-lifecycle", "builder"),
];

/// `--disallowedTools` specs for the `remote-shell` capability.
const SPECS_REMOTE_SHELL: [&str; 9] = [
    "Bash(ssh:*)",
    "Bash(scp:*)",
    "Bash(sftp:*)",
    "Bash(ssh-add:*)",
    "Bash(ssh-agent:*)",
    "Bash(ssh-keygen:*)",
    "Bash(ssh-keyscan:*)",
    "Bash(ssh-copy-id:*)",
    "Bash(autossh:*)",
];

/// `--disallowedTools` specs for the `cloud-cli` capability.
const SPECS_CLOUD_CLI: [&str; 10] = [
    "Bash(aws:*)",
    "Bash(gcloud:*)",
    "Bash(az:*)",
    "Bash(doctl:*)",
    "Bash(flyctl:*)",
    "Bash(fly:*)",
    "Bash(wrangler:*)",
    "Bash(heroku:*)",
    "Bash(kubectl:*)",
    "Bash(eksctl:*)",
];

/// `--disallowedTools` specs for the `forge-secrets` capability.
///
/// `gh auth status` is deliberately absent — every role runs it.
const SPECS_FORGE_SECRETS: [&str; 7] = [
    "Bash(gh secret:*)",
    "Bash(gh variable:*)",
    "Bash(gh auth token:*)",
    "Bash(gh auth login:*)",
    "Bash(gh auth refresh:*)",
    "Bash(gh auth logout:*)",
    "Bash(gh auth setup-git:*)",
];

/// `--disallowedTools` specs for the `credential-store` capability.
///
/// Path-shaped specs cover the Read/Edit/Write tools, which is the half
/// `Bash(...)` prefix matching cannot express at all. The Bash half of this
/// capability is guard-hook-only by construction.
const SPECS_CREDENTIAL_STORE: [&str; 12] = [
    "Read(//~/.ssh/**)",
    "Edit(//~/.ssh/**)",
    "Write(//~/.ssh/**)",
    "Read(//~/.aws/**)",
    "Edit(//~/.aws/**)",
    "Write(//~/.aws/**)",
    "Read(//~/.gnupg/**)",
    "Edit(//~/.gnupg/**)",
    "Write(//~/.gnupg/**)",
    "Read(//~/.config/gh/**)",
    "Edit(//~/.config/gh/**)",
    "Write(//~/.config/gh/**)",
];

/// The `--disallowedTools` specs denying one capability, or an empty slice for
/// a name outside [`CAPABILITY_NAMESPACE`].
///
/// Mirrors `_loom_role_deny_specs()` in `spawn-claude.sh` exactly, including
/// the order within each capability.
#[must_use]
pub fn deny_specs_for(capability: &str) -> &'static [&'static str] {
    match capability {
        "remote-shell" => &SPECS_REMOTE_SHELL,
        "cloud-cli" => &SPECS_CLOUD_CLI,
        "forge-secrets" => &SPECS_FORGE_SECRETS,
        "credential-store" => &SPECS_CREDENTIAL_STORE,
        _ => &[],
    }
}

/// Canonicalize a raw `LOOM_ROLE` value into a role-file basename, or `None`
/// when it is not usable as one.
///
/// Mirrors `_loom_role_policy_name()`:
///
/// 1. `tr '[:upper:]_' '[:lower:]-'` — ASCII upper-case folds to lower-case
///    and `_` becomes `-`.
/// 2. `tr -d '[:space:]'` — every POSIX space character is deleted, not
///    collapsed (so `"  builder "` is `builder`, and `"pr fixer"` is
///    `prfixer`, not `pr-fixer`).
/// 3. The three [`DISPATCH_ALIASES`], applied *after* folding so
///    `Development_Worker` resolves.
/// 4. Rejection of the empty string and of any name containing `/` or `.` —
///    the value lands in a filesystem path, so `..`, a leading dot, and any
///    separator never get that far. The shell's `*.*` pattern already
///    subsumes a leading-dot name, which is why there is no separate check.
#[must_use]
pub fn resolve_role_name(raw: &str) -> Option<String> {
    let folded: String = raw
        .chars()
        // POSIX [:space:] in the C locale: space, \t, \n, \v, \f, \r.
        .filter(|c| !matches!(c, ' ' | '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r'))
        .map(|c| if c == '_' { '-' } else { c.to_ascii_lowercase() })
        .collect();

    let aliased = DISPATCH_ALIASES
        .iter()
        .find(|(from, _)| *from == folded)
        .map_or(folded.as_str(), |(_, to)| *to);

    if aliased.is_empty() || aliased.contains('/') || aliased.contains('.') {
        return None;
    }
    Some(aliased.to_string())
}

/// What a role JSON says about `toolPolicy.allowedCapabilities`.
///
/// The two variants must never be collapsed: `Undeclared` is *unrestricted*
/// and `Declared(vec![])` is *fully restricted*, and both render as the empty
/// string once joined — which is precisely why the shell reaches for `jq -e`'s
/// exit status rather than reading its stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Allowlist {
    /// No `toolPolicy`, no `allowedCapabilities` array, or unparseable JSON.
    /// The role is unrestricted.
    Undeclared,
    /// An `allowedCapabilities` array was present. Non-string elements are
    /// dropped (matching jq's `select(type == "string")`); an empty vector is
    /// a role that may reach nothing.
    Declared(Vec<String>),
}

impl Allowlist {
    /// `true` when an `allowedCapabilities` array was present at all.
    #[must_use]
    pub fn is_declared(&self) -> bool {
        matches!(self, Allowlist::Declared(_))
    }

    /// The declared capability names, or an empty slice when undeclared.
    #[must_use]
    pub fn capabilities(&self) -> &[String] {
        match self {
            Allowlist::Undeclared => &[],
            Allowlist::Declared(caps) => caps,
        }
    }
}

/// Parse `toolPolicy.allowedCapabilities` out of a role JSON document.
///
/// Every failure mode — invalid JSON, a non-object document, a non-object
/// `toolPolicy`, a missing or non-array `allowedCapabilities` — yields
/// [`Allowlist::Undeclared`], exactly as `jq -er`'s non-zero exit does in
/// `spawn-claude.sh`.
#[must_use]
pub fn parse_allowlist(json_text: &str) -> Allowlist {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json_text) else {
        return Allowlist::Undeclared;
    };
    // `.toolPolicy` on a non-object is a jq error, not a null.
    let Some(doc) = value.as_object() else {
        return Allowlist::Undeclared;
    };
    let caps = match doc.get("toolPolicy") {
        // Absent, or explicitly null: `.allowedCapabilities` on null is null,
        // whose type is not "array".
        None | Some(serde_json::Value::Null) => return Allowlist::Undeclared,
        Some(serde_json::Value::Object(policy)) => policy.get("allowedCapabilities"),
        // A scalar/array `toolPolicy` makes `.allowedCapabilities` a jq error.
        Some(_) => return Allowlist::Undeclared,
    };
    match caps {
        Some(serde_json::Value::Array(items)) => Allowlist::Declared(
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        ),
        _ => Allowlist::Undeclared,
    }
}

/// A resolved role plus the declaration that governs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleToolPolicy {
    /// The canonical role name ([`resolve_role_name`]).
    pub role: String,
    /// The role JSON the verdict came from — the file a deny/warning message
    /// must name so a reader knows where to change the declaration. `None`
    /// when no candidate was readable.
    pub source: Option<PathBuf>,
    /// What that file declared.
    pub allowlist: Allowlist,
}

impl RoleToolPolicy {
    /// Load the policy for an already-canonical `role` from the first
    /// **readable** candidate path.
    ///
    /// First-readable-wins, and the search stops there: a readable file whose
    /// JSON is malformed resolves to [`Allowlist::Undeclared`] rather than
    /// falling through to the next candidate. That is `spawn-claude.sh`'s
    /// behaviour, and it is the safe one — silently consulting a *different*
    /// role file because the intended one was corrupt is worse than being
    /// unrestricted and saying which file was read.
    #[must_use]
    pub fn load(role: &str, candidates: &[PathBuf]) -> Self {
        for candidate in candidates {
            let Ok(text) = std::fs::read_to_string(candidate) else {
                continue;
            };
            return Self {
                role: role.to_string(),
                source: Some(candidate.clone()),
                allowlist: parse_allowlist(&text),
            };
        }
        Self {
            role: role.to_string(),
            source: None,
            allowlist: Allowlist::Undeclared,
        }
    }

    /// The candidate role-JSON paths, in `spawn-claude.sh`'s order:
    /// `<workspace>/.loom/roles/<role>.json`, then each `roles_dir` sibling
    /// (`<dir>/<role>.json`) the caller supplied.
    #[must_use]
    pub fn candidate_paths(
        role: &str,
        workspace: Option<&Path>,
        roles_dirs: &[PathBuf],
    ) -> Vec<PathBuf> {
        let file = format!("{role}.json");
        let mut out = Vec::with_capacity(1 + roles_dirs.len());
        if let Some(ws) = workspace {
            out.push(ws.join(".loom").join("roles").join(&file));
        }
        out.extend(roles_dirs.iter().map(|d| d.join(&file)));
        out
    }

    /// `true` when any declared capability *contains* a `*`.
    ///
    /// `spawn-claude.sh`'s rule — a substring test against the space-joined
    /// allowlist. See the module doc for why this differs from
    /// [`Self::is_restricted`]'s.
    #[must_use]
    pub fn has_spawn_claude_wildcard(&self) -> bool {
        self.allowlist
            .capabilities()
            .iter()
            .any(|c| c.contains('*'))
    }

    /// `true` when the role's declaration is restrictive on the Codex path:
    /// an `allowedCapabilities` array is present and no element *is* `"*"`.
    ///
    /// This is the predicate `spawn-codex.sh` needs to decide whether to emit
    /// its "enforcement NOT active on this path" warning: a restrictive
    /// declaration that only the guard-hook backstop can enforce, on a runtime
    /// with no `--disallowedTools` equivalent, means a session without that
    /// hook has no per-role restriction at all.
    #[must_use]
    pub fn is_restricted(&self) -> bool {
        self.allowlist.is_declared() && !self.allowlist.capabilities().iter().any(|c| c == "*")
    }

    /// The capabilities in [`CAPABILITY_NAMESPACE`] this role does **not**
    /// declare, in namespace order — or empty when no restriction applies.
    #[must_use]
    pub fn denied_capabilities(&self) -> Vec<&'static str> {
        if !self.allowlist.is_declared() || self.has_spawn_claude_wildcard() {
            return Vec::new();
        }
        let declared = self.allowlist.capabilities();
        CAPABILITY_NAMESPACE
            .iter()
            .copied()
            .filter(|cap| !declared.iter().any(|d| d == cap))
            .collect()
    }

    /// The full `--disallowedTools` spec list for this role, in namespace
    /// order, or empty when no restriction applies.
    ///
    /// An empty list is the caller's signal to inject nothing at all — the
    /// byte-for-byte no-op half of `spawn-claude.sh`'s degradation contract.
    #[must_use]
    pub fn deny_specs(&self) -> Vec<&'static str> {
        self.denied_capabilities()
            .into_iter()
            .flat_map(|cap| deny_specs_for(cap).iter().copied())
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
