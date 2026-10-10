//! Tests for the tool-call-time capability matcher (#8256).
//!
//! Organised per capability, each with the denied shapes a persuaded role
//! would type AND the everyday read-only-role commands that must stay
//! allowed — a backstop that denies `gh pr view` is as broken as one that
//! allows `ssh`.

use super::*;
use crate::role_tool_policy::{Allowlist, RoleToolPolicy};
use std::path::PathBuf;

const HOME: Option<&str> = Some("/home/agent");

fn caps(cmd: &str) -> Vec<&'static str> {
    command_hits(cmd, HOME)
        .into_iter()
        .map(|h| h.capability)
        .collect()
}

fn hits_cap(cmd: &str, cap: &str) -> bool {
    caps(cmd).contains(&cap)
}

fn assert_clean(cmd: &str) {
    assert!(caps(cmd).is_empty(), "expected no capability for {cmd:?}, got {:?}", caps(cmd));
}

fn role(name: &str, allowlist: Allowlist) -> RoleToolPolicy {
    RoleToolPolicy {
        role: name.to_string(),
        source: Some(PathBuf::from("/repo/.loom/roles/x.json")),
        allowlist,
    }
}

fn read_only() -> RoleToolPolicy {
    role("curator", Allowlist::Declared(vec![]))
}

// ---------------------------------------------------------------------------
// remote-shell
// ---------------------------------------------------------------------------

#[test]
fn remote_shell_every_program_as_a_command_word() {
    for prog in REMOTE_SHELL_PROGRAMS {
        assert!(hits_cap(&format!("{prog} host"), "remote-shell"), "{prog}");
        assert!(hits_cap(&format!("/usr/bin/{prog} host"), "remote-shell"), "{prog}");
    }
}

#[test]
fn remote_shell_through_wrappers_compounds_and_substitutions() {
    for cmd in [
        "sudo ssh host",
        "sudo -u root ssh host",
        "env FOO=1 ssh host",
        "env -i ssh host",
        "env -S 'ssh host'",
        "FOO=bar ssh host",
        "nohup ssh host &",
        "timeout 5 ssh host",
        "timeout -s KILL 5 ssh host",
        "nice -n 10 ssh host",
        "command ssh host",
        "exec ssh host",
        "echo hi; ssh host",
        "true && ssh host",
        "false || ssh host",
        "cat x | ssh host",
        "(ssh host)",
        "{ ssh host; }",
        "if ssh host; then :; fi",
        "for h in a b; do ssh $h; done",
        "x=$(ssh host cat /etc/hostname)",
        "echo \"$(ssh host)\"",
        "echo `ssh host`",
        "diff <(ssh host cat f) f",
        "bash -c 'ssh host'",
        "sh -ec \"ssh host\"",
        "bash -o pipefail -c 'ssh host'",
        "bash <<< 'ssh host'",
        "eval ssh host",
        "eval 'ssh host'",
        "xargs ssh < hosts",
        "xargs -n 1 ssh < hosts",
        "find . -name x -exec ssh {} \\;",
        "watch -n 5 ssh host uptime",
        "flock /tmp/l -c 'ssh host'",
        "\"ssh\" host",
        "s\\sh host",
        "'s'sh host",
        "echo ok\nssh host",
    ] {
        assert!(hits_cap(cmd, "remote-shell"), "not caught: {cmd:?}");
    }
}

