use super::*;
use std::cell::RefCell;
use std::path::Path;

fn policy(api: &str, canary: Option<&str>) -> Value {
    let mut enforcement = json!({"api": api, "runtimeEgress": "unverified"});
    if let Some(c) = canary {
        enforcement["negativeCanary"] = json!(c);
    }
    json!({
        "schemaVersion": 1,
        "github": {"apiOrigin": "https://github-proxy.example.com:8443"},
        "enforcement": enforcement,
    })
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn hosts() -> Vec<String> {
    BLOCKED_HOSTS.iter().map(|h| (*h).to_string()).collect()
}

#[test]
fn restore_v4_golden() {
    let got = render_restore(
        Family::V4,
        &hosts(),
        &set(&["10.0.0.9"]),
        &set(&["140.82.112.5", "140.82.113.6"]),
        true,
    );
    let want = "\
*filter
:LOOM_FORGE_EGRESS - [0:0]
-A LOOM_FORGE_EGRESS -p tcp --dport 443 -m string --string api.github.com --algo bm -j REJECT --reject-with tcp-reset
-A LOOM_FORGE_EGRESS -p tcp --dport 443 -m string --string uploads.github.com --algo bm -j REJECT --reject-with tcp-reset
-A LOOM_FORGE_EGRESS -d 10.0.0.9/32 -j ACCEPT
-A LOOM_FORGE_EGRESS -d 140.82.112.5/32 -p tcp -j REJECT --reject-with tcp-reset
-A LOOM_FORGE_EGRESS -d 140.82.113.6/32 -p tcp -j REJECT --reject-with tcp-reset
COMMIT
";
    assert_eq!(got, want);
}

#[test]
fn restore_v6_golden_without_sni() {
    let got = render_restore(
        Family::V6,
        &hosts(),
        &set(&["2001:db8::1"]),
        &set(&["2606:50c0:8000::154"]),
        false,
    );
    let want = "\
*filter
:LOOM_FORGE_EGRESS - [0:0]
-A LOOM_FORGE_EGRESS -d 2001:db8::1/128 -j ACCEPT
-A LOOM_FORGE_EGRESS -d 2606:50c0:8000::154/128 -p tcp -j REJECT --reject-with tcp-reset
COMMIT
";
    assert_eq!(got, want);
}

#[test]
fn boundary_is_inert_unless_required() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("policy.json");
    std::fs::write(&file, policy("observe", None).to_string()).unwrap();
    let mut egress = WorkerEgress {
        launcher: "/x/gh".into(),
        upstream_gh: None,
        policy_file: file,
        credential_file: None,
        required: false,
    };
    assert!(Boundary::from_egress(&egress).unwrap().is_none());
    egress.required = true;
    assert!(Boundary::from_egress(&egress).unwrap().is_some());
}

#[test]
fn required_policy_that_cannot_be_reread_aborts_instead_of_going_inert() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("policy.json");
    let egress = |policy_file: std::path::PathBuf| WorkerEgress {
        launcher: "/x/gh".into(),
        upstream_gh: None,
        policy_file,
        credential_file: None,
        required: true,
    };
    // Gone since admission.
    let f =
        Boundary::from_egress(&egress(Path::new("/nonexistent/policy.json").into())).unwrap_err();
    assert_eq!(f.code, "runtime.unverifiable");
    // Replaced with invalid JSON since admission.
    std::fs::write(&file, "{not json").unwrap();
    let f = Boundary::from_egress(&egress(file.clone())).unwrap_err();
    assert_eq!(f.code, "runtime.unverifiable");
    // `establish` surfaces the same refusal and never reaches docker.
    let f = establish(&egress(file), "img", &Options::default()).unwrap_err();
    assert_eq!(f.code, "runtime.unverifiable");
}

#[test]
fn boundary_allows_the_api_origin_and_git_and_defaults_the_canary() {
    let b = Boundary::from_policy(&policy("required", None)).unwrap();
    assert_eq!(b.blocked_hosts, hosts());
    assert_eq!(b.allowed_hosts, vec!["github-proxy.example.com", "github.com"]);
    assert_eq!(b.canary, DEFAULT_CANARY);
    let b = Boundary::from_policy(&policy("required", Some("curl -m 2 https://api.github.com/")))
        .unwrap();
    assert_eq!(b.canary, "curl -m 2 https://api.github.com/");
}

