#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::roster::{is_tailnet_literal, names_match, tailscale_names};
use super::*;

struct FakeRoster;

impl Roster for FakeRoster {
    fn is_fleet_host(&self, host: &str) -> bool {
        is_tailnet_literal(host)
            || ["loom-worker-1", "loom-worker-2"]
                .iter()
                .any(|n| names_match(n, host))
    }
    fn is_fleet_instance(&self, id: &str) -> bool {
        id == "i-0fleet"
    }
}

struct Fx {
    dir: tempfile::TempDir,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("s.sh"),
            "#!/bin/bash\nset -e\ncd ~/GitHub/2am\ngit rebase --abort || true\ngit reset --hard origin/main\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ro.sh"),
            "git -C ~/GitHub/2am status\ngrep -n foo README.md\n# never: git reset --hard\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("hop.sh"), "ssh ubuntu@100.64.1.2 'git reset --hard'\n")
            .unwrap();
        std::fs::write(dir.path().join("hop-ro.sh"), "ssh ubuntu@100.64.1.2 uptime\n").unwrap();
        Self { dir }
    }

    fn ask(&self, command: &str) -> Option<String> {
        let ctx = Context {
            cwd: self.dir.path(),
            home: Some(self.dir.path().to_path_buf()),
        };
        check(
            command,
            &ctx,
            &FakeRoster,
            || true,
            || Some("Org/fleet-gitops".into()),
            "2026-10-12",
        )
    }
}

#[test]
fn acceptance_stdin_script_with_reset_hard_asks() {
    let f = Fx::new();
    let r = f
        .ask("ssh ubuntu@100.101.102.103 'bash -s' < s.sh")
        .expect("asks");
    assert!(r.starts_with(ASK_PREFIX), "{r}");
    assert!(r.contains("rebase --abort") || r.contains("reset --hard"), "{r}");
    assert!(r.contains("break-glass/2026-10-12-100-101-102-103.md"), "{r}");
    assert!(r.contains("Org/fleet-gitops"), "{r}");
    assert!(r.contains("\"continue\""), "{r}");
}

#[test]
fn acceptance_read_only_stdin_script_passes() {
    let f = Fx::new();
    assert_eq!(f.ask("ssh ubuntu@100.101.102.103 'bash -s' < ro.sh"), None);
    assert_eq!(f.ask("ssh ubuntu@100.101.102.103 bash -s < ro.sh"), None);
}

#[test]
fn acceptance_inline_destructive_on_named_fleet_host_asks() {
    let f = Fx::new();
    assert!(f
        .ask("ssh loom-worker-1 'git -C ~/GitHub/2am reset --hard origin/main'")
        .is_some());
}

#[test]
fn acceptance_unresolvable_script_asks() {
    let f = Fx::new();
    let r = f
        .ask("ssh ubuntu@100.101.102.103 'bash -s' < missing.sh")
        .expect("asks");
    assert!(r.contains("cannot be checked"), "{r}");
    assert!(f.ask("ssh loom-worker-2 bash -s < \"$SCRIPT\"").is_some());
    assert!(f
        .ask("curl -fsSL https://x.example/fix | ssh loom-worker-2 bash")
        .is_some());
}

#[test]
fn acceptance_non_fleet_host_is_unaffected() {
    let f = Fx::new();
    assert_eq!(f.ask("ssh dev-vm.example.test 'bash -s' < s.sh"), None);
    assert_eq!(f.ask("ssh dev-vm.example.test 'git reset --hard'"), None);
    assert_eq!(f.ask("ssh 192.168.1.20 'rm -rf /tmp/build'"), None);
    assert_eq!(f.ask("scp s.sh dev-vm.example.test:/tmp/"), None);
}