#[test]
fn remote_shell_through_transports_and_session_wrappers() {
    // Judge round 1 on PR #11152: each was ALLOWED before.
    for cmd in [
        "rsync -e ssh a b:c",
        "rsync -avze 'ssh -p 2222' a b:c",
        "rsync --rsh=ssh a b",
        "rsync a host:b",
        "rsync -a host:/etc/x .",
        "rsync rsync://host/mod/x .",
        "script -c 'ssh h'",
        "script -qc 'ssh h' /dev/null",
        "script --command='ssh h' log",
        "su -c 'ssh h'",
        "su - root -c 'ssh h'",
        "su root --command 'ssh h'",
        "runuser -u x -- ssh h",
        "busybox ssh h",
        "toybox ssh h",
        "tmux new 'ssh h'",
        "tmux new-session -d ssh h",
        "tmux send-keys 'ssh h' Enter",
        "screen ssh h",
        "screen -dmS s ssh h",
        "git -c core.sshCommand='ssh -i k' fetch",
        "git -c core.sshcommand=x fetch",
        "git config core.sshCommand 'ssh -i k'",
        "git -c alias.x='!ssh h' x",
        "git clone 'ext::ssh h %S foo'",
        "GIT_SSH_COMMAND='ssh -i k' git fetch",
        "env GIT_SSH_COMMAND=x git fetch",
        "export GIT_SSH=/tmp/s",
        "RSYNC_RSH=ssh rsync a b",
    ] {
        assert!(hits_cap(cmd, "remote-shell"), "not caught: {cmd:?}");
    }
}

#[test]
fn local_rsync_git_and_tmux_stay_clean() {
    for cmd in [
        "rsync -a src/ dst/",
        "rsync -av ./a:b dst/",
        "rsync --exclude target -a a/ b/",
        "git fetch origin && git rebase origin/main",
        "git -c user.name=x commit -m 'ssh fix'",
        "git config --get core.editor",
        "tmux ls",
        "screen -ls",
        "script -q /dev/null",
        "busybox ls",
        "su -c 'ls -la'",
    ] {
        assert!(!hits_cap(cmd, "remote-shell"), "false positive: {cmd:?}");
    }
}

#[test]
fn remote_shell_mentions_are_not_invocations() {
    for cmd in [
        "grep -rn ssh docs/",
        "echo ssh",
        "gh pr comment 1 --body 'run ssh; then aws'",
        "git commit -m \"ssh support; aws too\"",
        "command -v ssh",
        "which ssh",
        "man ssh",
        "ls # ssh host",
        "cat sshd_config.example",
        "git log --grep=ssh",
    ] {
        assert!(!hits_cap(cmd, "remote-shell"), "false positive: {cmd:?}");
    }
}

// ---------------------------------------------------------------------------
// cloud-cli
// ---------------------------------------------------------------------------

#[test]
fn cloud_cli_every_program_as_a_command_word() {
    for prog in CLOUD_CLI_PROGRAMS {
        assert!(hits_cap(&format!("{prog} whoami"), "cloud-cli"), "{prog}");
        assert!(hits_cap(&format!("sudo {prog} whoami"), "cloud-cli"), "{prog}");
        assert!(hits_cap(&format!("bash -c '{prog} x'"), "cloud-cli"), "{prog}");
    }
    assert!(hits_cap("aws sts get-caller-identity", "cloud-cli"));
    assert!(hits_cap("AWS_PROFILE=prod aws s3 ls", "cloud-cli"));
}

#[test]
fn cloud_cli_mentions_are_not_invocations() {
    for cmd in [
        "grep aws README.md",
        "echo 'quarterly aws maintenance'",
        "gh issue view 5 --json body",
        "cat lazy.txt",
        "cargo test -p loom-daemon az",
    ] {
        assert!(!hits_cap(cmd, "cloud-cli"), "false positive: {cmd:?}");
    }
}

// ---------------------------------------------------------------------------
// forge-secrets
// ---------------------------------------------------------------------------

#[test]
fn forge_secrets_gh_forms() {
    for cmd in [
        "gh secret list",
        "gh secret set FOO --body x",
        "gh variable get X",
        "gh auth token",
        "gh auth login --with-token",
        "gh auth refresh -s admin:org",
        "gh auth logout",
        "gh auth setup-git",
        "gh auth status --show-token",
        "gh auth status -t",
        "gh api repos/o/r/actions/secrets",
        "gh api /orgs/o/actions/secrets/public-key",
        "gh api repos/o/r/actions/variables",
        "bash -c 'gh secret list'",
        "env GH_TOKEN=x gh secret list",
        "/usr/local/bin/gh secret list",
    ] {
        assert!(hits_cap(cmd, "forge-secrets"), "not caught: {cmd:?}");
    }
}