#[test]
fn script_emits_exactly_the_rendered_rules_and_falls_back_without_xt_string() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let write = |name: &str, body: &str| {
        use std::os::unix::fs::PermissionsExt;
        let p = bin.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    write(
        "getent",
        r#"case "$1 $2" in
"ahostsv4 api.github.com") echo "140.82.113.6  STREAM api.github.com";;
"ahostsv4 uploads.github.com") echo "140.82.113.6  STREAM u"; echo "140.82.112.5  STREAM u";;
"ahostsv4 github-proxy.example.com") echo "10.0.0.9  STREAM g";;
"ahostsv4 github.com") echo "140.82.114.4  STREAM g";;
"ahostsv6 api.github.com") echo "2606:50c0:8000::154  STREAM a";;
esac"#,
    );
    // The restore stub records stdin per family; `FAIL_STRING=1` makes it
    // reject a rule set carrying the xt_string rules, like a kernel without it.
    for fam in ["iptables", "ip6tables"] {
        write(
            &format!("{fam}-restore"),
            &format!(
                r#"input=$(cat)
if [ -n "${{FAIL_STRING:-}}" ] && printf '%s' "$input" | grep -q -- '-m string'; then exit 2; fi
printf '%s\n' "$input" > "$CAP/{fam}""#
            ),
        );
        write(fam, "exit 0");
    }
    let run = |fail_string: bool| {
        let cap = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(sidecar_script())
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("CAP", cap.path())
            .env("LOOM_EGRESS_ONCE", "1")
            .env("LOOM_EGRESS_BLOCKED", "api.github.com uploads.github.com")
            .env("LOOM_EGRESS_ALLOWED", "github-proxy.example.com github.com");
        if fail_string {
            cmd.env("FAIL_STRING", "1");
        }
        // READY_FILE is /tmp-absolute; remove any prior one so we assert it is written.
        let _ = std::fs::remove_file(READY_FILE);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let get = |f: &str| std::fs::read_to_string(cap.path().join(f)).unwrap();
        (get("iptables"), get("ip6tables"))
    };
    let blocked_v4 = set(&["140.82.112.5", "140.82.113.6"]);
    let allowed_v4 = set(&["10.0.0.9", "140.82.114.4"]);
    let (v4, v6) = run(false);
    assert_eq!(v4, render_restore(Family::V4, &hosts(), &allowed_v4, &blocked_v4, true));
    assert_eq!(
        v6,
        render_restore(Family::V6, &hosts(), &set(&[]), &set(&["2606:50c0:8000::154"]), true)
    );
    let (v4, v6) = run(true);
    assert_eq!(v4, render_restore(Family::V4, &hosts(), &allowed_v4, &blocked_v4, false));
    assert!(!v6.contains("-m string"));
}

#[test]
fn script_refuses_to_install_when_the_blocked_hosts_do_not_resolve() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let write = |name: &str, body: &str| {
        let p = bin.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    // DNS blip: only the allowed host answers.
    write("getent", r#"[ "$2" = github.com ] && echo "140.82.114.4  STREAM g""#);
    write("iptables-restore", "cat > \"$CAP/restored\"");
    write("ip6tables-restore", "cat > /dev/null");
    write("iptables", "exit 0");
    write("ip6tables", "exit 1");
    let cap = tempfile::tempdir().unwrap();
    let out = Command::new("sh")
        .arg("-c")
        .arg(sidecar_script())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("CAP", cap.path())
        .env("LOOM_EGRESS_ONCE", "1")
        .env("LOOM_EGRESS_BLOCKED", "api.github.com uploads.github.com")
        .env("LOOM_EGRESS_ALLOWED", "github.com")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(70), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!cap.path().join("restored").exists(), "no empty chain is installed");
}