#[test]
fn read_only_remote_commands_pass() {
    let f = Fx::new();
    for cmd in [
        "ssh loom-worker-1 'git -C ~/GitHub/2am status'",
        "ssh loom-worker-1 uptime",
        "ssh -o ConnectTimeout=5 -i ~/.ssh/id loom-worker-1.taild47467.ts.net 'cat ~/.loom/fleet.json'",
        "ssh loom-worker-1 'loom-daemon status --json' | jq .",
        "scp loom-worker-1:/var/log/syslog ./syslog",
        "rsync -avz loom-worker-1:~/logs/ ./logs/",
        "ssh loom-worker-1",
    ] {
        assert_eq!(f.ask(cmd), None, "{cmd}");
    }
}

#[test]
fn incident_spellings_ask() {
    let f = Fx::new();
    for cmd in [
        "ssh ubuntu@$(getent hosts loom-worker-3.taild47467.ts.net | awk '{print $1}') 'bash -s' < s.sh",
        "IP=100.90.1.2; ssh ubuntu@$IP 'bash -s' < s.sh",
        "cat s.sh | ssh ubuntu@100.90.1.2 bash -s",
        "for h in a b; do ssh -T ubuntu@100.90.1.2 'sudo bash -s' < s.sh; done",
        "bash -c \"ssh loom-worker-1 'git stash drop'\"",
        "eval \"ssh loom-worker-1 'git clean -fdx'\"",
        "timeout 30 ssh -p 22 loom-worker-1 -- 'sudo systemctl stop loom-daemon'",
        "ssh loom-worker-1 bash <<'EOF'\ncd ~/GitHub/loom\ngit branch -D feature/x\nEOF",
        "ssh loom-worker-1 sh <<< 'git push --force origin main'",
        "printf 'rm -rf ~/GitHub/2am\\n' | ssh loom-worker-1 bash",
        "ssh loom-worker-1 'echo x > ~/.loom/config.json'",
        "ssh loom-worker-1 'cp /tmp/new ~/loom-daemon/bin/loom-daemon'",
        "out=$(ssh loom-worker-1 'git reset --hard'); echo $out",
        "ssh ssh://ubuntu@loom-worker-2:22 'git rebase --abort'",
    ] {
        assert!(f.ask(cmd).is_some(), "should ask: {cmd}");
    }
}

#[test]
fn pushes_onto_fleet_hosts_ask() {
    let f = Fx::new();
    assert!(f.ask("scp s.sh ubuntu@100.90.1.2:/tmp/s.sh").is_some());
    assert!(f
        .ask("rsync -av --delete ./dist/ loom-worker-1:~/app/")
        .is_some());
    assert!(f
        .ask("rsync -av -e 'ssh -p 2222' ./dist/ loom-worker-2:/srv/")
        .is_some());
}

#[test]
fn ssm_send_command() {
    let f = Fx::new();
    assert!(f
        .ask("aws ssm send-command --instance-ids i-0fleet --document-name AWS-RunShellScript --parameters 'commands=[\"git reset --hard\"]'")
        .is_some());
    assert!(f
        .ask("aws ssm send-command --targets Key=tag:fleet,Values=loom --document-name AWS-RunShellScript --parameters file://missing.json")
        .is_some());
    assert_eq!(
        f.ask("aws ssm send-command --instance-ids i-0other --document-name AWS-RunShellScript --parameters 'commands=[\"git reset --hard\"]'"),
        None
    );
    assert_eq!(
        f.ask("aws ssm send-command --instance-ids i-0fleet --document-name AWS-RunShellScript --parameters 'commands=[\"uptime\"]'"),
        None
    );
}

#[test]
fn prose_mentions_are_not_invocations() {
    let f = Fx::new();
    for cmd in [
        "echo \"ssh loom-worker-1 'git reset --hard'\"",
        "grep -n 'ssh loom-worker-1' notes.md",
        "git reset --hard origin/main",
    ] {
        assert_eq!(f.ask(cmd), None, "{cmd}");
    }
}