#[test]
fn forge_secrets_behind_leading_gh_options() {
    // Judge round 1 on PR #11152: option values hid the subcommand.
    for cmd in [
        "gh -R o/r secret list",
        "gh --repo o/r secret list",
        "gh --repo=o/r secret list",
        "gh --repo o/r variable list",
        "gh -R o/r variable get X",
        "gh --hostname ghe.example api repos/o/r/actions/secrets",
        "gh --unknown val api repos/o/r/actions/variables",
        "gh auth --hostname ghe.example token",
        "gh -R o/r auth status --show-token",
        "bash -c 'gh -R o/r secret list'",
    ] {
        assert!(hits_cap(cmd, "forge-secrets"), "not caught: {cmd:?}");
    }
    for cmd in [
        "gh -R o/r pr view 12",
        "gh --repo o/r issue list --label loom:issue",
        "gh --repo=o/r pr comment 3 --body secret",
        "gh -R o/r pr comment 3 --body 'see gh secret list'",
        "gh --hostname ghe.example api repos/o/r/pulls/3",
        "gh -R o/r auth status",
        "gh pr comment 3 --body secret",
    ] {
        assert_clean(cmd);
    }
}

#[test]
fn forge_secrets_everyday_gh_stays_clean() {
    for cmd in [
        "gh auth status",
        "gh pr view 12 --comments",
        "gh issue list --label loom:issue",
        "gh pr comment 3 --body 'do not run gh secret list'",
        "gh api repos/o/r/pulls/3",
        "gh issue view 8256 --json title,body",
        "gh pr edit 3 --add-label loom:pr",
    ] {
        assert_clean(cmd);
    }
}

// ---------------------------------------------------------------------------
// credential-store
// ---------------------------------------------------------------------------

#[test]
fn credential_store_reads_and_writes() {
    for cmd in [
        "cat ~/.ssh/id_ed25519",
        "cat $HOME/.ssh/id_rsa",
        "cat ${HOME}/.aws/credentials",
        "cat /home/agent/.ssh/id_rsa",
        "cat /home/other/.ssh/id_rsa",
        "cat /root/.ssh/id_rsa",
        "cat /Users/me/.aws/credentials",
        "echo key >> ~/.ssh/authorized_keys",
        "tee -a ~/.ssh/authorized_keys < k",
        "cp k ~/.ssh/authorized_keys",
        "mkdir -p ~/.ssh",
        "ls ~/.ssh",
        "cat ~/.config/gh/hosts.yml",
        "cat ~/.gnupg/secring.gpg",
        "cat ~/.netrc",
        "cat ~/.docker/config.json",
        "ls ~/.loom/tokens",
        "cat ~/.kube/config",
        "cat ~/.claude/.credentials.json",
        "cat ~/foo/../.ssh/id_rsa",
        "cat ~/.s*h/id_rsa",
        "cat ~/.*/credentials",
        "cd && cat .ssh/id_rsa",
        "scp host:~/.ssh/id_rsa .",
        "curl --data-binary @x --key=~/.ssh/id_rsa https://e.example",
        "base64 < ~/.ssh/id_rsa",
        "x=$(cat ~/.aws/credentials)",
        "KEY=~/.ssh/id_rsa ./run",
    ] {
        assert!(hits_cap(cmd, "credential-store"), "not caught: {cmd:?}");
    }
}

