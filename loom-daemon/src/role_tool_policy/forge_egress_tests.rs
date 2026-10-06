//! Golden tests for the forge-egress `--disallowedTools` specs (#9989 slice 2).
//!
//! The spec strings are asserted literally (the parity discipline of the
//! parent module's tests), and every spec is paired with an example command
//! that (a) the spec matches under Claude Code's glob reading and (b) the
//! slice-1 hook classifier denies **as the same class** — so the two layers
//! cannot drift apart without a test failing.
#![allow(clippy::unwrap_used)]

use super::*;
use crate::forge_egress::guard::classify;
use crate::forge_egress::policy::{Origin, PolicyDoc};
use crate::role_tool_policy::{Allowlist, RoleToolPolicy};
use serde_json::json;
use std::path::PathBuf;

const LAUNCHER: &str = "/usr/local/bin/gh";

fn loaded(origin: Origin, api: &str) -> Resolution {
    Resolution::Loaded(PolicyDoc {
        data: json!({
            "schemaVersion": 1,
            "enforcement": {"api": api, "runtimeEgress": "unverified"},
            "toolchain": {"launcherPath": LAUNCHER},
        }),
        path: PathBuf::from("/etc/loom/forge-egress/policy.json"),
        origin,
        ignored: vec![],
    })
}

/// Claude Code's reading of a `Bash(…)` rule: `*` is any run of characters
/// anywhere, and a trailing `:*` is the legacy prefix form.
fn spec_matches(spec: &str, command: &str) -> bool {
    let inner = spec
        .strip_prefix("Bash(")
        .unwrap()
        .strip_suffix(')')
        .unwrap();
    let glob = inner
        .strip_suffix(":*")
        .map_or_else(|| inner.to_string(), |p| format!("{p}*"));
    glob_match(glob.as_bytes(), command.as_bytes())
}

fn glob_match(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|i| glob_match(rest, &s[i..])),
        Some((c, rest)) => s.first() == Some(c) && glob_match(rest, &s[1..]),
    }
}

