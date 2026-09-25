//! Per-provider profile **sharing rules** — the data half of pooled-profile
//! provisioning (issue #8672).
//!
//! # Why this is data, not code
//!
//! Loom rotates Codex accounts by pointing `CODEX_HOME` at a per-account
//! profile directory, and issue #8628 proposes the same for Kimi via
//! `KIMI_CODE_HOME`. The CLIs read *everything* from that directory —
//! `AGENTS.md`, `config.toml`, `prompts/`, hooks, MCP servers — so a freshly
//! created profile is a blank install and a pooled account behaves unlike the
//! operator's own.
//!
//! Populating it is the same operation for every such provider; only the
//! *surface list* differs. So the provider-specific part lives here as a
//! [`ProviderProfileRules`] table and [`super::profile_provisioning`] contains
//! no provider name at all — adding Kimi is a new `const` in this file plus a
//! row in [`PROVIDER_RULES`], not a new code path. The provisioner's own test
//! suite proves that by driving a synthetic non-Codex provider through the
//! identical entry point.
//!
//! # The rules themselves
//!
//! | Surface | [`ShareMode`] | Why |
//! |---|---|---|
//! | Capability dirs (`prompts/`, `skills/`, …) | [`ShareMode::Symlink`] | live share — anything installed later needs no re-sync |
//! | Session/history trees | [`ShareMode::Symlink`] | append-only, so one `--resume` history across accounts is safe |
//! | Plain files (`AGENTS.md`) | [`ShareMode::Copy`] | the CLIs write tmp-then-rename, which would *replace* a symlink with a regular file and silently fork the shared original |
//! | Settings documents (`config.toml`) | [`ShareMode::MergeToml`] / [`ShareMode::MergeJson`] | share configuration key by key, never wholesale — a copy would drag identity and trust state along with it |
//! | Credentials, identity, per-project trust | *absent from the table*, plus [`ProviderProfileRules::never_share`] | never read, never written |
//!
//! The design is superset-sh/superset's (`packages/agent-setup/src/
//! provider-profiles.ts`, `profile-sharing.ts`, Elastic License 2.0) —
//! referenced for its rules, not copied.

use std::path::{Component, Path};

/// How one named surface of a provider profile is shared with a pooled
/// profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareMode {
    /// Replace the pooled profile's entry with a symlink to the default
    /// profile's directory. Live share: content installed into the default
    /// later is visible immediately, with nothing to re-provision.
    Symlink,
    /// Copy the file's bytes. Never a symlink: a CLI that writes
    /// tmp-then-rename would replace the link with a regular file and
    /// silently fork the *shared original* into a private copy.
    Copy,
    /// Merge the default's top-level keys into the pooled profile's JSON
    /// document, honouring [`ProviderProfileRules::key_denylist`].
    MergeJson,
    /// As [`ShareMode::MergeJson`], for a TOML document.
    MergeToml,
}

impl ShareMode {
    /// `true` for the modes that rewrite a settings document key by key.
    #[must_use]
    pub fn is_merge(self) -> bool {
        matches!(self, ShareMode::MergeJson | ShareMode::MergeToml)
    }
}

/// One entry of a provider's sharing table: a profile-relative path and the
/// mechanism that populates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SurfaceRule {
    /// Path relative to the profile root. Always a plain single segment or a
    /// `/`-joined relative path — never absolute, never containing `..`
    /// (enforced by [`ProviderProfileRules::validate`]).
    pub path: &'static str,
    pub mode: ShareMode,
}

/// How a provider's **managed hook bridge** is installed into a pooled
/// profile, when it has one.
///
/// Loom does not write this file itself: the provider's own bridge
/// provisioner owns it, merges exactly one Loom-owned entry, and preserves
/// every operator entry. Provisioning just invokes it once per profile.
#[derive(Debug, Clone, Copy)]
pub struct HookBridgeSpec {
    /// Environment override naming the bridge provisioner script explicitly.
    pub script_env: &'static str,
    /// Workspace-relative candidate locations, highest precedence first
    /// (installed copy, then the Loom checkout's `defaults/`).
    pub script_candidates: &'static [&'static str],
    /// The subcommand that installs the managed entry.
    pub install_arg: &'static str,
    /// The flag naming the profile directory.
    pub home_flag: &'static str,
    /// The flag naming the workspace whose guard the bridge should invoke.
    pub workspace_flag: &'static str,
}