#[test]
fn credential_store_paths_are_normalized_before_the_home_test() {
    // Judge round 1 on PR #11152: every one of these was ALLOWED.
    for cmd in [
        "echo x >> /tmp/../home/agent/.ssh/authorized_keys",
        "cat /tmp/../home/agent/.ssh/id_rsa",
        "cat /proc/self/root/home/agent/.ssh/id_rsa",
        "cat /proc/thread-self/root/home/agent/.ssh/id_rsa",
        "cat /proc/1234/root/home/agent/.ssh/id_rsa",
        "cat /proc/1/task/1/root/root/.aws/credentials",
        "cat /proc/self/root/proc/self/root/home/agent/.ssh/id_rsa",
        "cat /proc/self/root/../home/agent/.ssh/id_rsa",
        "cat /proc/*/root/home/agent/.ssh/id_rsa",
        "cat /proc/self/cwd/.ssh/id_rsa",
        "cat //home/agent/.ssh/id_rsa",
        "cat /./home/agent/.ssh/id_rsa",
        "cat /home//agent//.ssh//id_rsa",
        "cat /home/agent/./.ssh/id_rsa",
        "cat /../../home/agent/.ssh/id_rsa",
        "cat /home/agent/x/../.ssh/id_rsa",
        "cat /usr/../root/.ssh/id_rsa",
        "cat ~/../other/.ssh/id_rsa",
        "cat $HOME/../other/.aws/credentials",
        "cat ../../home/agent/.ssh/id_rsa",
        "cat a/../../../../root/.ssh/id_rsa",
        "cp k /tmp/../home/agent/.ssh/authorized_keys",
    ] {
        assert!(hits_cap(cmd, "credential-store"), "not caught: {cmd:?}");
    }
    for cmd in [
        "cat /proc/self/status",
        "cat /proc/self/root/etc/hostname",
        "cat /tmp/../etc/ssh/sshd_config",
        "cat /home/agent/repo/../repo/src/main.rs",
        "cat ../README.md",
    ] {
        assert!(!hits_cap(cmd, "credential-store"), "false positive: {cmd:?}");
    }
    // The same normalization serves the Edit/Write path mode.
    for p in [
        "/tmp/../home/agent/.ssh/authorized_keys",
        "/proc/self/root/home/agent/.ssh/authorized_keys",
        "//home/agent/.aws/config",
    ] {
        assert!(path_hit(p, HOME).is_some(), "{p}");
    }
    // With no known $HOME, `~` still anchors under /home.
    assert!(!command_hits("cat ~/../x/.ssh/id_rsa", None).is_empty());
}

#[test]
fn credential_store_lookalikes_stay_clean() {
    for cmd in [
        "ls ~",
        "cat ~/.config/git/config",
        "cat ~/.bashrc",
        "ls .loom/worktrees",
        "cat defaults/docs/guard-hooks.md",
        "ls *",
        "git add .*",
        "cat /etc/ssh/sshd_config",
    ] {
        assert!(!hits_cap(cmd, "credential-store"), "false positive: {cmd:?}");
    }
}

#[test]
fn a_grep_pattern_is_text_but_every_other_operand_is_a_path() {
    // The hook hands the matcher the real operands (not blanked text), so the
    // matcher itself separates a grep/rg PATTERN from the files it reads.
    for cmd in [
        "grep -rn '~/.ssh' docs/",
        "grep -rn \"/home/agent/.ssh\" docs/",
        "rg '~/.aws/credentials' src",
        "grep -e '~/.ssh' -- docs/",
        "grep -A 3 '~/.ssh' docs/",
        "grep -rne '~/.ssh' docs/",
    ] {
        assert!(!hits_cap(cmd, "credential-store"), "pattern flagged: {cmd:?}");
    }
    for cmd in [
        "grep -h \".*\" \"/home/agent/.ssh/id_rsa\"",
        "rg \"x\" \"/home/agent/.aws/credentials\"",
        "grep x ~/.ssh/id_rsa",
        "grep -e x ~/.ssh/id_rsa",
        "grep -e x -- ~/.ssh/id_rsa",
        "grep -f ~/.ssh/id_rsa docs/",
        "grep -f pats ~/.ssh/id_rsa",
        "grep -rn '~/.ssh' ~/.ssh",
        "grep --regexp=x ~/.ssh/id_rsa",
        "grep -ex ~/.ssh/id_rsa",
        "rg -ex ~/.ssh/id_rsa",
        "grep -rnex ~/.ssh/id_rsa",
        "grep -rne x ~/.ssh/id_rsa",
        "grep --regexp=x -- ~/.ssh/id_rsa",
        "grep -ex -- ~/.ssh/id_rsa",
        "rg --files ~/.ssh",
        "grep -fpats ~/.ssh/id_rsa",
    ] {
        assert!(hits_cap(cmd, "credential-store"), "missed: {cmd:?}");
    }
}