/// One example per spec, in emission order (launcher path excluded).
const GOLDEN: &[(&str, Bypass, &str)] = &[
    (
        "Bash(curl *api.github.com*)",
        Bypass::CanonicalApiClient,
        "curl -sS https://api.github.com/zen",
    ),
    (
        "Bash(curl *uploads.github.com*)",
        Bypass::CanonicalApiClient,
        "curl https://uploads.github.com/x",
    ),
    (
        "Bash(wget *api.github.com*)",
        Bypass::CanonicalApiClient,
        "wget -qO- https://api.github.com/zen",
    ),
    (
        "Bash(wget *uploads.github.com*)",
        Bypass::CanonicalApiClient,
        "wget https://uploads.github.com/x",
    ),
    (
        "Bash(http *api.github.com*)",
        Bypass::CanonicalApiClient,
        "http GET api.github.com/user",
    ),
    (
        "Bash(https *api.github.com*)",
        Bypass::CanonicalApiClient,
        "https api.github.com/zen",
    ),
    (
        "Bash(xh *api.github.com*)",
        Bypass::CanonicalApiClient,
        "xh get api.github.com/zen",
    ),
    (
        "Bash(xhs *api.github.com*)",
        Bypass::CanonicalApiClient,
        "xhs get api.github.com/zen",
    ),
    (
        "Bash(python* *api.github.com*)",
        Bypass::CanonicalApiClient,
        "python3 -c 'import urllib.request as u; u.urlopen(\"https://api.github.com/zen\")'",
    ),
    (
        "Bash(node *api.github.com*)",
        Bypass::CanonicalApiClient,
        "node -e 'fetch(\"https://api.github.com/zen\")'",
    ),
    (
        "Bash(gh api https://*)",
        Bypass::GhApiAbsoluteUrl,
        "gh api https://api.github.com/repos/o/r",
    ),
    (
        "Bash(gh api http://*)",
        Bypass::GhApiAbsoluteUrl,
        "gh api http://example.test/repos/o/r",
    ),
    (
        "Bash(GH_HOST=*)",
        Bypass::GhHostEnv,
        "GH_HOST=github-proxy.example gh issue list",
    ),
    ("Bash(export GH_HOST=*)", Bypass::GhHostEnv, "export GH_HOST=other"),
    (
        "Bash(GH_CONFIG_DIR=*)",
        Bypass::GhConfigDirEnv,
        "GH_CONFIG_DIR=/tmp/x gh issue list",
    ),
    (
        "Bash(export GH_CONFIG_DIR=*)",
        Bypass::GhConfigDirEnv,
        "export GH_CONFIG_DIR=/tmp/x",
    ),
    (
        "Bash(gh * --hostname*)",
        Bypass::GhHostnameFlag,
        "gh auth status --hostname other.example",
    ),
    (
        "Bash(gh config set *api_host*)",
        Bypass::GhConfigApiHost,
        "gh config set api_host other.example",
    ),
    ("Bash(gh auth login:*)", Bypass::GhAuthMutation, "gh auth login --with-token"),
    ("Bash(gh auth refresh:*)", Bypass::GhAuthMutation, "gh auth refresh -s repo"),
    ("Bash(gh auth setup-git:*)", Bypass::GhAuthMutation, "gh auth setup-git"),
    ("Bash(env -i *gh *)", Bypass::EnvClearedGh, "env -i PATH=/usr/bin gh issue list"),
    ("Bash(env - *gh *)", Bypass::EnvClearedGh, "env - PATH=/usr/bin gh api user"),
    (
        "Bash(env --ignore-environment *gh *)",
        Bypass::EnvClearedGh,
        "env --ignore-environment PATH=/usr/bin gh api user",
    ),
    ("Bash(/usr/bin/gh:*)", Bypass::PathQualifiedGh, "/usr/bin/gh api user"),
    (
        "Bash(/opt/homebrew/bin/gh:*)",
        Bypass::PathQualifiedGh,
        "/opt/homebrew/bin/gh issue list",
    ),
    (
        "Bash(/home/linuxbrew/.linuxbrew/bin/gh:*)",
        Bypass::PathQualifiedGh,
        "/home/linuxbrew/.linuxbrew/bin/gh api user",
    ),
    ("Bash(/snap/bin/gh:*)", Bypass::PathQualifiedGh, "/snap/bin/gh api user"),
    ("Bash(~/bin/gh:*)", Bypass::PathQualifiedGh, "~/bin/gh api user"),
    ("Bash(~/.local/bin/gh:*)", Bypass::PathQualifiedGh, "~/.local/bin/gh api user"),
    ("Bash(pip* *PyGithub*)", Bypass::Sdk, "pip install PyGithub"),
    ("Bash(pip* *pygithub*)", Bypass::Sdk, "pip3 install pygithub"),
    ("Bash(uv *PyGithub*)", Bypass::Sdk, "uv add PyGithub"),
    ("Bash(uv *pygithub*)", Bypass::Sdk, "uv pip install pygithub"),
    ("Bash(python* *PyGithub*)", Bypass::Sdk, "python3 -m pip install PyGithub"),
    ("Bash(python* *pygithub*)", Bypass::Sdk, "python -m pip install pygithub"),
    (
        "Bash(python* *import github*)",
        Bypass::Sdk,
        "python3 -c 'import github; print(1)'",
    ),
    (
        "Bash(python* *from github import*)",
        Bypass::Sdk,
        "python3 -c 'from github import Github'",
    ),
    ("Bash(npm *octokit*)", Bypass::Sdk, "npm install @octokit/rest"),
    ("Bash(npx *octokit*)", Bypass::Sdk, "npx -p octokit node x.js"),
    ("Bash(pnpm *octokit*)", Bypass::Sdk, "pnpm add octokit"),
    ("Bash(yarn *octokit*)", Bypass::Sdk, "yarn add @octokit/rest"),
    ("Bash(bun *octokit*)", Bypass::Sdk, "bun add octokit"),
    ("Bash(node *octokit*)", Bypass::Sdk, "node -e 'require(\"@octokit/rest\")'"),
    ("Bash(cargo *octocrab*)", Bypass::Sdk, "cargo add octocrab"),
    ("Bash(go *go-github*)", Bypass::Sdk, "go get github.com/google/go-github/v60"),
];