/// Everything provisioning needs to know about one provider.
#[derive(Debug, Clone, Copy)]
pub struct ProviderProfileRules {
    /// Registry provider name, lowercase (`"codex"`).
    pub provider: &'static str,
    /// The environment variable whose value the provider's CLI reads its
    /// profile directory from (`CODEX_HOME`; `KIMI_CODE_HOME` for #8628).
    /// Diagnostics only — the provisioner is told the source and target
    /// directories explicitly and never consults the live environment, so a
    /// daemon child that already has `CODEX_HOME` pointed at *another pooled
    /// profile* can never become a provisioning source.
    pub home_env: &'static str,
    /// Home-relative location of the provider's **default** (operator)
    /// profile — `.codex` for `~/.codex`.
    pub default_home_relative: &'static str,
    /// Environment override naming the default profile explicitly, for an
    /// operator whose own profile is not at the conventional location (and
    /// for tests, which must never reach the real one).
    pub default_home_env: &'static str,
    /// The sharing table, in application order.
    pub surfaces: &'static [SurfaceRule],
    /// File names the provisioner must never open, in the source or the
    /// target, at any point. Credentials live here.
    pub never_share: &'static [&'static str],
    /// Dotted key paths never merged out of the default profile's settings
    /// documents. A denied path also denies everything beneath it, so
    /// `"hooks.state"` covers `hooks.state."<id>".trusted_hash` while leaving
    /// the rest of `hooks` shareable.
    pub key_denylist: &'static [&'static str],
    /// The provider's managed hook bridge, when it has one.
    pub hook_bridge: Option<HookBridgeSpec>,
}

impl ProviderProfileRules {
    /// Reject a table that could escape the profile root or that contradicts
    /// its own `never_share` list. Called once per provisioning run, so a
    /// malformed table fails loudly at the entry point rather than mid-write.
    pub fn validate(&self) -> anyhow::Result<()> {
        for surface in self.surfaces {
            let path = Path::new(surface.path);
            if surface.path.is_empty() || path.is_absolute() {
                anyhow::bail!(
                    "provider {:?} sharing table: surface {:?} must be a non-empty relative path",
                    self.provider,
                    surface.path
                );
            }
            if path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            {
                anyhow::bail!(
                    "provider {:?} sharing table: surface {:?} must not contain `.` or `..`",
                    self.provider,
                    surface.path
                );
            }
            if self.is_never_shared(path) {
                anyhow::bail!(
                    "provider {:?} sharing table: surface {:?} is also listed as never-shared",
                    self.provider,
                    surface.path
                );
            }
        }
        Ok(())
    }

    /// `true` when any component of `relative` names a never-shared file.
    ///
    /// Component-wise rather than a whole-path comparison so a nested
    /// `sessions/auth.json` is refused as firmly as a top-level one — the
    /// provisioner uses this as a last-resort assertion before every open.
    #[must_use]
    pub fn is_never_shared(&self, relative: &Path) -> bool {
        relative.components().any(|component| match component {
            Component::Normal(name) => self
                .never_share
                .iter()
                .any(|denied| name.eq_ignore_ascii_case(denied)),
            _ => false,
        })
    }

    /// `true` when `dotted` is on the key denylist, or nested beneath a
    /// denied prefix.
    #[must_use]
    pub fn is_denied_key(&self, dotted: &str) -> bool {
        self.key_denylist.iter().any(|denied| {
            dotted == *denied
                || (dotted.len() > denied.len()
                    && dotted.starts_with(denied)
                    && dotted.as_bytes()[denied.len()] == b'.')
        })
    }
}

/// Codex (`CODEX_HOME`, `~/.codex`).
///
/// `hooks.json` is deliberately **absent** from `surfaces`: it is co-owned by
/// `defaults/scripts/provision-codex-hooks.sh`, the managed `pre_tool_use`
/// bridge writer (#4495), which merges exactly one Loom entry into it and
/// preserves every operator entry byte-for-byte. Provisioning invokes that
/// script instead of merging the file itself, so the profile keeps a single
/// managed writer rather than two with separate idempotency records.
pub const CODEX_RULES: ProviderProfileRules = ProviderProfileRules {
    provider: "codex",
    home_env: "CODEX_HOME",
    default_home_relative: ".codex",
    default_home_env: "LOOM_CODEX_DEFAULT_HOME",
    surfaces: &[
        // Capability directories — live share.
        SurfaceRule {
            path: "prompts",
            mode: ShareMode::Symlink,
        },
        SurfaceRule {
            path: "skills",
            mode: ShareMode::Symlink,
        },
        SurfaceRule {
            path: "plugins",
            mode: ShareMode::Symlink,
        },
        // Session/history trees — append-only, so one `--resume` history
        // across pooled accounts is safe and is the point.
        SurfaceRule {
            path: "sessions",
            mode: ShareMode::Symlink,
        },
        SurfaceRule {
            path: "archived_sessions",
            mode: ShareMode::Symlink,
        },
        // Repo instructions — copied, never linked (tmp-then-rename).
        SurfaceRule {
            path: "AGENTS.md",
            mode: ShareMode::Copy,
        },
        SurfaceRule {
            path: "instructions.md",
            mode: ShareMode::Copy,
        },
        // Settings — key-merged under the denylist below.
        SurfaceRule {
            path: "config.toml",
            mode: ShareMode::MergeToml,
        },
    ],
    // Codex owns `auth.json` and Loom treats it as opaque: never read, never
    // parsed, never copied (`account_lifecycle`'s standing rule). The `.bak`
    // spelling is listed because a refresh can leave one behind.
    never_share: &["auth.json", "auth.json.bak"],
    key_denylist: &[
        // Codex hook trust: `hooks.state."<identity>".trusted_hash`. Trust is
        // established per profile, interactively (#4495/#5005) — importing
        // another profile's hashes would fake a trust decision that never
        // happened for this one.
        "hooks.state",
        // Per-project trust state: `[projects."/path"] trust_level = …`.
        "projects",
        // Identity and credential material that can appear in config.toml.
        "account_id",
        "chatgpt_account_id",
        "preferred_auth_method",
        "api_key",
        "openai_api_key",
        "credentials",
        "auth",
        "tokens",
    ],
    hook_bridge: Some(HookBridgeSpec {
        script_env: "LOOM_CODEX_HOOKS_SCRIPT",
        script_candidates: &[
            ".loom/scripts/provision-codex-hooks.sh",
            "defaults/scripts/provision-codex-hooks.sh",
        ],
        install_arg: "install",
        home_flag: "--codex-home",
        workspace_flag: "--workspace",
    }),
};