#[test]
fn credential_store_path_mode() {
    for p in [
        "/home/agent/.ssh/authorized_keys",
        "/home/agent/.aws/config",
        "/root/.gnupg/x",
        "/home/agent/.config/gh/hosts.yml",
    ] {
        assert_eq!(path_hit(p, HOME).map(|h| h.capability), Some("credential-store"), "{p}");
    }
    for p in [
        "/home/agent/repo/src/main.rs",
        "/home/agent/.bashrc",
        "/tmp/.ssh-notes.md",
    ] {
        assert!(path_hit(p, HOME).is_none(), "{p}");
    }
}

// ---------------------------------------------------------------------------
// The verdict: allowlist x hit
// ---------------------------------------------------------------------------

#[test]
fn a_read_only_role_is_denied_every_capability() {
    let p = read_only();
    for (cmd, cap) in [
        ("ssh host", "remote-shell"),
        ("aws s3 ls", "cloud-cli"),
        ("gh secret list", "forge-secrets"),
        ("echo k >> ~/.ssh/authorized_keys", "credential-store"),
    ] {
        let hit = p
            .check_command(cmd, HOME)
            .unwrap_or_else(|| panic!("allowed: {cmd}"));
        assert_eq!(hit.capability, cap);
        let reason = p.deny_reason(&hit);
        assert!(reason.starts_with(CHECK_DENY_PREFIX), "{reason}");
        assert!(reason.contains(cap) && reason.contains("/repo/.loom/roles/x.json"), "{reason}");
        assert!(reason.contains("grants: (nothing)"), "{reason}");
    }
    assert!(p
        .check_path("/home/agent/.ssh/authorized_keys", HOME)
        .is_some());
}

#[test]
fn an_unrestricted_role_is_never_denied() {
    // builder/doctor/driver/loom declare ["*"]; an undeclared custom role is
    // unrestricted too. Neither may ever see a deny from this matcher.
    for allowlist in [
        Allowlist::Declared(vec!["*".to_string()]),
        Allowlist::Undeclared,
    ] {
        let p = role("builder", allowlist);
        for cmd in [
            "ssh host",
            "aws s3 ls",
            "gh secret list",
            "cat ~/.ssh/id_rsa",
        ] {
            assert!(p.check_command(cmd, HOME).is_none(), "{cmd}");
        }
        assert!(p.check_path("/home/agent/.ssh/config", HOME).is_none());
    }
}

#[test]
fn a_granted_capability_is_allowed_and_the_rest_still_denied() {
    let p = role("judge", Allowlist::Declared(vec!["cloud-cli".to_string()]));
    assert!(p.check_command("aws s3 ls", HOME).is_none());
    assert_eq!(p.check_command("ssh host", HOME).unwrap().capability, "remote-shell");
    // A compound command is denied on its first un-granted capability, not
    // allowed because its first capability was granted.
    assert_eq!(
        p.check_command("aws s3 ls && gh secret list", HOME)
            .unwrap()
            .capability,
        "forge-secrets"
    );
}

#[test]
fn everyday_read_only_role_commands_pass_a_restricted_role() {
    let p = read_only();
    for cmd in [
        "gh issue list --label loom:issue --json number,title",
        "gh pr diff 12",
        "git fetch origin && git log --oneline -5",
        "cargo test -p loom-daemon role_tool_policy",
        "./.loom/scripts/merge-pr.sh 12 --dry-run",
        "loom-daemon forge check-claim 42",
        "rg -n 'toolPolicy' defaults/roles",
        "cat defaults/roles/judge.json | jq .toolPolicy",
    ] {
        assert!(p.check_command(cmd, HOME).is_none(), "denied: {cmd}");
    }
}

