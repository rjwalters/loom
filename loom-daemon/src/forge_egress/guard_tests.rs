#![allow(clippy::unwrap_used)]

use super::*;
use crate::forge_egress::policy::{Candidate, PolicyDoc};
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

fn class(cmd: &str) -> Option<Bypass> {
    classify(cmd, Some(LAUNCHER))
}

/// Every class, plain and through each wrapper spelling the merge-redirect
/// rule sees through: `bash -c`, `sh -lc`, `eval`, `echo … | sh`.
#[test]
fn every_class_is_denied_plain_and_wrapped() {
    let cases: &[(Bypass, &str)] = &[
        (Bypass::CanonicalApiClient, "curl -sS https://api.github.com/zen"),
        (Bypass::CanonicalApiClient, "wget -qO- https://uploads.github.com/x"),
        (Bypass::CanonicalApiClient, "http GET api.github.com/user"),
        (Bypass::CanonicalApiClient, "xh api.github.com/zen"),
        (
            Bypass::CanonicalApiClient,
            "python3 -c 'import urllib.request; urllib.request.urlopen(\"https://api.github.com/zen\")'",
        ),
        (Bypass::GhApiAbsoluteUrl, "gh api https://api.github.com/repos/o/r"),
        (Bypass::GhApiAbsoluteUrl, "gh api HTTP://example.test/repos/o/r"),
        (Bypass::GhHostEnv, "GH_HOST=github-proxy.example gh issue list"),
        (Bypass::GhHostEnv, "export GH_HOST=other"),
        (Bypass::GhConfigDirEnv, "GH_CONFIG_DIR=/tmp/x gh issue list"),
        (Bypass::GhHostnameFlag, "gh api repos/o/r --hostname other.example"),
        (Bypass::GhHostnameFlag, "gh auth status --hostname=other.example"),
        (Bypass::GhConfigApiHost, "gh config set api_host other.example"),
        (Bypass::GhConfigApiHost, "gh config set -h github.com api_host x"),
        (Bypass::GhAuthMutation, "gh auth login --with-token"),
        (Bypass::GhAuthMutation, "gh auth setup-git"),
        (Bypass::GhAuthMutation, "gh auth refresh -s repo"),
        (Bypass::EnvClearedGh, "env -i PATH=/usr/bin gh issue list"),
        (Bypass::EnvClearedGh, "env --ignore-environment gh api user"),
        (Bypass::EnvClearedGh, "env - gh api user"),
        (Bypass::PathQualifiedGh, "/opt/homebrew/bin/gh issue list"),
        (Bypass::PathQualifiedGh, "~/bin/gh api user"),
        (Bypass::PathQualifiedGh, "exec /usr/bin/gh api user"),
        (Bypass::Sdk, "pip install PyGithub"),
        (Bypass::Sdk, "python3 -m pip install pygithub"),
        (Bypass::Sdk, "npm install @octokit/rest"),
        (Bypass::Sdk, "cargo add octocrab"),
        (Bypass::Sdk, "go get github.com/google/go-github/v60"),
        (Bypass::Sdk, "python3 -c 'from github import Github; Github()'"),
        (Bypass::Sdk, "node -e \"const {Octokit} = require('@octokit/rest')\""),
    ];
    for (want, cmd) in cases {
        assert_eq!(class(cmd), Some(*want), "plain: {cmd}");
        let escaped = cmd.replace('"', "\\\"");
        for wrapped in [
            format!("bash -c \"{escaped}\""),
            format!("sh -lc \"{escaped}\""),
            format!("eval \"{escaped}\""),
            format!("echo \"{escaped}\" | sh"),
            format!("cd /tmp && {cmd}"),
        ] {
            assert_eq!(class(&wrapped), Some(*want), "wrapped: {wrapped}");
        }
    }
    // Every class has at least one fixture.
    for b in Bypass::ALL {
        assert!(cases.iter().any(|(c, _)| *c == b), "no fixture for {b:?}");
    }
}

#[test]
fn heredoc_to_a_shell_or_interpreter_is_live_code() {
    assert_eq!(
        class("bash <<'EOF'\ncurl https://api.github.com/zen\nEOF"),
        Some(Bypass::CanonicalApiClient)
    );
    assert_eq!(class("python3 - <<'EOF'\nfrom github import Github\nEOF"), Some(Bypass::Sdk));
    assert_eq!(
        class("gh api \\\n  https://api.github.com/user"),
        Some(Bypass::GhApiAbsoluteUrl)
    );
    assert_eq!(
        class("printf '%s\\n' '/usr/bin/gh api user' | bash"),
        Some(Bypass::PathQualifiedGh)
    );
}