#[test]
fn canary_exit_status_classification() {
    assert_eq!(classify_canary(Some((0, String::new()))), CanaryOutcome::Open);
    // Connection refused/reset (7), TLS reset mid-handshake (35, 56): blocked.
    for code in [7, 35, 56] {
        assert_eq!(classify_canary(Some((code, String::new()))), CanaryOutcome::Blocked, "{code}");
    }
    // Inconclusive: docker/tool errors, timeouts, DNS (6), TLS trust (60),
    // usage/config errors (2).
    for code in [125, 126, 127, 124, 137, 28, 6, 60, 2] {
        assert!(
            matches!(classify_canary(Some((code, String::new()))), CanaryOutcome::NotRun(_)),
            "{code}"
        );
    }
    assert!(matches!(classify_canary(None), CanaryOutcome::NotRun(_)));
    // A block with a dead allowed gateway is unproven.
    assert_eq!(
        require_positive(CanaryOutcome::Blocked, Some((0, String::new()))),
        CanaryOutcome::Blocked
    );
    for positive in [Some((7, String::new())), Some((6, String::new())), None] {
        assert!(matches!(
            require_positive(CanaryOutcome::Blocked, positive),
            CanaryOutcome::NotRun(_)
        ));
    }
    assert_eq!(require_positive(CanaryOutcome::Open, None), CanaryOutcome::Open);
}

#[test]
fn doctor_runtime_section_states() {
    // The doctor's runtime section and the spawn canary share one classifier.
    let b = Boundary::from_policy(&policy("required", None)).unwrap();
    // verified: only from a blocked canary — no finding.
    assert!(canary_findings(&b, CanaryOutcome::Blocked).is_empty());
    let open = canary_findings(&b, CanaryOutcome::Open);
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].code, "runtime.bypass-open");
    let unrun = canary_findings(&b, CanaryOutcome::NotRun("x"));
    assert_eq!(unrun[0].code, "runtime.unverifiable");
    // Bare metal with no canary configured stays unverifiable, never verified.
    let bare = crate::forge_egress::checks::assert_runtime(
        &policy("required", None),
        &Observed::default(),
    );
    assert_eq!(bare[0].code, "runtime.unverifiable");
    // ...and a config-only `verified` claim is rejected.
    let mut claimed = policy("required", None);
    claimed["enforcement"]["runtimeEgress"] = json!("verified");
    let bare = crate::forge_egress::checks::assert_runtime(&claimed, &Observed::default());
    assert_eq!(bare[0].code, "runtime.verified-without-canary");
}

/// Scripted docker: records every call, answers by subcommand.
struct Fake {
    calls: RefCell<Vec<Vec<String>>>,
    start: Option<(i32, String)>,
    ready: Option<(i32, String)>,
    running: &'static str,
    canary: Option<(i32, String)>,
    positive: Option<(i32, String)>,
}

impl Fake {
    fn ok(canary: Option<(i32, String)>) -> Self {
        Self {
            calls: RefCell::default(),
            start: Some((0, "id".into())),
            ready: Some((0, String::new())),
            running: "true",
            canary,
            positive: Some((0, String::new())),
        }
    }
    fn removed(&self) -> bool {
        self.calls.borrow().iter().any(|c| {
            c.first().map(String::as_str) == Some("rm")
                && c.get(1).map(String::as_str) == Some("-f")
        })
    }
}

impl Docker for Fake {
    fn run(&self, args: &[String], _: Duration) -> Option<(i32, String)> {
        self.calls.borrow_mut().push(args.to_vec());
        match args.first().map(String::as_str) {
            Some("rm") | Some("logs") => Some((0, "log tail".into())),
            Some("exec") => self.ready.clone(),
            Some("inspect") => Some((0, self.running.into())),
            Some("run") if args.contains(&"-d".to_string()) => self.start.clone(),
            Some("run") if args.last() == Some(&boundary().positive_probe) => self.positive.clone(),
            Some("run") => self.canary.clone(),
            _ => None,
        }
    }
}

fn boundary() -> Boundary {
    Boundary::from_policy(&policy("required", None)).unwrap()
}

fn go(fake: &Fake) -> Result<Option<Sidecar>, Box<Finding>> {
    establish_with(fake, &boundary(), "img", "img", &Options::default(), Duration::from_millis(300))
}