#[test]
fn toggle_off_allows() {
    let f = Fx::new();
    let ctx = Context {
        cwd: f.dir.path(),
        home: None,
    };
    assert_eq!(
        check(
            "ssh loom-worker-1 'git reset --hard'",
            &ctx,
            &FakeRoster,
            || false,
            || None,
            "d"
        ),
        None
    );
    assert!(toggle_enabled(None, None));
    assert!(!toggle_enabled(Some("0"), None));
    assert!(toggle_enabled(Some("1"), Some(&Value::Bool(false))));
    assert!(!toggle_enabled(None, Some(&Value::Bool(false))));
    assert!(!toggle_enabled(None, Some(&Value::String("false".into()))));
}

#[test]
fn tailnet_literals_and_names() {
    assert!(is_tailnet_literal("100.64.0.1"));
    assert!(is_tailnet_literal("100.127.255.255"));
    assert!(!is_tailnet_literal("100.128.0.1"));
    assert!(!is_tailnet_literal("10.0.0.1"));
    assert!(is_tailnet_literal("fd7a:115c:a1e0::1"));
    assert!(is_tailnet_literal("box.taild47467.ts.net."));
    assert!(names_match("loom-worker-1", "LOOM-WORKER-1.taild47467.ts.net"));
    assert!(!names_match("loom-worker-1", "loom-worker-10"));
    assert!(!names_match("a.example.com", "a.other.com"));
    let status: Value = serde_json::from_str(
        r#"{"Self":{"HostName":"volt","DNSName":"volt.t.ts.net.","TailscaleIPs":["100.1.2.3"]},
            "Peer":{"k":{"HostName":"loom-worker-3","DNSName":"loom-worker-3.t.ts.net.","TailscaleIPs":["100.4.5.6"]}}}"#,
    )
    .unwrap();
    let names = tailscale_names(&status);
    assert!(names.contains(&"loom-worker-3".to_string()));
    assert!(names.contains(&"100.4.5.6".to_string()));
}

#[test]
fn destructive_patterns() {
    for t in [
        "git reset --hard",
        "git -C x clean -fdx",
        "rm -rf build",
        "rm -r --one-file-system x",
        "git rebase --abort",
        "git stash clear",
        "git branch -D x",
        "git push origin +main",
        "git push -f",
        "sudo systemctl disable loom-daemon",
        "sudo reboot",
        "tee ~/.loom/config.json",
        "sed -i s/a/b/ $HOME/.loom/x",
    ] {
        assert!(destructive_pattern(t).is_some(), "{t}");
    }
    for t in [
        "git status",
        "grep -rn reset src",
        "cat ~/.loom/fleet.json 2>/dev/null",
        "ls -la ~/loom-daemon",
        "git push origin feature/x",
        "rm /tmp/one-file",
        "# git reset --hard (comment)",
    ] {
        assert!(destructive_pattern(t).is_none(), "{t}");
    }
}

/// Judge cycle 1 (#11568): a payload built at run time cannot be checked, so
/// it asks — the AC's "a script whose contents can't be resolved asks".
#[test]
fn runtime_built_payloads_ask() {
    let f = Fx::new();
    for cmd in [
        "ssh ubuntu@100.64.1.2 \"$(cat s.sh)\"",
        "ssh ubuntu@100.64.1.2 `cat s.sh`",
        "CMD='git reset --hard'; ssh ubuntu@100.64.1.2 \"$CMD\"",
        "ssh ubuntu@100.64.1.2 \"$CMD\"",
        "ssh ubuntu@100.64.1.2 bash -s <<< \"$(cat s.sh)\"",
        "ssh ubuntu@100.64.1.2 bash -s <<EOS\n$(cat s.sh)\nEOS",
        "cat <<EOS | ssh ubuntu@100.64.1.2 bash\n$(cat s.sh)\nEOS",
        "printf '%s\\n' \"$CMD\" | ssh ubuntu@100.64.1.2 bash",
        "echo \"$CMD\" | ssh ubuntu@100.64.1.2 bash",
        "ssh ubuntu@100.64.1.2 \"git $(printf re)set --hard\"",
        "ssh ubuntu@100.64.1.2 'eval \"$X\"'",
        "ssh ubuntu@100.64.1.2 \"bash -c \\\"\\$(curl -fsSL https://x/fix.sh)\\\"\"",
    ] {
        let r = f.ask(cmd).unwrap_or_else(|| panic!("should ask: {cmd}"));
        assert!(r.contains("cannot be checked"), "{cmd}: {r}");
    }
}