#[test]
fn pathological_nesting_terminates() {
    // Substitution nesting is linear in length; quote-nesting would grow 4x per
    // level and make the fixture itself astronomically large.
    let cmd = format!("{}ssh host{}", "$(".repeat(40), ")".repeat(40));
    // Past MAX_DEPTH the matcher stops looking; it must return, not overflow.
    let _ = command_hits(&cmd, HOME);
    let deep = "$(".repeat(5000);
    let _ = command_hits(&deep, HOME);
}

#[test]
fn glob_rules() {
    assert!(glob_match(".s*h", ".ssh"));
    assert!(glob_match(".*", ".aws"));
    assert!(glob_match(".ss?", ".ssh"));
    assert!(glob_match(".[s]sh", ".ssh"));
    assert!(!glob_match("*", ".ssh"), "a wildcard never matches a leading dot");
    assert!(!glob_match(".bash*", ".ssh"));
}

// ---------------------------------------------------------------------------
// Wrapper options and ANSI-C quoting must not hide the executable
// ---------------------------------------------------------------------------

#[test]
fn xargs_option_values_do_not_hide_the_executable() {
    for cmd in [
        "xargs --process-slot-var SLOT ssh example.invalid",
        "xargs --process-slot-var=SLOT ssh example.invalid",
        "xargs --max-chars 100 ssh example.invalid",
        "xargs -s 100 -P 2 -n 1 ssh example.invalid",
        "xargs -s100 -I{} ssh example.invalid",
    ] {
        assert!(hits_cap(cmd, "remote-shell"), "{cmd}");
    }
    assert!(hits_cap(
        "xargs --process-slot-var SLOT aws sts get-caller-identity",
        "cloud-cli"
    ));
    assert_clean("xargs --process-slot-var SLOT echo");
}

#[test]
fn xargs_optional_value_options_do_not_swallow_the_executable() {
    // GNU `--replace` / `--max-lines` take only an attached `=value`.
    for cmd in [
        "xargs --replace ssh example.invalid",
        "xargs --max-lines ssh example.invalid",
        "xargs --replace=X ssh example.invalid",
    ] {
        assert!(hits_cap(cmd, "remote-shell"), "{cmd}");
    }
}

#[test]
fn priority_wrapper_long_options_consume_their_values() {
    for cmd in [
        "nice --adjustment 5 ssh example.invalid",
        "nice --adjustment=5 ssh example.invalid",
        "nice -n 5 ssh example.invalid",
        "ionice --class 2 --classdata 4 ssh example.invalid",
        "ionice -c 2 -n 4 ssh example.invalid",
        "stdbuf --output L --error 0 ssh example.invalid",
        "stdbuf -oL ssh example.invalid",
        "chrt --sched-runtime 1000 --sched-deadline 2000 --deadline 0 ssh example.invalid",
    ] {
        assert!(hits_cap(cmd, "remote-shell"), "{cmd}");
    }
    assert!(hits_cap("nice --adjustment 5 aws sts get-caller-identity", "cloud-cli"));
    assert_clean("nice --adjustment 5 echo");
}

#[test]
fn taskset_and_chrt_consume_their_operand_exactly_once() {
    // `taskset -c` is the CPU-list mode flag, not a value option: the list is
    // the single positional operand, and the executable follows it.
    for cmd in [
        "taskset -c 0 ssh example.invalid",
        "taskset --cpu-list 0-3 ssh example.invalid",
        "taskset 0x1 ssh example.invalid",
        "taskset -a -c 0,2 ssh example.invalid",
        "chrt -i 0 ssh example.invalid",
        "chrt -o 0 ssh example.invalid",
        "chrt -f 10 ssh example.invalid",
        "chrt --idle 0 ssh example.invalid",
    ] {
        assert!(hits_cap(cmd, "remote-shell"), "{cmd}");
    }
    assert!(hits_cap("taskset -c 0 aws sts get-caller-identity", "cloud-cli"));
    assert!(hits_cap("taskset --cpu-list 0 aws sts get-caller-identity", "cloud-cli"));
    assert!(hits_cap("taskset -c 0 gh secret list", "forge-secrets"));
    assert_clean("taskset -c 0 echo");
    assert_clean("taskset 0x1 echo");
    assert_clean("chrt -i 0 echo");
}