/// Every provider whose pooled profiles Loom knows how to provision.
///
/// Kimi (`KIMI_CODE_HOME`) joins this list as one more `const` once #8628
/// lands its account pool and its profile layout is verified first-hand;
/// nothing in [`super::profile_provisioning`] changes when it does.
pub const PROVIDER_RULES: &[&ProviderProfileRules] = &[&CODEX_RULES];

/// Look a provider's sharing rules up by (case-insensitive) name.
#[must_use]
pub fn rules_for(provider: &str) -> Option<&'static ProviderProfileRules> {
    PROVIDER_RULES
        .iter()
        .copied()
        .find(|rules| rules.provider.eq_ignore_ascii_case(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_table_validates() {
        for rules in PROVIDER_RULES {
            rules.validate().unwrap_or_else(|error| {
                panic!("provider {} has an invalid sharing table: {error}", rules.provider)
            });
        }
    }

    #[test]
    fn codex_is_resolvable_case_insensitively() {
        assert_eq!(rules_for("codex").unwrap().provider, "codex");
        assert_eq!(rules_for("CoDeX").unwrap().provider, "codex");
        assert!(rules_for("claude").is_none());
    }

    #[test]
    fn auth_json_is_never_a_shared_surface() {
        for rules in PROVIDER_RULES {
            for surface in rules.surfaces {
                assert!(
                    !rules.is_never_shared(Path::new(surface.path)),
                    "{} shares never-shareable surface {}",
                    rules.provider,
                    surface.path
                );
            }
        }
        assert!(CODEX_RULES.is_never_shared(Path::new("auth.json")));
        assert!(CODEX_RULES.is_never_shared(Path::new("sessions/auth.json")));
        assert!(!CODEX_RULES.is_never_shared(Path::new("config.toml")));
    }

    #[test]
    fn denied_keys_cover_their_subtrees_but_not_their_siblings() {
        assert!(CODEX_RULES.is_denied_key("hooks.state"));
        assert!(CODEX_RULES.is_denied_key("hooks.state.abc.trusted_hash"));
        assert!(!CODEX_RULES.is_denied_key("hooks"));
        assert!(!CODEX_RULES.is_denied_key("hooks.stateful"));
        assert!(CODEX_RULES.is_denied_key("projects"));
        assert!(CODEX_RULES.is_denied_key("projects./home/me/repo.trust_level"));
        assert!(!CODEX_RULES.is_denied_key("model"));
    }

    #[test]
    fn validate_rejects_an_escaping_surface() {
        const BAD: ProviderProfileRules = ProviderProfileRules {
            provider: "bad",
            home_env: "BAD_HOME",
            default_home_relative: ".bad",
            default_home_env: "LOOM_BAD_DEFAULT_HOME",
            surfaces: &[SurfaceRule {
                path: "../escape",
                mode: ShareMode::Copy,
            }],
            never_share: &[],
            key_denylist: &[],
            hook_bridge: None,
        };
        assert!(BAD.validate().is_err());
    }

    #[test]
    fn validate_rejects_a_surface_that_is_also_never_shared() {
        const CONTRADICTORY: ProviderProfileRules = ProviderProfileRules {
            provider: "bad",
            home_env: "BAD_HOME",
            default_home_relative: ".bad",
            default_home_env: "LOOM_BAD_DEFAULT_HOME",
            surfaces: &[SurfaceRule {
                path: "auth.json",
                mode: ShareMode::Copy,
            }],
            never_share: &["auth.json"],
            key_denylist: &[],
            hook_bridge: None,
        };
        assert!(CONTRADICTORY.validate().is_err());
    }
}