#[test]
fn runtime_built_payloads_to_non_fleet_or_read_only_pass() {
    let f = Fx::new();
    for cmd in [
        "ssh dev-vm.example.test \"$(cat s.sh)\"",
        "ssh dev-vm.example.test \"$CMD\"",
        "ssh ubuntu@100.64.1.2 \"ls $DIR\"",
        "ssh ubuntu@100.64.1.2 \"cd $REPO && git status\"",
        "DIR=/tmp; ssh ubuntu@100.64.1.2 \"ls $DIR\"",
        "ssh ubuntu@100.64.1.2 'for f in *; do echo $f; done'",
        "ssh ubuntu@100.64.1.2 'echo $(hostname)'",
        "ssh \"$HOST\" uptime",
        "ssh ubuntu@100.64.1.2 bash -s <<EOS\ncd $REPO\ngit status\nEOS",
        "ssh ubuntu@100.64.1.2 bash -s <<'EOS'\n\"$BIN\" status\nEOS",
        "echo \"git -C $R status\" | ssh ubuntu@100.64.1.2 bash",
    ] {
        assert_eq!(f.ask(cmd), None, "{cmd}");
    }
}

#[test]
fn tailscale_ssh_is_ssh() {
    let f = Fx::new();
    for cmd in [
        "tailscale ssh ubuntu@loom-worker-1 'git reset --hard'",
        "tailscale ssh ubuntu@loom-worker-1 'bash -s' < s.sh",
        "sudo tailscale ssh loom-worker-2 \"$(cat s.sh)\"",
    ] {
        assert!(f.ask(cmd).is_some(), "should ask: {cmd}");
    }
    for cmd in [
        "tailscale ssh ubuntu@loom-worker-1 uptime",
        "tailscale ssh dev-vm.example.test 'git reset --hard'",
        "tailscale status",
        "tailscale ping loom-worker-1",
    ] {
        assert_eq!(f.ask(cmd), None, "{cmd}");
    }
}

#[test]
fn fan_out_over_hosts_asks() {
    let f = Fx::new();
    for cmd in [
        "echo ubuntu@100.64.1.2 | xargs -I{} ssh {} 'git reset --hard'",
        "cat hosts | xargs -I % ssh ubuntu@% 'git reset --hard'",
        "cat hosts | xargs -P4 -I{} sh -c 'ssh {} bash -s < s.sh'",
        "xargs -a hosts -I{} tailscale ssh {} 'bash -s' < s.sh",
        "cat cmds | xargs ssh ubuntu@100.64.1.2",
        "parallel ssh {} 'git reset --hard' ::: ubuntu@100.64.1.2",
        "parallel -j4 ssh {1} 'git clean -fdx' ::: a b",
        "parallel \"ssh {} 'git reset --hard'\" ::: ubuntu@100.64.1.2",
        "printf '%s\\n' h1 h2 | xargs -n1 sh -c 'ssh \"$0\" git reset --hard'",
        "ls | xargs -I{} scp {} loom-worker-1:/tmp/",
    ] {
        assert!(f.ask(cmd).is_some(), "should ask: {cmd}");
    }
    for cmd in [
        "echo ubuntu@100.64.1.2 | xargs -I{} ssh {} uptime",
        "cat hosts | xargs -I % ssh % 'git -C ~/GitHub/loom status'",
        "parallel ssh {} uptime ::: ubuntu@100.64.1.2",
        "ls | xargs -I{} echo ssh {}",
        "find . -name '*.log' | xargs rm",
        "echo dev-vm.example.test | xargs -I{} scp loom-worker-1:/tmp/x {}",
    ] {
        assert_eq!(f.ask(cmd), None, "{cmd}");
    }
}