#[test]
fn blocked_canary_admits_the_worker_into_the_sidecar() {
    let fake = Fake::ok(Some((7, "curl: (7)".into())));
    let sidecar = go(&fake).unwrap().expect("isolated");
    assert!(!fake.removed(), "a live boundary is kept for the worker");
    let args = sidecar.network_args();
    assert_eq!(args[0], "--network");
    assert_eq!(args[1], format!("container:{}", sidecar.name));
    // The canary ran in the worker's namespace, with no capabilities, and the
    // sidecar (not the worker) got NET_ADMIN.
    let calls = fake.calls.borrow();
    let canary = calls
        .iter()
        .find(|c| c.contains(&"--rm".to_string()) && !c.contains(&"-d".to_string()))
        .unwrap();
    assert!(canary.contains(&format!("container:{}", sidecar.name)));
    assert!(canary.contains(&DEFAULT_CANARY.to_string()));
    assert!(!canary.contains(&"NET_ADMIN".to_string()));
    let start = calls
        .iter()
        .find(|c| c.contains(&"-d".to_string()))
        .unwrap();
    assert!(start.contains(&"NET_ADMIN".to_string()) && start.contains(&"ALL".to_string()));
    // The positive control ran against the allowed gateway.
    assert!(calls
        .iter()
        .any(|c| c.last() == Some(&boundary().positive_probe)));
}

#[test]
fn blocked_canary_without_a_reachable_gateway_is_unverifiable() {
    for positive in [
        Some((6, "could not resolve".into())),
        Some((7, String::new())),
        None,
    ] {
        let mut fake = Fake::ok(Some((7, "curl: (7)".into())));
        fake.positive = positive;
        assert_eq!(go(&fake).unwrap_err().code, "runtime.unverifiable");
        assert!(fake.removed());
    }
}

#[test]
fn dns_tls_and_config_failures_do_not_verify_the_boundary() {
    for code in [6, 60, 2, 28] {
        let fake = Fake::ok(Some((code, String::new())));
        assert_eq!(go(&fake).unwrap_err().code, "runtime.unverifiable", "{code}");
        assert!(fake.removed());
    }
}

#[test]
fn positive_probe_targets_the_api_origin_else_git() {
    assert_eq!(
        boundary().positive_probe,
        "curl -sS --max-time 5 -o /dev/null https://github-proxy.example.com:8443/"
    );
    let mut p = policy("required", None);
    p["github"]["apiOrigin"] = json!("https://api.github.com");
    assert_eq!(
        Boundary::from_policy(&p).unwrap().positive_probe,
        "curl -sS --max-time 5 -o /dev/null https://github.com/"
    );
}

#[test]
fn succeeding_canary_is_bypass_open_and_aborts_the_spawn() {
    let fake = Fake::ok(Some((0, "{\"zen\":true}".into())));
    let finding = go(&fake).unwrap_err();
    assert_eq!(finding.code, "runtime.bypass-open");
    assert!(fake.removed(), "the sidecar is torn down before the agent starts");
    let msg = crate::forge_egress::worker_env::refusal_message(&finding);
    assert!(
        msg.contains("enforcement.api=required") && msg.contains("runtime.bypass-open"),
        "{msg}"
    );
}

#[test]
fn a_canary_that_cannot_run_is_unverifiable_and_aborts() {
    for canary in [Some((127, "curl: not found".into())), None] {
        let fake = Fake::ok(canary);
        assert_eq!(go(&fake).unwrap_err().code, "runtime.unverifiable");
        assert!(fake.removed());
    }
}

#[test]
fn rules_that_never_install_abort_without_running_the_canary() {
    let mut fake = Fake::ok(Some((7, String::new())));
    fake.ready = Some((1, String::new()));
    fake.running = "false"; // the sidecar exited 70
    assert_eq!(go(&fake).unwrap_err().code, "runtime.unverifiable");
    assert!(fake.removed());
    assert!(!fake
        .calls
        .borrow()
        .iter()
        .any(|c| c.contains(&DEFAULT_CANARY.to_string())));
    let mut fake = Fake::ok(None);
    fake.start = Some((125, "no such image".into()));
    assert_eq!(go(&fake).unwrap_err().code, "runtime.unverifiable");
}