#[test]
fn stdbuf_and_ionice_keep_their_own_value_options() {
    for cmd in [
        "stdbuf -i0 -o L -e 0 ssh example.invalid",
        "ionice -c 3 ssh example.invalid",
        "ionice -P 1 -t ssh example.invalid",
    ] {
        assert!(hits_cap(cmd, "remote-shell"), "{cmd}");
    }
    assert_clean("ionice -c 3 echo");
}

#[test]
fn time_options_do_not_hide_the_executable() {
    for cmd in [
        "time -p ssh example.invalid",
        "time -o /tmp/t -f %e ssh example.invalid",
        "time -- ssh example.invalid",
    ] {
        assert!(hits_cap(cmd, "remote-shell"), "{cmd}");
    }
    assert!(hits_cap("time -p aws sts get-caller-identity", "cloud-cli"));
    assert_clean("time -p ls");
}

#[test]
fn sudo_long_options_consume_their_values() {
    for cmd in [
        "sudo --user root aws sts get-caller-identity",
        "sudo --group wheel --user root aws sts get-caller-identity",
        "sudo --chdir /tmp ssh example.invalid",
        "sudo --user=root aws sts get-caller-identity",
    ] {
        assert!(!caps(cmd).is_empty(), "{cmd}");
    }
    assert_clean("sudo --user root ls");
}

#[test]
fn ansi_c_escapes_are_decoded_before_matching() {
    assert!(hits_cap(r"$'\x73sh' example.invalid", "remote-shell"));
    assert!(hits_cap(r"$'\163sh' example.invalid", "remote-shell"));
    assert!(hits_cap(r"$'\141ws' sts get-caller-identity", "cloud-cli"));
    assert!(hits_cap(r"$'ssh' example.invalid", "remote-shell"));
    assert_clean(r"echo $'a\tb'");
}

#[test]
fn ansi_c_nul_truncates_the_word_like_bash() {
    assert!(hits_cap(r"$'ssh\0suffix' example.invalid", "remote-shell"));
    assert!(hits_cap(r"$'aws\x00suffix' sts get-caller-identity", "cloud-cli"));
    assert!(hits_cap(r"$'ssh\u0000suffix' example.invalid", "remote-shell"));
    assert!(hits_cap(r"$'ssh\U00000000suffix' example.invalid", "remote-shell"));
    assert!(hits_cap(r"$'ssh\000suffix' example.invalid", "remote-shell"));
    assert_clean(r"$'ls\0ssh' example.invalid");
}

#[test]
fn substitutions_inside_parameter_expansions_are_analyzed() {
    for cmd in [
        r#"echo "${unset_var:-$(ssh example.invalid)}""#,
        "echo ${unset_var:-$(ssh example.invalid)}",
        r#"echo "${unset_var:-`ssh example.invalid`}""#,
        "echo ${unset_var:-`ssh example.invalid`}",
        // A `}` inside the substitution must not end the expansion early.
        "echo ${v:-$(echo }; ssh example.invalid)}",
        "echo ${a:-${b:-$(ssh example.invalid)}}",
    ] {
        assert!(hits_cap(cmd, "remote-shell"), "{cmd}");
    }
    assert!(hits_cap(r#"echo "${v:-$(aws sts get-caller-identity)}""#, "cloud-cli"));
    assert_clean(r#"echo "${v:-$(echo hi)}" ${w:-fallback} ${#x} "${y%.txt}""#);
    assert_clean(r#"echo "${v:-`date`}""#);
}