#[test]
fn heredoc_delimiter_quoting_is_tracked() {
    use super::lex::{pipelines, Stdin};
    let stdin = |c: &str| pipelines(c)[0][0].stdin.clone();
    for (cmd, want) in [
        ("bash <<EOS\nx\nEOS", true),
        ("bash <<'EOS'\nx\nEOS", false),
        ("bash <<\"EOS\"\nx\nEOS", false),
        ("bash <<\\EOS\nx\nEOS", false),
        ("bash <<-EOS\n\tx\n\tEOS", true),
    ] {
        match stdin(cmd) {
            Some(Stdin::Heredoc { expands, .. }) => assert_eq!(expands, want, "{cmd}"),
            other => panic!("{cmd}: {other:?}"),
        }
    }
}

/// Judge cycle 2 on #11568: a hop through a host that is not on the fleet
/// still reaches one. The remote command of every hop is scanned as a
/// command line of its own, and a script fed to the outer hop reaches the
/// inner one.
#[test]
fn nested_hops_through_a_non_fleet_host_ask() {
    let f = Fx::new();
    for cmd in [
        "ssh bastion ssh ubuntu@100.64.1.2 'git reset --hard'",
        "ssh bastion -t ssh ubuntu@100.64.1.2 'git reset --hard'",
        "ssh devvm.example.com \"ssh ubuntu@100.64.1.2 'git reset --hard'\"",
        "ssh devvm.example.com 'ssh loom-worker-1 \"rm -rf ~/GitHub\"'",
        "ssh bastion 'tailscale ssh loom-worker-1 git reset --hard'",
        "ssh bastion ssh ubuntu@100.64.1.2 bash -s < s.sh",
        "cat s.sh | ssh bastion ssh ubuntu@100.64.1.2 bash -s",
        "ssh bastion ssh ubuntu@100.64.1.2 bash -s <<< 'git reset --hard'",
        "ssh bastion ssh devvm ssh ubuntu@100.64.1.2 'git reset --hard'",
        "tailscale ssh bastion ssh loom-worker-2 'git clean -fdx'",
        "ssh bastion 'echo ubuntu@100.64.1.2 | xargs -I{} ssh {} git reset --hard'",
        "ssh bastion 'bash -c \"ssh ubuntu@100.64.1.2 git reset --hard\"'",
        "ssh bastion 'rsync -a ./x ubuntu@100.64.1.2:/srv/'",
        "ssh bastion 'for h in a b; do ssh $h rm -rf /srv/x; done'",
        // A script on the bastion cannot be read from here.
        "ssh bastion 'ssh ubuntu@100.64.1.2 bash -s < /tmp/fix.sh'",
        "ssh bastion 'ssh ubuntu@100.64.1.2 bash -s < s.sh'",
        "aws ssm send-command --instance-ids i-0other --document-name AWS-RunShellScript --parameters 'commands=[\"ssh ubuntu@100.64.1.2 git reset --hard\"]'",
    ] {
        assert!(f.ask(cmd).is_some(), "should ask: {cmd}");
    }
    let r = f
        .ask("ssh bastion ssh ubuntu@100.64.1.2 bash -s < s.sh")
        .expect("asks");
    assert!(r.contains("fleet host `100.64.1.2`"), "{r}");
}