#[test]
fn sidecar_args_carry_hosts_and_gateway_only_when_asked() {
    let b = boundary();
    let plain = sidecar_args("n", "img", &b, &Options::default(), 30);
    assert!(!plain.contains(&"--add-host".to_string()));
    assert!(plain.contains(&"LOOM_EGRESS_BLOCKED=api.github.com uploads.github.com".to_string()));
    assert!(plain.contains(&"LOOM_EGRESS_ALLOWED=github-proxy.example.com github.com".to_string()));
    let gw = sidecar_args(
        "n",
        "img",
        &b,
        &Options {
            add_host_gateway: true,
            watch_pid: None,
        },
        30,
    );
    assert!(gw.contains(&"host.docker.internal:host-gateway".to_string()));
}

#[test]
fn sidecar_runs_as_root_with_only_the_network_capabilities() {
    let a = sidecar_args("n", "img", &boundary(), &Options::default(), 30);
    let at = |flag: &str| a.iter().position(|x| x == flag).unwrap();
    assert_eq!(a[at("--user") + 1], "0:0");
    assert_eq!(a[at("--cap-drop") + 1], "ALL");
    let caps: Vec<_> = a
        .windows(2)
        .filter(|w| w[0] == "--cap-add")
        .map(|w| w[1].as_str())
        .collect();
    assert_eq!(caps, ["NET_ADMIN", "NET_RAW"]);
    // The worker's network args set no identity or capability.
    let net = Sidecar { name: "n".into() }.network_args();
    assert!(!net.iter().any(|x| x == "--user" || x == "--cap-add"));
}

/// Real-docker matrix; skips cleanly without Docker or the opt-in image
/// (`LOOM_EGRESS_DOCKER_TEST_IMAGE`: a worker image with iptables + curl, and
/// outbound network). With a policy whose canary hits `api.github.com`, the
/// canary must be blocked and `git ls-remote` over github.com must still work.
#[test]
fn docker_boundary_blocks_api_and_keeps_git_transport() {
    let Some(image) = std::env::var("LOOM_EGRESS_DOCKER_TEST_IMAGE")
        .ok()
        .filter(|i| !i.is_empty())
    else {
        eprintln!("skip: LOOM_EGRESS_DOCKER_TEST_IMAGE not set");
        return;
    };
    if Command::new("docker")
        .arg("info")
        .output()
        .map_or(true, |o| !o.status.success())
    {
        eprintln!("skip: docker unavailable");
        return;
    }
    // A real, reachable gateway: the positive probe must pass for real.
    let mut live = policy("required", None);
    live["github"]["apiOrigin"] = json!("https://github.com");
    let b = Boundary::from_policy(&live).unwrap();
    let sidecar = establish_with(
        &RealDocker,
        &b,
        &image,
        &image,
        &Options::default(),
        Duration::from_secs(30),
    )
    .expect("boundary installed and canary blocked")
    .expect("isolated");
    let run = |cmd: &str| {
        RealDocker
            .run(
                &[
                    "run".into(),
                    "--rm".into(),
                    "--network".into(),
                    format!("container:{}", sidecar.name),
                    "--entrypoint".into(),
                    "sh".into(),
                    image.clone(),
                    "-c".into(),
                    cmd.into(),
                ],
                Duration::from_secs(40),
            )
            .map(|(c, _)| c)
    };
    assert_ne!(run("curl -sS --max-time 5 https://api.github.com/zen"), Some(0));
    assert_ne!(run("curl -sS -6 --max-time 5 https://api.github.com/zen"), Some(0));
    // The worker holds no network capability: it cannot remove the rules.
    assert_ne!(run("iptables -F LOOM_FORGE_EGRESS"), Some(0));
    assert_eq!(run("git ls-remote https://github.com/rjwalters/loom HEAD >/dev/null"), Some(0));
    RealDocker.run(&["rm".into(), "-f".into(), sidecar.name], Duration::from_secs(20));
}