#[test]
fn managed_commands_are_not_denied() {
    for cmd in [
        "gh issue list",
        "gh issue list --label loom:issue --limit 50",
        "gh api repos/o/r",
        "gh api graphql -f query='query { viewer { login } }'",
        "gh api repos/o/r/pulls --jq '.[].html_url'",
        "./.loom/scripts/merge-pr.sh 123",
        "./.loom/scripts/create-issue.sh --title t --body b",
        "git push",
        "git push -u origin feature/issue-9989",
        "git ls-remote https://github.com/o/r",
        "/usr/local/bin/gh issue list",
        "command -v gh",
        "which gh",
        "ls -l /usr/local/libexec/x/gh",
        "cat ~/.config/gh/hosts.yml",
        "echo https://api.github.com/zen",
        "grep -rn api.github.com src/",
        "curl -sS https://example.com/zen",
        "gh pr create --title t --body 'see the gh auth login docs'",
        "pip install requests",
        "python3 -c 'import json'",
        "MY_GH_HOST=x make",
        "unset GH_HOST",
    ] {
        assert_eq!(class(cmd), None, "{cmd}");
    }
}

#[test]
fn launcher_exemption_only_from_a_trusted_origin() {
    let repo = enforced(&loaded(Origin::Repo, "required")).unwrap();
    assert_eq!(repo.launcher, None);
    assert_eq!(
        classify("/usr/local/bin/gh issue list", repo.launcher.as_deref()),
        Some(Bypass::PathQualifiedGh)
    );
    for origin in [Origin::Env, Origin::Machine] {
        let e = enforced(&loaded(origin, "required")).unwrap();
        assert_eq!(e.launcher.as_deref(), Some(LAUNCHER));
    }
}

#[test]
fn inert_without_an_enforcing_policy() {
    let curl = "curl https://api.github.com/zen";
    assert_eq!(check(curl, &Resolution::Unconfigured, || true), None);
    assert_eq!(check(curl, &loaded(Origin::Machine, "observe"), || true), None);
    let unreadable = Resolution::Unreadable {
        candidate: Candidate {
            origin: Origin::Machine,
            path: PathBuf::from("/nonexistent"),
        },
        error: "NotFound".into(),
        ignored: vec![],
    };
    assert_eq!(check(curl, &unreadable, || true), None);
    // An unknown enforcement value fails closed, like `is_observe_only`.
    assert!(check(curl, &loaded(Origin::Machine, "bogus"), || true).is_some());
}

#[test]
fn toggle_off_allows_and_is_only_read_on_a_hit() {
    let policy = loaded(Origin::Machine, "required");
    assert_eq!(check("curl https://api.github.com/zen", &policy, || false), None);
    assert_eq!(check("gh issue list", &policy, || panic!("toggle read on a miss")), None);
}

#[test]
fn toggle_precedence() {
    let f = json!(false);
    let fs = json!("false");
    let t = json!(true);
    assert!(toggle_enabled(None, None));
    assert!(!toggle_enabled(None, Some(&f)));
    assert!(!toggle_enabled(None, Some(&fs)));
    assert!(toggle_enabled(None, Some(&t)));
    assert!(!toggle_enabled(Some("0"), Some(&t)));
    assert!(toggle_enabled(Some("1"), Some(&f)));
    assert!(!toggle_enabled(Some("no"), None));
    // Unrecognised env values fall through to config.
    assert!(!toggle_enabled(Some("off"), Some(&f)));
    assert!(toggle_enabled(Some("maybe"), None));
}

#[test]
fn denial_names_code_alternative_and_origin_never_the_command() {
    // Built at runtime so the source never carries a token-shaped literal (#9133).
    let secret = format!("{}_{}", "ghp", "Ab9".repeat(12));
    let cmd = format!("curl -H 'Authorization: token {secret}' https://api.github.com/user");
    let reason = check(&cmd, &loaded(Origin::Machine, "required"), || true).unwrap();
    assert!(reason.starts_with(DENY_PREFIX), "{reason}");
    assert!(reason.contains(FINDING_CODE));
    assert!(reason.contains("plain `gh …`"));
    assert!(reason.contains("origin: machine"));
    assert!(!reason.contains(&secret));
    assert!(!reason.contains("Authorization"));
    assert!(!reason.contains(LAUNCHER));
}