/// Managed-path commands the issue requires stay allowed (its negative
/// fixtures), plus forge text that merely *mentions* a bypass.
const ALLOWED: &[&str] = &[
    "gh issue list",
    "gh api repos/o/r",
    "gh api graphql -f query=x",
    "gh auth status",
    "gh pr create --body 'see https://api.github.com/zen and GH_HOST='",
    "gh issue comment 1 --body 'pip install PyGithub was denied'",
    "git commit -m 'deny curl https://api.github.com'",
    "./.loom/scripts/merge-pr.sh 123",
    "./.loom/scripts/create-issue.sh --title x",
    "git push -u origin feature/issue-9989",
    "/usr/local/bin/gh api user",
    "curl -sS https://example.com/health",
];

#[test]
fn no_policy_emits_nothing() {
    assert!(deny_specs(&Resolution::Unconfigured).is_empty());
}

#[test]
fn observe_policy_emits_nothing() {
    assert!(deny_specs(&loaded(Origin::Machine, "observe")).is_empty());
}

#[test]
fn required_policy_emits_the_golden_list_in_order() {
    let got = deny_specs(&loaded(Origin::Machine, "required"));
    let want: Vec<&str> = GOLDEN.iter().map(|(spec, _, _)| *spec).collect();
    assert_eq!(got, want);
}

#[test]
fn every_class_has_at_least_one_spec() {
    let got = deny_specs(&loaded(Origin::Env, "required"));
    for class in Bypass::ALL {
        assert!(
            specs_for(class).iter().any(|s| got.contains(s)),
            "{class:?} has no --disallowedTools spec"
        );
    }
}

#[test]
fn each_spec_matches_its_example_and_the_hook_agrees_on_the_class() {
    for (spec, class, example) in GOLDEN {
        assert!(spec_matches(spec, example), "{spec} does not match {example:?}");
        assert_eq!(
            classify(example, Some(LAUNCHER)),
            Some(*class),
            "hook classifier disagrees for {example:?} ({spec})"
        );
    }
}

#[test]
fn managed_commands_and_mere_mentions_are_not_denied() {
    let specs = deny_specs(&loaded(Origin::Machine, "required"));
    for command in ALLOWED {
        let hit: Vec<_> = specs.iter().filter(|s| spec_matches(s, command)).collect();
        assert!(hit.is_empty(), "{command:?} denied by {hit:?}");
    }
}

#[test]
fn the_trusted_launcher_is_never_denied() {
    let machine = deny_specs(&loaded(Origin::Machine, "required"));
    assert!(!machine.contains(&"Bash(/usr/local/bin/gh:*)"));
    // A repo-local policy may not choose an executable, so nothing is exempt.
    let repo = deny_specs(&loaded(Origin::Repo, "required"));
    assert!(repo.contains(&"Bash(/usr/local/bin/gh:*)"));
    assert_eq!(repo.len(), machine.len() + 1);
}

#[test]
fn merge_keeps_role_order_and_dedupes_the_forge_secrets_overlap() {
    let role = RoleToolPolicy {
        role: "curator".to_string(),
        source: None,
        allowlist: Allowlist::Declared(vec![]),
    };
    let role_specs = role.deny_specs();
    assert_eq!(role_specs.len(), 38);
    let egress = deny_specs(&loaded(Origin::Machine, "required"));
    let merged = merge(role_specs.clone(), &egress);
    assert_eq!(&merged[..38], &role_specs[..]);
    // login / refresh / setup-git are already forge-secrets specs.
    assert_eq!(merged.len(), 38 + egress.len() - 3);
    for spec in &egress {
        assert_eq!(merged.iter().filter(|s| *s == spec).count(), 1, "{spec}");
    }
    // No policy: the role list passes through untouched.
    assert_eq!(merge(role_specs.clone(), &[]), role_specs);
}