#[test]
fn read_only_or_non_fleet_nested_hops_pass() {
    let f = Fx::new();
    for cmd in [
        "ssh bastion ssh ubuntu@100.64.1.2 uptime",
        "ssh bastion -t ssh ubuntu@100.64.1.2 'git status'",
        "ssh devvm 'ssh 10.0.0.5 rm -rf /tmp/x'",
        "ssh devvm \"ssh ubuntu@100.64.1.2 'ls $HOME'\"",
        "ssh bastion 'tailscale ssh loom-worker-1 uptime'",
        "ssh bastion ssh ubuntu@100.64.1.2 bash -s < ro.sh",
        "ssh bastion 'echo ubuntu@100.64.1.2 | xargs -I{} ssh {} uptime'",
        "ssh bastion 'scp loom-worker-1:/tmp/x ./'",
        "ssh bastion 'git reset --hard'",
        "ssh bastion 'for h in a b; do ssh $h uptime; done'",
        "ssh bastion 'cat /etc/hosts | grep ssh'",
        "cat s.sh | ssh bastion ssh ubuntu@100.64.1.2 uptime",
        "echo ubuntu@100.64.1.2 | ssh bastion 'xargs -I{} ssh {} uptime' < s.sh",
    ] {
        assert_eq!(f.ask(cmd), None, "{cmd}");
    }
}

/// Judge cycle 3 on #11610: a stdin script fed to a non-fleet hop is a
/// command line of its own on that hop — its text is scanned for nested
/// hops too, not just classified against the outer target.
#[test]
fn stdin_script_fed_to_a_non_fleet_hop_is_scanned_for_nested_hops() {
    let f = Fx::new();
    for cmd in [
        "ssh bastion bash -s <<'EOS'\nssh ubuntu@100.64.1.2 'git reset --hard'\nEOS",
        "ssh bastion 'bash -s' <<EOS\nssh loom-worker-1 rm -rf ~/GitHub\nEOS",
        "ssh bastion bash -s <<'EOS'\nssh ubuntu@100.64.1.2 bash -s <<'EOS2'\ngit reset --hard\nEOS2\nEOS",
        "ssh bastion bash -s <<< \"ssh ubuntu@100.64.1.2 'git reset --hard'\"",
        "ssh bastion bash -s < hop.sh",
        "ssh bastion < hop.sh",
        "cat hop.sh | ssh bastion bash",
    ] {
        assert!(f.ask(cmd).is_some(), "should ask: {cmd}");
    }
    // The read-only counter-case still passes.
    for cmd in [
        "ssh bastion bash -s <<'EOS'\nssh ubuntu@100.64.1.2 uptime\nEOS",
        "ssh bastion 'bash -s' <<EOS\nssh loom-worker-1 uptime\nEOS",
        "ssh bastion bash -s < hop-ro.sh",
        "ssh bastion < hop-ro.sh",
        "cat hop-ro.sh | ssh bastion bash",
    ] {
        assert_eq!(f.ask(cmd), None, "{cmd}");
    }
}

#[test]
fn hops_nested_past_the_depth_limit_are_undeterminable() {
    let f = Fx::new();
    let mut cmd = "ssh 10.0.0.9 uptime".to_string();
    for _ in 0..=MAX_DEPTH {
        cmd = format!("bash -c {}", shell_quote(&cmd));
    }
    let r = f.ask(&cmd).expect("asks");
    assert!(r.contains("nested more than"), "{r}");
    // Within the limit, the same read-only hop passes.
    assert_eq!(f.ask("bash -c 'bash -c \"ssh 10.0.0.9 uptime\"'"), None);
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[test]
fn ssh_options_after_the_destination_are_skipped() {
    let f = Fx::new();
    assert!(f
        .ask("ssh loom-worker-1 -t -o BatchMode=yes 'git reset --hard'")
        .is_some());
    assert!(f.ask("ssh loom-worker-1 -t bash -s < s.sh").is_some());
    assert_eq!(f.ask("ssh loom-worker-1 -t -- uptime"), None);
}
