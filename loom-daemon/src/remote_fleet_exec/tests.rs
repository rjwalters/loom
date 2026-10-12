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
