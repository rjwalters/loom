//! Forge-egress `--disallowedTools` specs (#9989 slice 2, C6 of epic #9983).
//!
//! Slice 1 added the `loom:forge-egress` `PreToolUse` rule, whose classifier
//! is [`crate::forge_egress::guard`]. Hooks do not fire in a
//! `bypassPermissions` run (guard-hooks.md "Machine-Level Execution"), so this
//! module renders the **same bypass classes** — [`Bypass::ALL`], one spec
//! table per class — as Claude Code permission-deny specs, which the CLI
//! enforces on its own.
//!
//! # Gating
//!
//! The specs exist only under a policy that turns the guard on — the very same
//! predicate, [`guard::enforced`]: a loaded policy whose `enforcement.api` is
//! not `observe` (the fail-closed reading of `required`). No policy, an
//! unreadable one, or an `observe` one ⇒ **no specs at all**, so an upstream
//! user sees byte-for-byte the output they saw before. A daemon too old to have
//! this module emits none either, which is the same answer.
//!
//! The `guards.forgeEgress` toggle is deliberately **not** consulted: it is a
//! repo-level opt-out of the *hook*, and a repo cannot waive a machine policy's
//! `required` (the issue's scope 1: "the policy's `required` is still enforced
//! at runtime").
//!
//! # What a deny spec can and cannot express
//!
//! A spec is a glob over the command text (`*` anywhere, `:*` as a trailing
//! prefix form — the shapes `defaults/.claude/settings.json` already relies
//! on). It cannot mask a `--body` / commit-message mention the way the hook
//! does, so every spec is **anchored on the command word** (`curl …`,
//! `gh api https://…`) rather than on a bare substring — `gh pr create --body
//! "see api.github.com"` must stay allowed. That makes this layer a backstop
//! for the direct spellings; the wrapper spellings (`bash -c "…"`, `eval`,
//! `echo … | sh`) and the long tail stay the hook's job.
//!
//! `PathQualifiedGh` lists the common install locations; the policy's own
//! `toolchain.launcherPath` (trusted origins only, exactly as the hook reads
//! it) is dropped from that list so the managed launcher is never denied.

use crate::forge_egress::guard::{self, Bypass};
use crate::forge_egress::policy::Resolution;

/// The deny specs for one bypass class, in emission order. Every entry is
/// asserted literally, with a matching example command, by the golden tests.
#[must_use]
pub fn specs_for(class: Bypass) -> &'static [&'static str] {
    match class {
        Bypass::CanonicalApiClient => &[
            "Bash(curl *api.github.com*)",
            "Bash(curl *uploads.github.com*)",
            "Bash(wget *api.github.com*)",
            "Bash(wget *uploads.github.com*)",
            "Bash(http *api.github.com*)",
            "Bash(https *api.github.com*)",
            "Bash(xh *api.github.com*)",
            "Bash(xhs *api.github.com*)",
            "Bash(python* *api.github.com*)",
            "Bash(node *api.github.com*)",
        ],
        Bypass::GhApiAbsoluteUrl => &["Bash(gh api https://*)", "Bash(gh api http://*)"],
        Bypass::GhHostEnv => &["Bash(GH_HOST=*)", "Bash(export GH_HOST=*)"],
        Bypass::GhConfigDirEnv => &["Bash(GH_CONFIG_DIR=*)", "Bash(export GH_CONFIG_DIR=*)"],
        Bypass::GhHostnameFlag => &["Bash(gh * --hostname*)"],
        Bypass::GhConfigApiHost => &["Bash(gh config set *api_host*)"],
        // The first two are also `forge-secrets` specs; the merge dedupes.
        Bypass::GhAuthMutation => &[
            "Bash(gh auth login:*)",
            "Bash(gh auth refresh:*)",
            "Bash(gh auth setup-git:*)",
        ],
        Bypass::EnvClearedGh => &[
            "Bash(env -i *gh *)",
            "Bash(env - *gh *)",
            "Bash(env --ignore-environment *gh *)",
        ],
        Bypass::PathQualifiedGh => &PATH_QUALIFIED_GH,
        Bypass::Sdk => &[
            "Bash(pip* *PyGithub*)",
            "Bash(pip* *pygithub*)",
            "Bash(uv *PyGithub*)",
            "Bash(uv *pygithub*)",
            "Bash(python* *PyGithub*)",
            "Bash(python* *pygithub*)",
            "Bash(python* *import github*)",
            "Bash(python* *from github import*)",
            "Bash(npm *octokit*)",
            "Bash(npx *octokit*)",
            "Bash(pnpm *octokit*)",
            "Bash(yarn *octokit*)",
            "Bash(bun *octokit*)",
            "Bash(node *octokit*)",
            "Bash(cargo *octocrab*)",
            "Bash(go *go-github*)",
        ],
    }
}

/// Path-qualified `gh` binaries, as `(path, spec)` so the launcher filter
/// compares paths, not spec strings.
const PATH_QUALIFIED_GH_PATHS: [(&str, &str); 7] = [
    ("/usr/bin/gh", "Bash(/usr/bin/gh:*)"),
    ("/usr/local/bin/gh", "Bash(/usr/local/bin/gh:*)"),
    ("/opt/homebrew/bin/gh", "Bash(/opt/homebrew/bin/gh:*)"),
    ("/home/linuxbrew/.linuxbrew/bin/gh", "Bash(/home/linuxbrew/.linuxbrew/bin/gh:*)"),
    ("/snap/bin/gh", "Bash(/snap/bin/gh:*)"),
    ("~/bin/gh", "Bash(~/bin/gh:*)"),
    ("~/.local/bin/gh", "Bash(~/.local/bin/gh:*)"),
];

const PATH_QUALIFIED_GH: [&str; 7] = [
    PATH_QUALIFIED_GH_PATHS[0].1,
    PATH_QUALIFIED_GH_PATHS[1].1,
    PATH_QUALIFIED_GH_PATHS[2].1,
    PATH_QUALIFIED_GH_PATHS[3].1,
    PATH_QUALIFIED_GH_PATHS[4].1,
    PATH_QUALIFIED_GH_PATHS[5].1,
    PATH_QUALIFIED_GH_PATHS[6].1,
];

/// The forge-egress deny specs for `resolution`, in [`Bypass::ALL`] order —
/// empty unless [`guard::enforced`] says the policy turns the guard on.
#[must_use]
pub fn deny_specs(resolution: &Resolution) -> Vec<&'static str> {
    let Some(enforced) = guard::enforced(resolution) else {
        return Vec::new();
    };
    let launcher_spec = enforced.launcher.as_deref().and_then(|l| {
        PATH_QUALIFIED_GH_PATHS
            .iter()
            .find(|(path, _)| *path == l)
            .map(|(_, spec)| *spec)
    });
    Bypass::ALL
        .iter()
        .flat_map(|class| specs_for(*class).iter().copied())
        .filter(|spec| Some(*spec) != launcher_spec)
        .collect()
}

/// `role` specs followed by `egress` specs, each spec once, first position
/// wins — so a role restriction's order is untouched and the overlap with
/// `forge-secrets` is not emitted twice.
#[must_use]
pub fn merge(role: Vec<&'static str>, egress: &[&'static str]) -> Vec<&'static str> {
    let mut out = role;
    for spec in egress {
        if !out.contains(spec) {
            out.push(spec);
        }
    }
    out
}

#[cfg(test)]
#[path = "forge_egress_tests.rs"]
mod tests;
