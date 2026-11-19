//! #12772: unattended recovery of a private session, driven through the fake
//! Docker seam ([`docker::fake`]) so the rules a live engine would otherwise
//! be the only witness to are pinned here: the wrong engine or container is
//! refused, the exclusive account lease is never broken, a stopped container
//! is started (never removed or recreated), a missing one is recreated only
//! against its surviving volume and recorded image, and an operator `stop`
//! holds the account down until an operator `start` lifts it.

use super::*;
use crate::tokens_pool::account_lifecycle::{AccountLifecycle, ProcessCodexRunner};
use crate::tokens_pool::account_registry::{account_inventory, AccountDescriptor, AccountProvider};
use crate::tokens_pool::docker_cli;
use crate::tokens_pool::profile_root_env::ProfileRootEnv;
use crate::tokens_pool::session_hold::{self, OperatorHeld};
use crate::tokens_pool::session_lifecycle::container_name;
use serde_json::{json, Value};
use serial_test::serial;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

const ENGINE: &str = "engine-a";
const IMAGE: &str = "example/private-session:pinned";
const CONTAINER_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// One fake engine: containers and volumes as `docker inspect` would report
/// them, the local images, and every call made against it.
#[derive(Clone, Default)]
struct Engine {
    id: String,
    containers: BTreeMap<String, Value>,
    /// The `args` column of `docker top` per container.
    processes: BTreeMap<String, Vec<String>>,
    volumes: BTreeMap<String, Value>,
    images: Vec<String>,
    /// The account profile a control report is cross-checked against.
    profile: PathBuf,
    /// Per-container profiles (#14578): a control report is cross-checked
    /// against the profile of the CONTAINER it addresses, so a draw over
    /// several seats sees each seat's own boundary. Containers absent here
    /// fall back to `profile`.
    profiles: BTreeMap<String, PathBuf>,
    /// `docker start` leaves the container stopped (its process exits at once).
    start_dies: bool,
    /// `docker start` hits its deadline (a wedged engine).
    start_times_out: bool,
    /// The verbatim body the fake returns for `private-workspace recover`
    /// execs; `None` answers with an empty published report (#14030).
    recover_response: Option<String>,
    /// The verbatim body for `private-workspace rebind` execs (#14357).
    rebind_response: Option<String>,
    /// What the sealed-bundle probe of a configured override image prints
    /// (#14451); `None` fails the probe.
    probe_output: Option<String>,
    calls: Vec<Vec<String>>,
    created: u32,
}

impl Engine {
    fn names(&self) -> String {
        self.containers
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn find(&self, target: &str) -> Option<&Value> {
        self.containers
            .get(target)
            .or_else(|| self.containers.values().find(|c| c["Id"] == target))
    }

    fn find_name(&self, target: &str) -> Option<String> {
        self.containers
            .iter()
            .find(|(name, c)| *name == target || c["Id"] == target)
            .map(|(name, _)| name.clone())
    }

    /// Containers attached to the volume (by name) or bind source `source`.
    fn attached(&self, source: &str) -> String {
        self.containers
            .iter()
            .filter(|(_, c)| {
                c["Mounts"].as_array().is_some_and(|mounts| {
                    mounts
                        .iter()
                        .any(|m| m["Name"] == source || m["Source"] == source)
                })
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn handle(&mut self, args: &[&str]) -> Result<String> {
        self.calls
            .push(args.iter().map(ToString::to_string).collect());
        match args {
            ["info", "--format", "{{.ID}}"] => Ok(self.id.clone()),
            ["info", "--format", "{{.OperatingSystem}}"] => Ok("Docker Engine - Community".into()),
            ["container", "ls", "-a", "--format", "{{.Names}}"] => Ok(self.names()),
            ["container", "ls", "-aq", "--no-trunc"] => Ok(self
                .containers
                .values()
                .map(|c| c["Id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
                .join("\n")),
            ["inspect", target] => {
                let container = self.find(target).context("no such container")?;
                Ok(json!([container]).to_string())
            }
            ["ps", "-a", "--filter", filter, "--format", "{{.Names}}"] => {
                let source = filter
                    .strip_prefix("volume=")
                    .context("unexpected filter")?;
                Ok(self.attached(source))
            }
            ["top", container, "-o", "pid,args"] => {
                let processes = self.processes.get(*container).context("not running")?;
                let mut out = String::from("PID ARGS\n");
                for (i, p) in processes.iter().enumerate() {
                    out.push_str(&format!("{} {p}\n", i + 1));
                }
                Ok(out)
            }
            ["volume", "ls", "--format", "{{.Name}}"] => {
                Ok(self.volumes.keys().cloned().collect::<Vec<_>>().join("\n"))
            }
            ["volume", "inspect", name] => {
                let volume = self.volumes.get(*name).context("no such volume")?;
                Ok(json!([volume]).to_string())
            }
            ["volume", "create", rest @ ..] => {
                let name = rest.last().unwrap();
                self.volumes
                    .insert((*name).to_owned(), volume_json(name, &labels_of(rest)));
                Ok((*name).to_owned())
            }
            ["image", "inspect", "--format", "{{.Id}}", image] => {
                if self.images.iter().any(|i| i == image) {
                    Ok("sha256:0".into())
                } else {
                    bail!("Docker image failed (exit status: 1)")
                }
            }
            ["start", _] if self.start_times_out => Err(docker_cli::DockerTimedOut {
                subcommand: "start".into(),
                secs: docker_cli::DOCKER_CALL_TIMEOUT.as_secs(),
            }
            .into()),
            ["start", container] => {
                let state = self
                    .containers
                    .get_mut(*container)
                    .context("no such container")?;
                if !self.start_dies {
                    state["State"]["Running"] = json!(true);
                    self.processes
                        .insert((*container).to_owned(), idle_processes());
                }
                Ok(String::new())
            }
            ["run", rest @ ..] if rest.contains(&"--rm") && rest.contains(&"/bin/sh") => self
                .probe_output
                .clone()
                .context("fake Docker: bundle probe failed"),
            ["run", rest @ ..] if rest.contains(&"--rm") => {
                if rest.ends_with(&["private-workspace", "provision-controls"]) {
                    Ok(json!({"protocol": PROTOCOL, "status": "ready"}).to_string())
                } else {
                    // The fresh-volume `chown` initializer.
                    Ok(String::new())
                }
            }
            ["run", rest @ ..] => {
                let run = parse_run(rest);
                self.created += 1;
                let id = format!("{:0>64}", format!("c{}", self.created));
                let name = run.name.clone();
                let container = run.inspect(&id, true);
                self.containers.insert(name.clone(), container);
                self.processes.insert(name, idle_processes());
                Ok(id)
            }
            ["exec", .., "private-workspace", "setup"] => {
                Ok(json!({"protocol": PROTOCOL, "status": "ready"}).to_string())
            }
            ["exec", target, "loom-daemon", "private-workspace", "control"] => {
                // Addressed by immutable id, never by name.
                assert_eq!(target.len(), 64, "control probe must address the container id");
                self.find(target).context("no such container")?;
                let profile = self
                    .find_name(target)
                    .and_then(|name| self.profiles.get(&name).cloned())
                    .unwrap_or_else(|| self.profile.clone());
                let report = bundle::Report {
                    protocol: bundle::CONTROL_PROTOCOL.into(),
                    control_version: bundle::CONTROL_VERSION,
                    status: bundle::Status::Ready,
                    codex_cli: "codex 0.0.0".into(),
                    identity: "ab".repeat(32),
                    profile: bundle::profile_digests(&profile),
                    managed: true,
                };
                Ok(serde_json::to_string(&report)?)
            }
            ["exec", rest @ ..] if rest.contains(&"prepare") => {
                Ok(json!({"protocol": PROTOCOL, "revision": "a".repeat(40)}).to_string())
            }
            ["exec", rest @ ..] if rest.contains(&"rebind") => self
                .rebind_response
                .clone()
                .context("no rebind response seeded"),
            ["exec", rest @ ..] if rest.contains(&"recover") => match &self.recover_response {
                Some(body) => Ok(body.clone()),
                None => Ok(json!({
                    "protocol": PROTOCOL,
                    "outcome": {
                        "repository": "https://forge.example/a/b",
                        "cloned": true,
                        "branches": [],
                        "published": [],
                        "discarded": [],
                        "ignored": [],
                        "detached": []
                    }
                })
                .to_string()),
            },
            ["stop", .., container] => {
                let name = self.find_name(container).context("no such container")?;
                self.containers.get_mut(&name).unwrap()["State"]["Running"] = json!(false);
                self.processes.remove(&name);
                Ok(String::new())
            }
            ["rm", container] => {
                let name = self.find_name(container).context("no such container")?;
                self.containers.remove(&name);
                self.processes.remove(&name);
                Ok(String::new())
            }
            other => bail!("fake Docker: unexpected call {other:?}"),
        }
    }

    /// Calls that change engine state, rendered as `<verb> <object>`.
    fn mutations(&self) -> Vec<String> {
        self.calls
            .iter()
            .filter_map(|call| {
                let args: Vec<&str> = call.iter().map(String::as_str).collect();
                Some(match args.as_slice() {
                    ["start", c] => format!("start {c}"),
                    ["stop", .., c] => format!("stop {c}"),
                    ["rm", c] => format!("rm {c}"),
                    ["volume", "create", .., v] => format!("volume create {v}"),
                    ["volume", "rm", v] => format!("volume rm {v}"),
                    ["run", rest @ ..] if rest.contains(&"--rm") => {
                        format!("run --rm {}", rest.last().unwrap())
                    }
                    ["run", rest @ ..] => format!("run {}", parse_run(rest).image),
                    ["exec", .., last] => format!("exec {last}"),
                    _ => return None,
                })
            })
            .collect()
    }
}

fn idle_processes() -> Vec<String> {
    vec![
        "/usr/bin/tini -- /bin/sleep infinity".into(),
        "/bin/sleep infinity".into(),
    ]
}

fn labels_of(args: &[&str]) -> BTreeMap<String, String> {
    args.windows(2)
        .filter(|w| w[0] == "--label")
        .filter_map(|w| w[1].split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn volume_json(name: &str, labels: &BTreeMap<String, String>) -> Value {
    json!({"Name": name, "Driver": "local", "Options": null, "Labels": labels})
}

/// What a `docker run` asked for, enough to answer `docker inspect` for it.
struct Run {
    name: String,
    user: String,
    read_only: bool,
    cap_drop: Vec<String>,
    security_opt: Vec<String>,
    mounts: Vec<Value>,
    labels: BTreeMap<String, String>,
    entrypoint: String,
    image: String,
    cmd: Vec<String>,
}

impl Run {
    fn inspect(&self, id: &str, running: bool) -> Value {
        json!({
            "Id": id,
            "Name": format!("/{}", self.name),
            "State": {"Running": running, "Restarting": false, "Paused": false},
            "ExecIDs": null,
            "Config": {
                "Image": self.image,
                "User": self.user,
                "Entrypoint": [self.entrypoint],
                "Cmd": self.cmd,
                "Labels": self.labels,
            },
            "HostConfig": {
                "Privileged": false,
                "ReadonlyRootfs": self.read_only,
                "CapDrop": self.cap_drop,
                "SecurityOpt": self.security_opt,
            },
            "Mounts": self.mounts,
        })
    }
}

fn parse_mount(spec: &str) -> Value {
    let mut kind = "";
    let mut src = "";
    let mut dst = "";
    let mut read_only = false;
    for part in spec.split(',') {
        match part.split_once('=') {
            Some(("type", v)) => kind = v,
            Some(("src", v)) => src = v,
            Some(("dst", v)) => dst = v,
            None if part == "readonly" => read_only = true,
            other => panic!("unexpected mount option {other:?}"),
        }
    }
    match kind {
        "volume" => json!({"Destination": dst, "Type": "volume", "Name": src, "RW": !read_only}),
        "bind" => json!({"Destination": dst, "Type": "bind", "Source": src, "RW": !read_only}),
        other => panic!("unexpected mount type {other}"),
    }
}

fn parse_run(args: &[&str]) -> Run {
    let mut run = Run {
        name: String::new(),
        user: String::new(),
        read_only: false,
        cap_drop: vec![],
        security_opt: vec![],
        mounts: vec![],
        labels: BTreeMap::new(),
        entrypoint: String::new(),
        image: String::new(),
        cmd: vec![],
    };
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "-d" | "--read-only" | "--rm" => {
                run.read_only |= args[i] == "--read-only";
                i += 1;
            }
            "--name" => {
                run.name = args[i + 1].into();
                i += 2;
            }
            "--user" => {
                run.user = args[i + 1].into();
                i += 2;
            }
            "--cap-drop" => {
                run.cap_drop.push(args[i + 1].into());
                i += 2;
            }
            "--security-opt" => {
                run.security_opt.push(args[i + 1].into());
                i += 2;
            }
            "--mount" => {
                run.mounts.push(parse_mount(args[i + 1]));
                i += 2;
            }
            "--label" => {
                let (k, v) = args[i + 1].split_once('=').unwrap();
                run.labels.insert(k.into(), v.into());
                i += 2;
            }
            "--tmpfs" | "--env" | "--network" | "--cap-add" => i += 2,
            "--entrypoint" => {
                run.entrypoint = args[i + 1].into();
                i += 2;
            }
            image => {
                run.image = image.into();
                run.cmd = args[i + 1..].iter().map(ToString::to_string).collect();
                break;
            }
        }
    }
    run
}

/// The session container `docker::create` would make for `config`, as
/// `docker inspect` reports it, in the given state.
fn session_container(config: &Config, id: &str, running: bool, image: &str) -> Value {
    let mut labels = BTreeMap::new();
    labels.insert("loom.workspace-mode".to_owned(), MODE.to_owned());
    labels.insert("loom.workspace".to_owned(), REPO.to_owned());
    labels.insert("loom.account".to_owned(), config.account.clone());
    labels.insert("loom.repository".to_owned(), config.repository.clone());
    let mut mounts = vec![
        json!({"Destination": ROOT, "Type": "volume", "Name": config.volume, "RW": true}),
        json!({"Destination": PROFILE, "Type": "bind", "Source": config.profile, "RW": true}),
    ];
    for name in bundle::PROFILE_CONTROLS {
        mounts.push(json!({
            "Destination": bundle::control_destination(name),
            "Type": "bind",
            "Source": config.profile.join(name),
            "RW": false,
        }));
    }
    for extra in &config.extra_mounts {
        mounts.push(json!({
            "Destination": extra.container,
            "Type": "bind",
            "Source": extra.host,
            "RW": false,
        }));
    }
    Run {
        name: config.container.clone(),
        user: "1000:1000".into(),
        read_only: true,
        cap_drop: vec!["ALL".into()],
        security_opt: vec!["no-new-privileges".into()],
        mounts,
        labels,
        entrypoint: "/usr/bin/tini".into(),
        image: image.into(),
        cmd: vec!["--".into(), "/bin/sleep".into(), "infinity".into()],
    }
    .inspect(id, running)
}

struct Env {
    workspace: tempfile::TempDir,
    _root: tempfile::TempDir,
    _env: ProfileRootEnv,
    account: AccountDescriptor,
    config: Config,
    dir: PathBuf,
    engine: Rc<RefCell<Engine>>,
    _fake: docker::fake::Guard,
}

impl Env {
    fn engine(&self) -> std::cell::Ref<'_, Engine> {
        self.engine.borrow()
    }
    fn with(&self, f: impl FnOnce(&mut Engine)) {
        f(&mut self.engine.borrow_mut());
    }
    fn seed_container(&self, id: &str, running: bool) {
        let container = session_container(&self.config, id, running, IMAGE);
        self.with(|e| {
            e.containers
                .insert(self.config.container.clone(), container);
            if running {
                e.processes
                    .insert(self.config.container.clone(), idle_processes());
            }
        });
    }
    fn seed_volume(&self) {
        let mut labels = BTreeMap::new();
        labels.insert("loom.account".to_owned(), self.config.account.clone());
        labels.insert("loom.repository".to_owned(), self.config.repository.clone());
        labels
            .insert("loom.profile".to_owned(), self.config.profile.to_string_lossy().into_owned());
        self.with(|e| {
            e.volumes
                .insert(self.config.volume.clone(), volume_json(&self.config.volume, &labels));
        });
    }
    fn mutations(&self) -> Vec<String> {
        self.engine().mutations()
    }
    fn recover(&self, image: Option<&str>) -> Result<Status> {
        // A git fork anywhere in this test binary (thousands of tests drive
        // real git in parallel threads) inherits copies of every open
        // descriptor between fork and exec, and can keep this lease's flock
        // alive microseconds past its release (the #9409 transient). A real
        // holder persists; the transient clears, so a bounded retry of the
        // busy error — which `Lease::acquire` reports before any mutation —
        // tells the two apart, exactly like `reacquire` does in tests.rs.
        let mut attempt = 0;
        loop {
            match recover(self.workspace.path(), "alice", image, &|| Ok(())) {
                Err(error) if attempt < 20 && error.to_string().contains("busy") => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                other => return other,
            }
        }
    }

    /// Single-shot recovery for the tests that pin the exact engine-call
    /// sequence, where a retried probe would show up in the record.
    fn recover_strict(&self, image: Option<&str>) -> Result<Status> {
        recover(self.workspace.path(), "alice", image, &|| Ok(()))
    }
    fn held(&self) -> bool {
        session_hold::held_across(std::slice::from_ref(&self.account.credential_reference))
    }
}

/// A configured private account `alice` on a fake engine with nothing on it.
fn setup() -> Env {
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let env = ProfileRootEnv::set(root.path());
    let service = AccountLifecycle::new(workspace.path(), ProcessCodexRunner).unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("auth.json");
    std::fs::write(&source, "fake-secret").unwrap();
    service.import_with_email("alice", &source, None).unwrap();
    let account = account_inventory(workspace.path(), AccountProvider::Codex)
        .unwrap()
        .into_iter()
        .find(|a| a.id.name == "alice")
        .unwrap();
    let profile = account.credential_reference.canonicalize().unwrap();
    for name in bundle::PROFILE_CONTROLS {
        std::fs::write(profile.join(name), name).unwrap();
    }
    let config = Config {
        schema_version: 1,
        account: "alice".into(),
        container: container_name("alice"),
        engine: ENGINE.into(),
        docker_desktop: false,
        repository: "https://forge.example/a/b".into(),
        base: "main".into(),
        repositories: vec![],
        volume: "loom-codex-workspace-alice".into(),
        profile: profile.clone(),
        gh_config: None,
        session_image: None,
        extra_mounts: Vec::new(),
    };
    let dir = state_dir(&account.credential_reference).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    save(&dir.join("workspace.json"), &config).unwrap();
    let engine = Rc::new(RefCell::new(Engine {
        id: ENGINE.into(),
        images: vec![IMAGE.into()],
        profile,
        ..Engine::default()
    }));
    let handler = engine.clone();
    let fake = docker::fake::install(Box::new(move |args| handler.borrow_mut().handle(args)));
    Env {
        workspace,
        _root: root,
        _env: env,
        account,
        config,
        dir,
        engine,
        _fake: fake,
    }
}

fn message(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

#[test]
#[serial]
fn a_stopped_private_session_is_started_in_place_never_removed_or_recreated() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, false);
    let status = env.recover(Some(IMAGE)).unwrap();
    assert!(status.running);
    assert_eq!(status.container_id.as_deref(), Some(CONTAINER_ID));
    // The one mutation is `docker start`; the controls are then re-verified.
    // No `rm`, no `run`, no volume change, and no `prepare` (which would
    // clone/fetch/reset over unfinished work on the volume).
    assert_eq!(
        env.mutations(),
        vec![
            format!("start {}", env.config.container),
            "exec setup".to_owned(),
            "exec control".to_owned(),
        ]
    );
    assert!(env
        .engine()
        .calls
        .iter()
        .all(|c| !c.iter().any(|a| a == "prepare")));
    assert!(env.engine().volumes.contains_key(&env.config.volume));
    assert!(env.engine().containers[&env.config.container]["Id"] == CONTAINER_ID);
    // The image hint is irrelevant for a stopped container: none is needed.
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, false);
    assert!(env.recover(None).unwrap().running);
}

#[test]
#[serial]
fn a_missing_private_session_is_recreated_only_against_its_surviving_volume() {
    let env = setup();
    env.seed_volume();
    let status = env.recover(Some(IMAGE)).unwrap();
    assert!(status.running);
    assert_eq!(
        env.mutations(),
        vec![
            "run --rm provision-controls".to_owned(),
            format!("run {IMAGE}"),
            "exec setup".to_owned(),
            "exec control".to_owned(),
        ],
        "no `volume create` and no fresh-volume initializer: the existing volume is reused"
    );
    let run = env
        .engine()
        .calls
        .iter()
        .find(|c| c[0] == "run" && !c.contains(&"--rm".to_owned()))
        .cloned()
        .unwrap();
    let run: Vec<&str> = run.iter().map(String::as_str).collect();
    let created = parse_run(&run[1..]);
    assert_eq!(created.name, env.config.container);
    assert!(created.mounts.iter().any(|m| {
        m["Type"] == "volume" && m["Name"] == env.config.volume && m["Destination"] == ROOT
    }));
    // The lease-renewal store lives on the cache volume, never in the clone's
    // work tree (#14160).
    let envs: Vec<&str> = run[1..]
        .windows(2)
        .filter(|w| w[0] == "--env")
        .map(|w| w[1])
        .collect();
    assert!(
        envs.contains(&"LOOM_LEASE_RENEW_STATE_DIR=/workspace/cache/lease-renew"),
        "{envs:?}"
    );
    // What it made passes the same validation a reused container must.
    let state = env.engine().containers[&env.config.container].clone();
    docker::validate_settings(&env.config, &state).unwrap();
}

#[test]
#[serial]
fn recovery_refuses_to_replace_a_missing_volume_or_substitute_an_image() {
    // No volume: `create` would silently make an empty one.
    let env = setup();
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(message(&error).contains("volume"), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());
    // Volume, but no recorded image: nothing is substituted.
    env.seed_volume();
    let error = env.recover(None).unwrap_err();
    assert!(message(&error).contains("image is unknown"), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());
    // A recorded image that is not local: never pulled.
    let error = env
        .recover(Some("example/private-session:gone"))
        .unwrap_err();
    assert!(message(&error).contains("not available locally"), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());
}

#[test]
#[serial]
fn recovery_refuses_another_engine_before_reading_any_container() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, false);
    env.with(|e| e.id = "engine-b".into());
    let error = env.recover_strict(Some(IMAGE)).unwrap_err();
    assert!(message(&error).contains("engine changed"), "{error:#}");
    assert_eq!(env.engine().calls, vec![vec!["info", "--format", "{{.ID}}"]]);
}

#[test]
#[serial]
fn recovery_refuses_a_container_that_is_not_this_accounts_session() {
    // Same name, another account's labels.
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, false);
    env.with(|e| {
        e.containers.get_mut(&env.config.container).unwrap()["Config"]["Labels"]["loom.account"] =
            json!("mallory");
    });
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(message(&error).contains("identity differs"), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());

    // The right container, but the workspace volume is shared with another.
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, false);
    let intruder = session_container(&env.config, OTHER_ID, true, IMAGE);
    env.with(|e| {
        e.containers.insert("intruder".into(), intruder);
    });
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(message(&error).contains("another container"), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());

    // Missing by name, and the volume belongs to someone else.
    let env = setup();
    env.seed_volume();
    env.with(|e| {
        e.volumes.get_mut(&env.config.volume).unwrap()["Labels"]["loom.account"] = json!("mallory");
    });
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(message(&error).contains("owned by this account"), "{error:#}");
    assert_eq!(env.mutations(), vec!["run --rm provision-controls".to_owned()]);
}

#[test]
#[serial]
fn recovery_never_breaks_the_exclusive_account_lease() {
    // Another holder (a job, a sweep, an operator lifecycle command): busy.
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, false);
    let holder = lease::Lease::acquire(&env.dir).unwrap();
    let error = env.recover_strict(Some(IMAGE)).unwrap_err();
    assert!(message(&error).contains("busy"), "{error:#}");
    assert_eq!(env.engine().calls, vec![vec!["info", "--format", "{{.ID}}"]]);
    drop(holder);

    // A durable job lease on a different container identity: preserved.
    let job = lease::Job {
        owner: "sweep-1".into(),
        kind: JobKind::Sweep,
        issue: None,
        branch: None,
        container_id: OTHER_ID.into(),
        base_revision: "deadbeef".into(),
        host_pid: 1,
        control: String::new(),
        repository: String::new(),
    };
    save(&env.dir.join("job.json"), &job).unwrap();
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(message(&error).contains("another container identity"), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());
    assert!(env.dir.join("job.json").exists());

    // Missing by name, but the leased container still runs under another
    // name: the original worker may be alive.
    let env = setup();
    env.seed_volume();
    save(&env.dir.join("job.json"), &job).unwrap();
    let renamed = session_container(&env.config, OTHER_ID, true, IMAGE);
    env.with(|e| {
        e.containers.insert("renamed".into(), renamed);
        e.processes.insert("renamed".into(), idle_processes());
    });
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(message(&error).contains("still exists/runs"), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());
    assert!(env.dir.join("job.json").exists());

    // A stale lease on the very container that stopped: the crashed job is
    // retired to `last-job.json` (its work stays on the volume) and the
    // container is started.
    let env = setup();
    env.seed_volume();
    env.seed_container(OTHER_ID, false);
    save(&env.dir.join("job.json"), &job).unwrap();
    assert!(env.recover(Some(IMAGE)).unwrap().running);
    assert!(!env.dir.join("job.json").exists());
    assert!(env.dir.join("last-job.json").exists());
    assert_eq!(env.mutations()[0], format!("start {}", env.config.container));
}

#[test]
#[serial]
fn recovery_refuses_a_busy_session_and_a_hold_at_the_mutation_boundary() {
    // Running with a worker process: not idle, nothing touched.
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    env.with(|e| {
        e.processes
            .get_mut(&env.config.container)
            .unwrap()
            .push("codex exec".into());
    });
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(message(&error).contains("unverified worker"), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());

    // The caller's hold probe says held: typed error, no mutation.
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, false);
    let error = recover(env.workspace.path(), "alice", Some(IMAGE), &|| Err(OperatorHeld.into()))
        .unwrap_err();
    assert!(error.downcast_ref::<OperatorHeld>().is_some(), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());

    // The profile's own hold file is honoured whatever the caller probes:
    // stopped container and missing container alike.
    session_hold::write_hold(&env.config.profile, session_hold::now_unix_ms()).unwrap();
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(error.downcast_ref::<OperatorHeld>().is_some(), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());
    env.with(|e| {
        e.containers.clear();
    });
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(error.downcast_ref::<OperatorHeld>().is_some(), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());
}

#[test]
#[serial]
fn recovery_fails_closed_when_the_started_container_does_not_stay_running() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, false);
    env.with(|e| e.start_dies = true);
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(message(&error).contains("not running"), "{error:#}");
    // One start, no retry loop of its own (the reconciler backs off), and no
    // control exec against a dead container.
    assert_eq!(env.mutations(), vec![format!("start {}", env.config.container)]);
}

#[test]
#[serial]
fn an_operator_stop_holds_the_account_down_until_an_operator_start_lifts_it() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    assert!(!env.held());
    let status = stop(env.workspace.path(), "alice", false).unwrap();
    assert!(!status.running);
    assert!(env.held(), "stop must leave the operator hold in the profile");
    assert!(
        !env.engine().containers.contains_key(&env.config.container),
        "stop removes the container"
    );
    assert!(env.engine().volumes.contains_key(&env.config.volume), "and keeps the volume");
    let after_stop = env.mutations();
    assert_eq!(
        after_stop,
        vec![
            format!("stop {}", env.config.container),
            format!("rm {}", env.config.container)
        ]
    );
    // The reconciler's unattended recovery now refuses, so a stopped private
    // session is not recreated behind the operator's back.
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(error.downcast_ref::<OperatorHeld>().is_some(), "{error:#}");
    assert_eq!(env.mutations(), after_stop);
    // An operator start lifts the hold before its first mutation.
    let status = start(
        env.workspace.path(),
        "alice",
        &env.config.repository,
        &env.config.base,
        Some(IMAGE),
    )
    .unwrap();
    assert!(status.running);
    assert!(!env.held());
    assert!(env.mutations().iter().any(|m| m == &format!("run {IMAGE}")));
    // Recovery is unattended again from here.
    env.with(|e| {
        e.containers.get_mut(&env.config.container).unwrap()["State"]["Running"] = json!(false);
        e.processes.remove(&env.config.container);
    });
    assert!(env.recover(Some(IMAGE)).unwrap().running);

    // A stop with nothing to stop still holds: a session already gone is not
    // recreated either.
    let env = setup();
    env.seed_volume();
    stop(env.workspace.path(), "alice", false).unwrap();
    assert!(env.held());
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(error.downcast_ref::<OperatorHeld>().is_some(), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());
}

#[test]
#[serial]
fn a_wedged_start_surfaces_the_typed_timeout_and_releases_the_lease() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, false);
    env.engine.borrow_mut().start_times_out = true;
    let error = env.recover(Some(IMAGE)).unwrap_err();
    // The reconcile pass keys on this type to end the pass as Docker-unavailable.
    assert!(error.downcast_ref::<docker_cli::DockerTimedOut>().is_some(), "{error:#}");
    // The flock went with the failed recovery; the account is not wedged.
    drop(lease::Lease::acquire(&env.dir).unwrap());
}

#[test]
fn long_execs_keep_a_budget_beyond_the_quick_call_default() {
    assert!(docker::EXEC_TIMEOUT > docker_cli::timeout_for(&["exec"]));
    assert_eq!(docker_cli::timeout_for(&["start", "c"]), docker_cli::DOCKER_CALL_TIMEOUT);
}

/// `session_recover` with the #9409 fork transient retried out (see
/// [`Env::recover`]): a concurrent git fork can keep the previous call's
/// released lease alive microseconds past its drop.
fn recover_session(
    workspace: &Path,
    name: &str,
    action: RecoverAction,
    issue: Option<u64>,
    branches: &[String],
) -> Result<RecoverReport> {
    let mut attempt = 0;
    loop {
        match session_recover(workspace, name, action, issue, branches) {
            Err(error) if attempt < 20 && error.to_string().contains("busy") => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            other => return other,
        }
    }
}

/// What `session recover` (#14030) asks under the job's guard policy: the
/// exclusive lease is held for the whole call, a stopped or busy session is
/// refused before any exec, and every registered clone of the account is
/// addressed with the requested scope.
#[test]
#[serial]
fn session_recover_follows_the_job_guard_policy_and_scopes_every_clone() {
    let env = setup();
    // No session at all: refused before any exec.
    let error =
        recover_session(env.workspace.path(), "alice", RecoverAction::List, None, &[]).unwrap_err();
    assert!(message(&error).contains("stopped"), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());

    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    // A live worker: not idle, refused like a job.
    env.with(|e| {
        e.processes
            .get_mut(&env.config.container)
            .unwrap()
            .push("codex exec".into());
    });
    let error =
        recover_session(env.workspace.path(), "alice", RecoverAction::List, None, &[]).unwrap_err();
    assert!(message(&error).contains("idle"), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());
    // Back to idle: the guard checks pass exactly like a job's.
    env.with(|e| {
        e.processes.get_mut(&env.config.container).unwrap().pop();
    });

    // A second registered repository: the recover walk covers it too.
    let mut config = env.config.clone();
    let second = identify("https://forge.example/a/c").unwrap();
    config.schema_version = 2;
    config.repositories.push(Registered {
        repository: second.url.clone(),
        base: "main".into(),
        slug: second.slug.clone(),
    });
    save(&env.dir.join("workspace.json"), &config).unwrap();
    // The fake reports a completed discard for the scoped call, so the
    // report is a real action rather than a scoped no-op.
    env.with(|e| {
        e.recover_response = Some(
            json!({
                "protocol": PROTOCOL,
                "outcome": {
                    "repository": "https://forge.example/a/b",
                    "cloned": true,
                    "branches": [],
                    "published": [],
                    "discarded": ["feature/issue-7"],
                    "ignored": [],
                    "detached": []
                }
            })
            .to_string(),
        )
    });

    // While the call runs, the exclusive account lease is held: the fake's
    // handler executes between the acquire and the drop.
    let engine = env.engine.clone();
    let dir = env.dir.clone();
    let probe = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let watcher = probe.clone();
    let _wrap = docker::fake::install(Box::new(move |args| {
        if args.contains(&"recover") {
            watcher
                .borrow_mut()
                .push(lease::Lease::acquire(&dir).is_err());
        }
        engine.borrow_mut().handle(args)
    }));
    let report = recover_session(
        env.workspace.path(),
        "alice",
        RecoverAction::Discard,
        Some(7),
        &["feature/issue-7".into()],
    )
    .unwrap();
    assert_eq!(
        probe.borrow().as_slice(),
        &[true, true],
        "the lease is exclusive during the call"
    );
    assert_eq!(report.account, "alice");
    assert_eq!(report.clones.len(), 2);
    // The primary is addressed without a slug, the additional with one, and
    // both carry the requested scope and the repository-scoped env.
    let engine = env.engine();
    let recover_calls: Vec<_> = engine
        .calls
        .iter()
        .filter(|c| c.iter().any(|a| a == "recover"))
        .collect();
    assert_eq!(recover_calls.len(), 2);
    let scoped_flags = |call: &[String]| -> Vec<String> {
        call.windows(2)
            .filter(|w| w[0].starts_with("--") && w[0] != "--env")
            .map(|w| format!("{}={}", w[0], w[1]))
            .collect()
    };
    let first = scoped_flags(recover_calls[0]);
    assert!(first.iter().all(|f| !f.starts_with("--slug")));
    assert_eq!(first.iter().filter(|f| f.starts_with("--branch")).count(), 1);
    let second = scoped_flags(recover_calls[1]);
    assert!(second.iter().any(|f| f.starts_with("--slug")));
    for call in recover_calls.iter() {
        let call: &[String] = call;
        assert!(call.iter().any(|a| a == "--discard"));
        assert!(call
            .windows(2)
            .any(|w| w[0] == "--branch" && w[1] == "feature/issue-7"));
        assert!(call.windows(2).any(|w| w[0] == "--issue" && w[1] == "7"));
        assert!(call.windows(2).any(|w| w[0] == "--base" && w[1] == "main"));
    }
    // The exec ran with the same repository-scoped auth context a job uses:
    // each clone's credential scope is set on its own exec.
    let scope_of = |call: &Vec<String>| -> Option<String> {
        call.windows(2)
            .find(|w| w[0] == "--env" && w[1].starts_with("LOOM_PRIVATE_REPOSITORY="))
            .map(|w| w[1].clone())
    };
    assert_eq!(
        scope_of(recover_calls[0]).as_deref(),
        Some("LOOM_PRIVATE_REPOSITORY=https://forge.example/a/b")
    );
    assert_eq!(
        scope_of(recover_calls[1]).as_deref(),
        Some("LOOM_PRIVATE_REPOSITORY=https://forge.example/a/c")
    );
    // The lease went with the finished call; the account is not wedged.
    drop(lease::Lease::acquire(&env.dir).unwrap());
}

/// #14433: `recover --publish` for a GitHub clone with no write credential
/// (no env token, no published owner App directory, no `gh` login) is refused
/// before any docker exec, naming what to provide and carrying no secret. List
/// and discard push nothing and are not refused; a process token lets the
/// publish through to the worker.
#[test]
#[serial]
fn session_recover_publish_without_a_github_credential_is_refused_before_exec() {
    use crate::env_test_support::EnvGuard;
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    // The primary (a non-GitHub forge) is fine; the registered GitHub clone is
    // what lacks a credential, and it must stop the walk before the primary's
    // exec too.
    let mut config = env.config.clone();
    let github = identify("https://github.com/acme/widget").unwrap();
    config.schema_version = 2;
    config.repositories.push(Registered {
        repository: github.url.clone(),
        base: "main".into(),
        slug: github.slug.clone(),
    });
    save(&env.dir.join("workspace.json"), &config).unwrap();
    let reported = json!({
        "protocol": PROTOCOL,
        "outcome": {
            "repository": "https://github.com/acme/widget",
            "cloned": true,
            "branches": [],
            "published": ["feature/issue-7"],
            "discarded": [],
            "ignored": [],
            "detached": []
        }
    })
    .to_string();
    env.with(|e| e.recover_response = Some(reported.clone()));
    let recover_calls = |env: &Env| {
        env.engine()
            .calls
            .iter()
            .filter(|c| c.iter().any(|a| a == "recover"))
            .count()
    };

    let guard = EnvGuard::set("GH_HOST", "github.com")
        .and_set("GH_TOKEN", None)
        .and_set("GITHUB_TOKEN", None)
        .and_set("LOOM_MACHINE_CHECKOUT", None);
    let error =
        recover_session(env.workspace.path(), "alice", RecoverAction::Publish, Some(7), &[])
            .unwrap_err();
    let text = message(&error);
    assert!(text.contains("no GitHub write credential"), "{text}");
    assert!(text.contains("GH_TOKEN") && text.contains("gh-config-by-owner"), "{text}");
    assert_eq!(recover_calls(&env), 0, "refused before any exec");

    // Listing pushes nothing: it still reaches the worker (both clones).
    recover_session(env.workspace.path(), "alice", RecoverAction::List, None, &[]).unwrap();
    assert_eq!(recover_calls(&env), 2);

    // A process token is a credential: the publish is forwarded (bare name).
    drop(guard);
    let _token = EnvGuard::set("GH_TOKEN", "ghs_TESTONLY_14433")
        .and_set("GH_HOST", Some("github.com".as_ref()));
    let report =
        recover_session(env.workspace.path(), "alice", RecoverAction::Publish, Some(7), &[])
            .unwrap();
    assert_eq!(report.clones.len(), 2);
    assert_eq!(report.clones[1].published, ["feature/issue-7"]);
    let engine = env.engine();
    let call = engine
        .calls
        .iter()
        .rfind(|c| c.iter().any(|a| a == "recover"))
        .unwrap();
    assert!(call
        .windows(2)
        .any(|w| w[0] == "--env" && w[1] == "GH_TOKEN"));
    assert!(!call.iter().any(|a| a.contains("ghs_TESTONLY_14433")));
}

/// #14469: when the only published credential for a GitHub clone's owner is
/// provably expired, the publish preflight refuses with the expired message
/// (#14435) — not with advice to publish an App config that is already
/// published — and still before any docker exec.
#[test]
#[serial]
fn session_recover_publish_with_only_an_expired_credential_names_the_expiry() {
    use crate::env_test_support::EnvGuard;
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    let mut config = env.config.clone();
    let github = identify("https://github.com/acme/widget").unwrap();
    config.schema_version = 2;
    config.repositories.push(Registered {
        repository: github.url.clone(),
        base: "main".into(),
        slug: github.slug.clone(),
    });
    save(&env.dir.join("workspace.json"), &config).unwrap();
    // The workspace fallback holds the owner's published App config, but its
    // token sidecar expired in 2020 and nothing fresher exists.
    let dir = crate::credential_preflight::github_app_gh_config_dir_for_owner(
        env.workspace.path(),
        "acme",
    );
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hosts.yml"), "github.com: {}\n").unwrap();
    std::fs::write(
        dir.join("identity.json"),
        r#"{"appId":"1","slug":null,"installationId":"2","owner":"acme","role":"writer","expiresAt":"2020-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    let guard = EnvGuard::set("GH_HOST", "github.com")
        .and_set("GH_TOKEN", None)
        .and_set("GITHUB_TOKEN", None)
        .and_set("LOOM_MACHINE_CHECKOUT", None);
    let error =
        recover_session(env.workspace.path(), "alice", RecoverAction::Publish, Some(7), &[])
            .unwrap_err();
    let text = message(&error);
    assert!(text.contains("expired and no fresher one is published"), "{text}");
    assert!(text.contains("credential refresh has not renewed it"), "{text}");
    assert!(!text.contains("provide the repository owner's published"), "{text}");
    assert_eq!(
        env.engine()
            .calls
            .iter()
            .filter(|c| c.iter().any(|a| a == "recover"))
            .count(),
        0,
        "refused before any exec"
    );
    drop(guard);
}

#[test]
#[serial]
fn session_recover_surfaces_worker_refusals_and_scoped_noops() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    // A worker-side refusal surfaces with its message (#14030).
    env.with(|e| {
        e.recover_response = Some(
            json!({"protocol": PROTOCOL, "error": "discard requires an explicit branch scope"})
                .to_string(),
        )
    });
    let error = recover_session(
        env.workspace.path(),
        "alice",
        RecoverAction::Discard,
        Some(7),
        &["feature/issue-7".into()],
    )
    .unwrap_err();
    assert!(message(&error).contains("explicit branch scope"), "{error:#}");

    // A clone that was never materialized reports cloned=false and is
    // skipped; a scoped action that acted nowhere is refused, not silent.
    env.with(|e| {
        e.recover_response = Some(
            json!({
                "protocol": PROTOCOL,
                "outcome": {
                    "repository": "https://forge.example/a/b",
                    "cloned": false,
                    "branches": [],
                    "published": [],
                    "discarded": [],
                    "ignored": ["feature/issue-7"],
                    "detached": []
                }
            })
            .to_string(),
        )
    });
    let error = recover_session(
        env.workspace.path(),
        "alice",
        RecoverAction::Discard,
        Some(7),
        &["feature/issue-7".into()],
    )
    .unwrap_err();
    assert!(message(&error).contains("no clone"), "{error:#}");
    // The listing form never refuses on a no-op.
    let report =
        recover_session(env.workspace.path(), "alice", RecoverAction::List, None, &[]).unwrap();
    assert!(!report.clones[0].cloned);
    // An unscoped publish with nothing to publish succeeds quietly.
    let report =
        recover_session(env.workspace.path(), "alice", RecoverAction::Publish, None, &[]).unwrap();
    assert!(report.clones[0].published.is_empty());
}

// ---------------------------------------------------------------------------
// #14111: converging a declared binding without an operator.
// ---------------------------------------------------------------------------

const NEW_REPO: &str = "https://forge.example/a/c";

fn converge_one(env: &Env, repository: &str, base: &str) -> Result<Converged> {
    converge(env.workspace.path(), "alice", repository, base, &|| Ok(()))
}

fn registered(env: &Env) -> Vec<String> {
    load(&env.dir)
        .unwrap()
        .repositories
        .into_iter()
        .map(|r| r.repository)
        .collect()
}

/// Calls that would remove, stop or recreate anything.
fn destructive(env: &Env) -> Vec<String> {
    env.mutations()
        .into_iter()
        .filter(|m| {
            ["stop ", "rm ", "run ", "volume "]
                .iter()
                .any(|prefix| m.starts_with(prefix))
        })
        .collect()
}

#[test]
#[serial]
fn converge_registers_a_missing_repository_on_an_idle_session_and_keeps_everything() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    assert_eq!(converge_one(&env, NEW_REPO, "main").unwrap(), Converged::Registered);
    assert_eq!(registered(&env), [NEW_REPO]);
    let config = load(&env.dir).unwrap();
    assert_eq!(config.repository, "https://forge.example/a/b", "the primary is unchanged");
    assert_eq!(destructive(&env), Vec::<String>::new(), "{:?}", env.mutations());
    assert!(env.engine().containers[&env.config.container]["Id"] == CONTAINER_ID);
    assert!(env.engine().volumes.contains_key(&env.config.volume));

    // Converged: the repeat reads no engine state at all.
    let calls = env.engine().calls.len();
    assert_eq!(converge_one(&env, NEW_REPO, "main").unwrap(), Converged::Present);
    assert_eq!(
        converge_one(&env, "https://forge.example/a/b.git", "main").unwrap(),
        Converged::Present
    );
    assert_eq!(env.engine().calls.len(), calls, "a present binding costs no docker call");
}

#[test]
#[serial]
fn converge_defers_a_busy_account_and_never_interrupts_it() {
    // A worker the lease does not name.
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    env.with(|e| {
        e.processes
            .get_mut(&env.config.container)
            .unwrap()
            .push("codex exec".into());
    });
    let error = converge_one(&env, NEW_REPO, "main").unwrap_err();
    assert!(error.downcast_ref::<AccountBusy>().is_some(), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());
    assert!(registered(&env).is_empty());

    // A job holding the exclusive lease.
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    let held = lease::Lease::acquire(&env.dir).unwrap();
    let error = converge_one(&env, NEW_REPO, "main").unwrap_err();
    assert!(error.downcast_ref::<AccountBusy>().is_some(), "{error:#}");
    assert!(env.engine().calls.is_empty(), "refused before any docker call");
    drop(held);
}

#[test]
#[serial]
fn converge_refuses_a_hold_and_never_lifts_it() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    // The caller's probe, and the profile's own hold file.
    let probed =
        converge(env.workspace.path(), "alice", NEW_REPO, "main", &|| Err(OperatorHeld.into()));
    assert!(probed.unwrap_err().downcast_ref::<OperatorHeld>().is_some());
    session_hold::write_hold(&env.config.profile, session_hold::now_unix_ms()).unwrap();
    let error = converge_one(&env, NEW_REPO, "main").unwrap_err();
    assert!(error.downcast_ref::<OperatorHeld>().is_some(), "{error:#}");
    assert!(env.held(), "an unattended binding never lifts an operator hold");
    assert_eq!(env.mutations(), Vec::<String>::new());
    assert!(registered(&env).is_empty());
}

#[test]
#[serial]
fn converge_fails_closed_before_docker_on_bad_input() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    for (repository, base, what) in [
        ("http://forge.example/a/c", "main", "HTTPS"),
        ("ssh://git@forge.example/a/c", "main", "HTTPS"),
        ("https://user:token@forge.example/a/c", "main", "HTTPS"),
        (NEW_REPO, "--upload-pack=x", "base"),
    ] {
        let error = converge_one(&env, repository, base).unwrap_err();
        assert!(message(&error).contains(what), "{repository}: {error:#}");
        assert!(!message(&error).contains("token"), "no credential is echoed: {error:#}");
    }
    let unknown =
        converge(env.workspace.path(), "nobody", NEW_REPO, "main", &|| Ok(())).unwrap_err();
    assert!(message(&unknown).contains("account not found"), "{unknown:#}");
    // The registered repository with a different base is a conflict, not a change.
    let error = converge_one(&env, "https://forge.example/a/b", "develop").unwrap_err();
    assert!(message(&error).contains("already configured with base main"), "{error:#}");
    assert!(env.engine().calls.is_empty(), "every refusal precedes the first docker call");
    assert!(registered(&env).is_empty());
}

#[test]
#[serial]
fn converge_does_not_convert_a_host_mounted_session_or_recreate_a_missing_container() {
    // A host-mounted session: session-managed, no private workspace.
    let env = setup();
    std::fs::remove_file(env.dir.join("workspace.json")).unwrap();
    crate::tokens_pool::session_lifecycle::mark_session_managed(
        &env.config.profile,
        &env.config.container,
    )
    .unwrap();
    let error = converge_one(&env, NEW_REPO, "main").unwrap_err();
    assert!(message(&error).contains("host-mounted session"), "{error:#}");
    assert!(env.engine().calls.is_empty());
    assert!(!env.dir.join("workspace.json").exists());

    // A configured account whose container is gone: recovery restores it with
    // the recorded image; a binding must not create one with a default.
    let env = setup();
    env.seed_volume();
    let error = converge_one(&env, NEW_REPO, "main").unwrap_err();
    assert!(message(&error).contains("refusing to recreate"), "{error:#}");
    assert_eq!(env.mutations(), Vec::<String>::new());
    assert!(registered(&env).is_empty(), "nothing was written");
}

#[test]
#[serial]
fn registered_repositories_lists_the_primary_then_the_additional_ones() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    converge_one(&env, NEW_REPO, "main").unwrap();
    let got = registered_repositories(env.workspace.path(), "alice")
        .unwrap()
        .unwrap();
    let urls: Vec<&str> = got.iter().map(|(url, _)| url.as_str()).collect();
    assert_eq!(urls, ["https://forge.example/a/b", NEW_REPO]);
    assert_eq!(got[1].1, "forge.example/a/c");
    // check_binding yields the same key for any spelling.
    assert_eq!(check_binding("https://Forge.example/a/c.git", "main").unwrap(), got[1].1);
}

const OLD_URL: &str = "https://forge.example/old/c";

fn rebind_one(env: &Env, from: &str, to: &str, dry_run: bool) -> Result<RebindReport> {
    // The #9409 fork transient: retry only a pre-mutation busy refusal.
    let mut attempt = 0;
    loop {
        match rebind(env.workspace.path(), "alice", from, to, dry_run) {
            Err(error) if attempt < 20 && error.to_string().contains("busy") => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            other => return other,
        }
    }
}

fn seed_rebind(env: &Env, status: &str, populated: bool) {
    let state = json!({"path": "p", "populated": populated, "branches": [{"branch": "feature/issue-9", "ahead": 1}], "dirty": true, "detached": 0});
    let empty =
        json!({"path": "p", "populated": false, "branches": [], "dirty": false, "detached": 0});
    let body = json!({"protocol": PROTOCOL, "outcome": {"status": status, "from": if status == "moved" { state.clone() } else { empty }, "to": state}});
    env.with(|e| e.rebind_response = Some(body.to_string()));
}

fn register_old(env: &Env, also_new: bool) {
    let mut config = env.config.clone();
    config.schema_version = 2;
    for url in std::iter::once(OLD_URL).chain(also_new.then_some(NEW_REPO)) {
        let id = identify(url).unwrap();
        config.repositories.push(Registered {
            repository: id.url,
            base: "main".into(),
            slug: id.slug,
        });
    }
    save(&env.dir.join("workspace.json"), &config).unwrap();
}

fn rebind_calls(env: &Env) -> usize {
    env.engine()
        .calls
        .iter()
        .filter(|c| c.iter().any(|a| a == "rebind"))
        .count()
}

#[test]
#[serial]
fn rebind_replaces_the_old_registration_after_the_volume_step_and_is_repeatable() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    register_old(&env, false);
    seed_rebind(&env, "moved", true);

    // A dry run asks the worker to verify and changes no registration.
    seed_rebind(&env, "dry_run", true);
    let dry = rebind_one(&env, OLD_URL, NEW_REPO, true).unwrap();
    assert_eq!(dry.status, RebindStatus::DryRun);
    assert_eq!(registered(&env), [OLD_URL]);
    assert!(env
        .engine()
        .calls
        .iter()
        .any(|c| c.iter().any(|a| a == "--dry-run")));

    seed_rebind(&env, "moved", true);
    let report = rebind_one(&env, OLD_URL, NEW_REPO, false).unwrap();
    assert_eq!(report.status, RebindStatus::Moved);
    assert!(report.registration_changed);
    assert_eq!(registered(&env), [NEW_REPO]);
    let new = identify(NEW_REPO).unwrap();
    assert_eq!(load(&env.dir).unwrap().repositories[0].slug, new.slug);
    assert_eq!(report.clone_path, format!("{WORK_ROOT}/{}", new.slug));
    assert_eq!(load(&env.dir).unwrap().repository, "https://forge.example/a/b");
    assert_eq!(destructive(&env), Vec::<String>::new(), "{:?}", env.mutations());
    let call = env
        .engine()
        .calls
        .iter()
        .find(|c| c.iter().any(|a| a == "rebind"))
        .cloned()
        .unwrap();
    assert!(call
        .windows(2)
        .any(|w| w[0] == "--env" && w[1] == format!("LOOM_PRIVATE_REPOSITORY={NEW_REPO}")));

    // Re-running verifies the canonical clone and changes nothing.
    seed_rebind(&env, "already_bound", true);
    let again = rebind_one(&env, OLD_URL, NEW_REPO, false).unwrap();
    assert_eq!(again.status, RebindStatus::AlreadyBound);
    assert!(!again.registration_changed);
    assert_eq!(registered(&env), [NEW_REPO]);
}

#[test]
#[serial]
fn rebind_collapses_a_duplicate_registration_and_reports_two_populated_clones() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    register_old(&env, true);
    // Both populated: reported, nothing changes.
    seed_rebind(&env, "both_populated", true);
    let report = rebind_one(&env, OLD_URL, NEW_REPO, false).unwrap();
    assert_eq!(report.status, RebindStatus::BothPopulated);
    assert!(!report.registration_changed);
    assert_eq!(registered(&env), [OLD_URL, NEW_REPO]);
    // One side unpopulated: the duplicate registration is retired, once.
    seed_rebind(&env, "already_bound", true);
    let report = rebind_one(&env, OLD_URL, NEW_REPO, false).unwrap();
    assert!(report.registration_changed);
    assert_eq!(registered(&env), [NEW_REPO]);
}

#[test]
#[serial]
fn rebind_refuses_busy_held_firewalled_primary_and_unregistered_before_the_volume() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    register_old(&env, false);
    seed_rebind(&env, "moved", true);

    // Busy: a worker no lease names.
    env.with(|e| {
        e.processes
            .get_mut(&env.config.container)
            .unwrap()
            .push("codex exec".into())
    });
    let error = rebind_one(&env, OLD_URL, NEW_REPO, false).unwrap_err();
    assert!(error.downcast_ref::<AccountBusy>().is_some(), "{error:#}");
    env.with(|e| {
        e.processes.get_mut(&env.config.container).unwrap().pop();
    });
    // Busy: a job holds the lease.
    let held = lease::Lease::acquire(&env.dir).unwrap();
    let error = rebind(env.workspace.path(), "alice", OLD_URL, NEW_REPO, false).unwrap_err();
    assert!(error.downcast_ref::<AccountBusy>().is_some(), "{error:#}");
    drop(held);
    // Held: never lifted.
    session_hold::write_hold(&env.config.profile, session_hold::now_unix_ms()).unwrap();
    let error = rebind_one(&env, OLD_URL, NEW_REPO, false).unwrap_err();
    assert!(error.downcast_ref::<OperatorHeld>().is_some(), "{error:#}");
    assert!(env.held());
    assert_eq!(rebind_calls(&env), 0);
    assert_eq!(registered(&env), [OLD_URL]);
    session_hold::lift_holds(std::slice::from_ref(&env.config.profile)).unwrap();

    // Primary, same repository in another spelling, and unregistered.
    for (from, to) in [
        ("https://forge.example/a/b", NEW_REPO),
        (OLD_URL, "https://forge.example/a/b"),
        (OLD_URL, "https://FORGE.example/old/c.git"),
        ("https://forge.example/x/y", NEW_REPO),
    ] {
        assert!(rebind_one(&env, from, to, false).is_err(), "{from} -> {to}");
    }
    assert_eq!(rebind_calls(&env), 0, "refused before any volume exec");
    assert_eq!(registered(&env), [OLD_URL]);

    // Firewalled: the fleet roster marks the canonical repository
    // `firewall: true`, so the move is refused before the volume is touched.
    let _roster = seed_firewall_roster(NEW_REPO);
    let error = rebind_one(&env, OLD_URL, NEW_REPO, false).unwrap_err();
    assert!(message(&error).contains("firewall: true"), "{error:#}");
    assert_eq!(rebind_calls(&env), 0, "refused before any volume exec");
    assert_eq!(registered(&env), [OLD_URL]);
}

/// A cached fleet store whose roster marks `remote` `firewall: true`, reached
/// through a throwaway `$HOME`. The guard restores the environment on drop.
fn seed_firewall_roster(remote: &str) -> (crate::env_test_support::EnvGuard, tempfile::TempDir) {
    use crate::env_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let cache = home.path().join(".loom/fleet-store/acme/fleet");
    std::fs::create_dir_all(cache.join("blobs")).unwrap();
    let fleet = json!({
        "_generated": {"schema_version": 1},
        "root": "/srv/src",
        "repos": [{"name": "notebook", "dir": "notebook", "remote": remote, "firewall": true}],
        "state": {},
        "config": {"defaults": {}},
    });
    std::fs::write(cache.join("blobs/fleet-json"), fleet.to_string()).unwrap();
    let manifest = json!({
        "version": 1,
        "repo": "acme/fleet",
        "reference": "main",
        "commit": "0123456789abcdef0123456789abcdef01234567",
        "etag": null,
        "confirmedAt": "2026-11-18T00:00:00Z",
        "files": {"fleet.json": "fleet-json"},
    });
    std::fs::write(cache.join("manifest.json"), manifest.to_string()).unwrap();
    let guard = EnvGuard::set("HOME", home.path())
        .and_set("LOOM_FLEET_REPO", Some("acme/fleet".as_ref()))
        .and_set("LOOM_FLEET_REF", None);
    (guard, home)
}

#[test]
#[serial]
fn rebind_keeps_the_registration_when_the_worker_refuses_a_wrong_repository() {
    let env = setup();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    register_old(&env, false);
    env.with(|e| {
        e.rebind_response = Some(
            json!({"protocol": PROTOCOL, "error": "wrong repository: shares no history"})
                .to_string(),
        )
    });
    let error = rebind_one(&env, OLD_URL, NEW_REPO, false).unwrap_err();
    assert!(message(&error).contains("wrong repository"), "{error:#}");
    assert_eq!(registered(&env), [OLD_URL]);
    assert_eq!(destructive(&env), Vec::<String>::new());
    drop(lease::Lease::acquire(&env.dir).unwrap());
}

// ---- #14451: accounts.session.image / extraMounts on a private-clone seat ----

const DERIVED: &str =
    "ghcr.io/example/derived@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_DERIVED: &str =
    "ghcr.io/example/derived@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// What the bundle probe prints for an image whose sealed bundle is intact.
fn intact_probe() -> String {
    use crate::tokens_pool::session_image::BUNDLE_PROBE_MARKER;
    let mut assets = BTreeMap::new();
    for relative in bundle::sealed_assets() {
        assets.insert((*relative).to_owned(), format!("digest-of-{relative}"));
    }
    let manifest = bundle::Manifest {
        protocol: bundle::CONTROL_PROTOCOL.into(),
        control_version: bundle::CONTROL_VERSION,
        codex_cli: "codex-cli 0.0.0".into(),
        codex_schema_pin: "0.0.0".into(),
        assets: assets.clone(),
        policy: bundle::expected_policy(),
        registration: bundle::registration(),
    };
    let mut out = serde_json::to_string_pretty(&manifest).unwrap();
    out.push_str(&format!("\n{BUNDLE_PROBE_MARKER}\n"));
    for (path, digest) in assets {
        out.push_str(&format!("{digest}  ./{path}\n"));
    }
    out
}

/// Write `accounts.session` into the workspace's own config tier.
fn configure(env: &Env, image: Option<&str>, mounts: &[(&Path, &str)]) {
    let mounts: Vec<Value> = mounts
        .iter()
        .map(|(host, container)| json!({"host": host, "container": container}))
        .collect();
    let mut session = json!({"extraMounts": mounts});
    if let Some(image) = image {
        session["image"] = json!(image);
    }
    let dir = env.workspace.path().join(".loom");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), json!({"accounts": {"session": session}}).to_string())
        .unwrap();
}

fn operator_start(env: &Env, image: Option<&str>) -> Result<Status> {
    start(env.workspace.path(), "alice", &env.config.repository, "main", image)
}

/// The `docker run` (not `--rm`) calls, parsed.
fn created_runs(env: &Env) -> Vec<Run> {
    env.engine()
        .calls
        .iter()
        .filter(|c| c[0] == "run" && !c.contains(&"--rm".to_owned()))
        .map(|c| {
            let args: Vec<&str> = c.iter().map(String::as_str).collect();
            parse_run(&args[1..])
        })
        .collect()
}

fn probes(env: &Env) -> usize {
    env.engine()
        .calls
        .iter()
        .filter(|c| c[0] == "run" && c.contains(&"/bin/sh".to_owned()))
        .count()
}

fn recorded(env: &Env) -> Config {
    load(&env.dir).unwrap()
}

#[test]
#[serial]
fn a_private_start_creates_from_the_configured_image_and_read_only_mounts() {
    let env = setup();
    env.seed_volume();
    let pdk = tempfile::tempdir().unwrap();
    configure(&env, Some(DERIVED), &[(pdk.path(), "/opt/pdk")]);
    env.with(|e| e.probe_output = Some(intact_probe()));
    let status = operator_start(&env, None).unwrap();

    let runs = created_runs(&env);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].image, DERIVED);
    let pdk_mount = runs[0]
        .mounts
        .iter()
        .find(|m| m["Destination"] == "/opt/pdk")
        .expect("the extra mount is created");
    assert_eq!(pdk_mount["RW"], false);
    assert_eq!(pdk_mount["Source"], json!(pdk.path()));
    assert!(runs[0].labels.contains_key("loom.extra-mounts"));
    // The probe ran before anything was created from the image.
    assert_eq!(probes(&env), 1);
    let calls = env.engine().calls.clone();
    let probe_at = calls
        .iter()
        .position(|c| c.contains(&"/bin/sh".to_owned()))
        .unwrap();
    let create_at = calls
        .iter()
        .position(|c| c[0] == "run" && !c.contains(&"--rm".to_owned()))
        .unwrap();
    assert!(probe_at < create_at);
    // Recorded, validated and reported.
    let saved = recorded(&env);
    assert_eq!(saved.session_image.as_deref(), Some(DERIVED));
    assert_eq!(saved.extra_mounts.len(), 1);
    assert_eq!(status.image.as_deref(), Some(DERIVED));
    assert_eq!(status.extra_mounts, saved.extra_mounts);
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["image"], DERIVED);
    assert_eq!(json["extra_mounts"][0]["container"], "/opt/pdk");
}

#[test]
#[serial]
fn an_explicit_image_beats_the_configured_one_and_is_not_probed_as_it() {
    let env = setup();
    env.seed_volume();
    configure(&env, Some(DERIVED), &[]);
    operator_start(&env, Some(IMAGE)).unwrap();
    assert_eq!(created_runs(&env)[0].image, IMAGE);
    assert_eq!(probes(&env), 0);
    assert_eq!(recorded(&env).session_image, None);
}

#[test]
#[serial]
fn a_failing_bundle_probe_refuses_before_any_container_exists() {
    let env = setup();
    env.seed_volume();
    let pdk = tempfile::tempdir().unwrap();
    configure(&env, Some(DERIVED), &[(pdk.path(), "/opt/pdk")]);
    env.with(|e| e.probe_output = None);
    let error = operator_start(&env, None).unwrap_err();
    assert!(message(&error).contains("override image"), "{error:#}");
    assert!(created_runs(&env).is_empty());
    assert!(env.engine().containers.is_empty());
    assert_eq!(recorded(&env).session_image, None);
    assert!(recorded(&env).extra_mounts.is_empty());
}

#[test]
#[serial]
fn a_recreate_after_stop_and_start_applies_the_new_configuration() {
    let env = setup();
    env.seed_volume();
    let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    configure(&env, Some(DERIVED), &[(first.path(), "/opt/pdk")]);
    env.with(|e| e.probe_output = Some(intact_probe()));
    operator_start(&env, None).unwrap();
    stop(env.workspace.path(), "alice", false).unwrap();
    assert!(env.engine().containers.is_empty());
    configure(&env, Some(OTHER_DERIVED), &[(second.path(), "/opt/models")]);
    operator_start(&env, None).unwrap();
    let runs = created_runs(&env);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].image, OTHER_DERIVED);
    assert!(runs[1]
        .mounts
        .iter()
        .any(|m| m["Destination"] == "/opt/models"));
    assert!(runs[1]
        .mounts
        .iter()
        .all(|m| m["Destination"] != "/opt/pdk"));
    assert_eq!(recorded(&env).session_image.as_deref(), Some(OTHER_DERIVED));
}

#[test]
#[serial]
fn recover_recreates_with_the_recorded_image_and_mounts_not_the_edited_config() {
    let env = setup();
    env.seed_volume();
    let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    configure(&env, Some(DERIVED), &[(first.path(), "/opt/pdk")]);
    env.with(|e| e.probe_output = Some(intact_probe()));
    operator_start(&env, None).unwrap();
    // The container is lost; the config is edited behind the seat's back.
    env.with(|e| {
        e.containers.clear();
        e.processes.clear();
        e.images.push(DERIVED.into());
    });
    configure(&env, Some(OTHER_DERIVED), &[(second.path(), "/opt/models")]);
    // Neither the stale hint nor the edited config decides: the record does.
    env.recover(Some(IMAGE)).unwrap();
    let runs = created_runs(&env);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].image, DERIVED);
    assert!(runs[1]
        .mounts
        .iter()
        .any(|m| m["Destination"] == "/opt/pdk" && m["RW"] == false));
    assert!(runs[1]
        .mounts
        .iter()
        .all(|m| m["Destination"] != "/opt/models"));
    // Re-verified on the way.
    assert_eq!(probes(&env), 2);
}

#[test]
#[serial]
fn recover_refuses_a_not_yet_rebuilt_override_image_and_never_substitutes_one() {
    let env = setup();
    env.seed_volume();
    configure(&env, Some(DERIVED), &[]);
    env.with(|e| e.probe_output = Some(intact_probe()));
    operator_start(&env, None).unwrap();
    // A daemon roll: the image's sealed bundle no longer matches.
    env.with(|e| {
        e.containers.clear();
        e.processes.clear();
        e.images.push(DERIVED.into());
        e.probe_output = Some(intact_probe().replace(
            &format!("\"control_version\": {}", bundle::CONTROL_VERSION),
            &format!("\"control_version\": {}", bundle::CONTROL_VERSION + 1),
        ));
    });
    let before = created_runs(&env).len();
    let error = env.recover(Some(IMAGE)).unwrap_err();
    assert!(message(&error).contains("override image"), "{error:#}");
    assert_eq!(created_runs(&env).len(), before, "nothing was created, stock or otherwise");
    assert!(env.engine().containers.is_empty());
}

#[test]
#[serial]
fn private_extra_mounts_pass_the_denylist_before_docker() {
    let ok = tempfile::tempdir().unwrap();
    let home = dirs::home_dir().expect("a home directory");
    let cases: Vec<(&str, PathBuf, &str)> = vec![
        ("host /", PathBuf::from("/"), "/opt/x"),
        ("host home", home.clone(), "/opt/x"),
        ("target is /", ok.path().to_path_buf(), "/"),
        ("target is /workspace", ok.path().to_path_buf(), "/workspace"),
        ("target in /workspace", ok.path().to_path_buf(), "/workspace/data"),
        ("target is the profile", ok.path().to_path_buf(), "/home/loom/.codex-profile"),
        (
            "target nests in the profile",
            ok.path().to_path_buf(),
            "/home/loom/.codex-profile/pdk",
        ),
        ("target is the bundle root", ok.path().to_path_buf(), bundle::CONTROL_ROOT),
        ("target is the forge config", ok.path().to_path_buf(), "/run/loom-gh"),
    ];
    for (what, host, target) in cases {
        let env = setup();
        env.seed_volume();
        configure(&env, None, &[(&host, target)]);
        let error = operator_start(&env, None).expect_err(what);
        assert!(message(&error).contains("refusing"), "{what}: {error:#}");
        assert!(created_runs(&env).is_empty(), "{what}: nothing reached docker run");
        assert!(recorded(&env).extra_mounts.is_empty(), "{what}: nothing was recorded");
    }
    // Two overlapping extras are refused too.
    let env = setup();
    env.seed_volume();
    configure(&env, None, &[(ok.path(), "/opt/pdk"), (ok.path(), "/opt/pdk/sub")]);
    let error = operator_start(&env, None).unwrap_err();
    assert!(message(&error).contains("overlapping"), "{error:#}");
    assert!(created_runs(&env).is_empty());
}

#[test]
#[serial]
fn validation_counts_recorded_extra_mounts_and_still_fails_closed() {
    let env = setup();
    let pdk = tempfile::tempdir().unwrap();
    let mut config = env.config.clone();
    config.extra_mounts = vec![crate::tokens_pool::session_image::ExtraMount {
        host: pdk.path().to_path_buf(),
        container: PathBuf::from("/opt/pdk"),
    }];
    let good = session_container(&config, CONTAINER_ID, true, IMAGE);
    docker::validate_settings(&config, &good).unwrap();
    // A recorded mount the container lacks.
    let mut missing = good.clone();
    missing["Mounts"].as_array_mut().unwrap().pop();
    assert!(docker::validate_settings(&config, &missing).is_err());
    // A mount nobody recorded.
    let unrecorded = session_container(&config, CONTAINER_ID, true, IMAGE);
    assert!(docker::validate_settings(&env.config, &unrecorded).is_err());
    // A recorded mount that is writable, or from another source.
    let mut writable = good.clone();
    writable["Mounts"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap()["RW"] = json!(true);
    assert!(docker::validate_settings(&config, &writable).is_err());
    let mut moved = good;
    moved["Mounts"].as_array_mut().unwrap().last_mut().unwrap()["Source"] = json!("/etc");
    assert!(docker::validate_settings(&config, &moved).is_err());
}

#[test]
#[serial]
fn without_the_configuration_a_private_start_is_unchanged() {
    let env = setup();
    env.seed_volume();
    let status = operator_start(&env, None).unwrap();
    let runs = created_runs(&env);
    assert_eq!(runs[0].image, crate::tokens_pool::session_lifecycle::DEFAULT_SESSION_IMAGE);
    assert_eq!(runs[0].mounts.len(), 2 + bundle::PROFILE_CONTROLS.len());
    assert!(!runs[0].labels.contains_key("loom.extra-mounts"));
    assert_eq!(probes(&env), 0);
    assert!(status.extra_mounts.is_empty());
    let text = std::fs::read_to_string(env.dir.join("workspace.json")).unwrap();
    assert!(!text.contains("extra_mounts") && !text.contains("session_image"), "{text}");
}

/// Give a disposable workspace the same Codex provider manifest as a launch.
fn install_codex_dispatch_manifest(root: &Path) {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/runtimes/codex.json");
    let dest = root.join(".loom/runtimes/codex.json");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::copy(shipped, dest).unwrap();
}

#[test]
#[serial]
fn idle_refusal_uses_the_draw_snapshot_when_inventory_changes() {
    use crate::tokens_pool::health::{self, TerminalClassification};
    use crate::tokens_pool::session_lifecycle::liveness::test_support::set_selector_docker;

    let _environment = crate::env_test_support::EnvGuard::clear(&[
        "CODEX_HOME",
        "LOOM_CODEX_HOME",
        "LOOM_CODEX_PROFILE",
        "LOOM_CODEX_NO_EXEC",
        "LOOM_SPAWN_NO_EXPORT",
    ]);
    let _probe = crate::env_test_support::EnvGuard::set("LOOM_CODEX_SESSION_PROBE", "0");
    let env = setup();
    let root = env.workspace.path();
    install_codex_dispatch_manifest(root);
    crate::test_git_helpers::run(root, &["init", "-q"]);
    crate::test_git_helpers::run(root, &["remote", "add", "origin", &env.config.repository]);
    super::super::session_lifecycle::mark_session_managed(
        &env.config.profile,
        &env.config.container,
    )
    .unwrap();
    health::record_terminal(root, &env.account.id, TerminalClassification::TokenExpired, "fixture")
        .unwrap();

    let registry = crate::tokens_pool::paths::per_repo_accounts_file(root);
    let profile = env.dir.join("workspace.json");
    let mut unrelated = env.config.clone();
    unrelated.repository = "https://forge.example/unrelated/repo".into();
    let changed_profile = serde_json::to_vec(&unrelated).unwrap();
    for (file, changed) in [
        (registry.as_path(), b"invalid fixture registry".as_slice()),
        (profile.as_path(), changed_profile.as_slice()),
    ] {
        let original = std::fs::read(file).unwrap();
        let replacement = root.join("replacement-data");
        std::fs::write(&replacement, changed).unwrap();
        let fake_docker = root.join("selection-snapshot");
        // Liveness is observed only after the selector collected its
        // inventory and evaluated its repository predicate.
        crate::exec_test_support::write_executable(
            &fake_docker,
            format!(
                "#!/bin/sh\n[ \"$1\" = ps ] || exit 88\ncp '{}' '{}'\n",
                replacement.display(),
                file.display(),
            ),
        );
        let _liveness = set_selector_docker(fake_docker.to_str().unwrap(), false);

        let result =
            dispatch::Selection::prepare_idle(root, None, JobKind::Sweep, None, "fixture", false);
        assert_eq!(std::fs::read(file).unwrap(), changed, "snapshot must have run");
        let error = result.err().expect("the matching account is not healthy");
        assert!(error.is::<health::NoHealthyAccountError>(), "{error:#}");
        assert_eq!(error.to_string(), dispatch::NO_IDLE_SEAT);
        std::fs::write(file, original).unwrap();
    }
    assert!(env.engine().calls.is_empty(), "health filtering made no target calls");
}

#[test]
#[serial]
fn legacy_sweep_prepares_one_target_and_hands_its_selection_to_launch() {
    use crate::sweep_registry::{
        private_dispatch, DispatchModel, SweepRegistry, SweepRegistryConfig,
    };
    use crate::types::SweepKind;

    let _environment = crate::env_test_support::EnvGuard::clear(&[
        "CODEX_HOME",
        "LOOM_CODEX_HOME",
        "LOOM_CODEX_PROFILE",
        "LOOM_CODEX_NO_EXEC",
        "LOOM_RUNTIME",
        "LOOM_RUNTIME_BUILDER",
        "LOOM_RUNTIME_SWEEP_LIFECYCLE",
        "LOOM_SPAWN_NO_EXPORT",
    ]);
    let _probe = crate::env_test_support::EnvGuard::set("LOOM_CODEX_SESSION_PROBE", "0");
    let _defaults = crate::env_test_support::EnvGuard::set("LOOM_CONFIG_DEFAULTS_FILE", "");
    // The write-scope fixture uses a synthetic GitHub origin and gh; do
    // not give private preparation a matching forge credential host.
    let _forge_host = crate::env_test_support::EnvGuard::set("GH_HOST", "fixture.invalid");
    let mut env = setup();
    let root = env.workspace.path();
    let scope = crate::write_scope_test_support::WritableRoot::register(root);
    env.config.repository = format!("https://github.com/{}.git", scope.repo);
    save(&env.dir.join("workspace.json"), &env.config).unwrap();
    super::super::session_lifecycle::mark_session_managed(
        &env.config.profile,
        &env.config.container,
    )
    .unwrap();
    env.seed_container(CONTAINER_ID, true);
    env.seed_volume();
    install_codex_dispatch_manifest(root);
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults");
    std::fs::copy(shipped.join("runtimes/claude.json"), root.join(".loom/runtimes/claude.json"))
        .unwrap();
    let tokens = tempfile::tempdir().unwrap();
    std::fs::write(tokens.path().join("fixture.token"), "sk-ant-oat-fixture\n").unwrap();
    let _tokens = crate::env_test_support::EnvGuard::set(
        crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV,
        tokens.path(),
    );
    let mut config = SweepRegistryConfig::new(root.to_owned());
    config.gh_bin = Some(scope.gh.clone());
    config.journal_path = Some(root.join("test-sweeps-journal.json"));
    let registry = SweepRegistry::shared(config);
    let roles = root.join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    for runtime in ["codex", "claude"] {
        crate::exec_test_support::write_executable(
            &root.join(format!(".loom/scripts/spawn-{runtime}.sh")),
            "#!/bin/sh\nexit 0\n",
        );
    }
    let controls = tempfile::tempdir().unwrap();
    let trusted = super::containment_tests::profile(controls.path(), Some(&[]), &["fresh"]);
    for name in bundle::PROFILE_CONTROLS {
        std::fs::copy(trusted.join(name), env.config.profile.join(name)).unwrap();
    }

    for (issue, runtime, capabilities, contained, preference_walk) in [
        (17_u32, "codex", &[][..], false, true),
        (18, "codex", &["worktreeIsolation"][..], true, true),
        (19, "claude", &[][..], false, true),
        (20, "codex", &[][..], false, false),
    ] {
        let runtimes = if preference_walk {
            json!({"preference": [runtime]})
        } else {
            json!({"default": runtime})
        };
        save(&root.join(".loom/config.json"), &json!({"runtimes": runtimes})).unwrap();
        save(&roles.join("builder.json"), &json!({"runtimeRequirements": capabilities})).unwrap();
        env.with(|engine| engine.calls.clear());
        let launch = private_dispatch::prepare(
            &registry,
            &SweepKind::Issue(issue),
            None,
            DispatchModel::Request(Some("fixture-model")),
        )
        .unwrap()
        .unwrap();
        let admitted = launch.admission.admitted.as_ref().unwrap();
        assert_eq!(admitted.runtime, runtime);
        assert_eq!(admitted.preference.is_some(), preference_walk);
        if let Some(preference) = &admitted.preference {
            assert_eq!(preference.tap, runtime);
            assert_eq!(preference.tier, 0);
        }
        assert_eq!(admitted.execution.is_some(), contained);
        assert_eq!(launch.model.as_deref(), Some("fixture-model"));
        let prepare_count = env
            .engine()
            .calls
            .iter()
            .filter(|args| {
                args.first().is_some_and(|arg| arg == "exec")
                    && args
                        .windows(2)
                        .any(|pair| pair[0] == "private-workspace" && pair[1] == "prepare")
            })
            .count();
        if runtime == "codex" {
            assert_eq!(prepare_count, 1, "legacy sweep #{issue} prepares its target once");
            let selection = launch
                .selection
                .as_ref()
                .expect("the launch owns the prepared seat");
            assert_eq!(selection.name, "alice");
            let lease = selection
                .lease()
                .expect("the original lease reaches launch");
            assert_eq!(lease.dir, env.dir);
            let job = lease::read(&lease.dir).unwrap().unwrap();
            assert_eq!(job.container_id, CONTAINER_ID);
            assert_eq!(job.issue, Some(u64::from(issue)));
            assert!(matches!(job.kind, JobKind::Sweep));
            assert_eq!(job.base_revision, "a".repeat(40));
            assert!(lease::Lease::acquire(&env.dir)
                .err()
                .unwrap()
                .is::<AccountBusy>());
        } else {
            assert_eq!(prepare_count, 0, "non-Codex fallback performs no target work");
            assert!(launch.selection.is_none());
        }
    }
}

#[test]
#[serial]
fn idle_reservation_retries_a_busy_weekly_favorite_and_hands_off_the_next_lease() {
    use crate::env_test_support::EnvGuard;
    use crate::tokens_pool::{paths::SHARED_ACCOUNTS_ROOT_ENV, session_lifecycle};
    use std::ffi::OsStr;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let _fork = super::tests::fork_guard();
    let home = tempfile::tempdir().unwrap();
    let _runtime_env = EnvGuard::clear(&[
        "CODEX_HOME",
        "LOOM_CODEX_HOME",
        "LOOM_CODEX_PROFILE",
        "LOOM_CODEX_NO_EXEC",
        "LOOM_CODEX_SESSION_EXEC",
        "LOOM_PRIVATE_LEASE_FD",
        "LOOM_SPAWN_NO_EXPORT",
        "GH_TOKEN",
        "GITHUB_TOKEN",
    ])
    .and_set("HOME", Some(home.path().as_os_str()))
    .and_set("GH_HOST", Some(OsStr::new("fixture.invalid")))
    .and_set(SHARED_ACCOUNTS_ROOT_ENV, Some(OsStr::new("")))
    .and_set("LOOM_CODEX_DEFAULT_HOME", Some(OsStr::new("")))
    .and_set("LOOM_CODEX_SESSION_PROBE", Some(OsStr::new("0")));
    let mut env = setup();
    let root = env.workspace.path().to_path_buf();
    let alice = env.config.clone();
    let alice_dir = env.dir.clone();
    env.seed_volume();
    env.seed_container(CONTAINER_ID, true);
    session_lifecycle::mark_session_managed(&alice.profile, &alice.container).unwrap();

    let service = AccountLifecycle::new(&root, ProcessCodexRunner).unwrap();
    service
        .import_with_email("bob", &alice.profile.join("auth.json"), None)
        .unwrap();
    env.account = account_inventory(&root, AccountProvider::Codex)
        .unwrap()
        .into_iter()
        .find(|a| a.id.name == "bob")
        .unwrap();
    env.config.account = "bob".into();
    env.config.container = container_name("bob");
    env.config.volume = "loom-codex-workspace-bob".into();
    env.config.profile = env.account.credential_reference.canonicalize().unwrap();
    env.dir = state_dir(&env.config.profile).unwrap();
    std::fs::create_dir_all(&env.dir).unwrap();
    save(&env.dir.join("workspace.json"), &env.config).unwrap();
    session_lifecycle::mark_session_managed(&env.config.profile, &env.config.container).unwrap();
    for name in bundle::PROFILE_CONTROLS {
        std::fs::copy(alice.profile.join(name), env.config.profile.join(name)).unwrap();
    }
    env.seed_volume();
    env.seed_container(OTHER_ID, true);

    for (profile, weekly) in [(&alice.profile, 5), (&env.config.profile, 30)] {
        let sessions = profile.join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let line = json!({"rate_limits": {"secondary": {
            "used_percent": weekly, "window_minutes": 10_080, "resets_in_seconds": 400_000,
        }}});
        std::fs::write(sessions.join("rollout-fixture.jsonl"), format!("{line}\n")).unwrap();
    }
    crate::test_git_helpers::run(&root, &["init", "-q"]);
    crate::test_git_helpers::run(&root, &["remote", "add", "origin", &alice.repository]);
    let runtimes = root.join(".loom/runtimes");
    std::fs::create_dir_all(&runtimes).unwrap();
    save(&runtimes.join("codex.json"), &json!({"accountProvider": "codex"})).unwrap();

    let reserve = |issue, owner| {
        dispatch::Selection::prepare_idle(&root, None, JobKind::Sweep, Some(issue), owner, false)
            .unwrap()
            .unwrap()
    };
    // Advance the cursor past Alice with a real locked draw. The next draw
    // would still favor Alice on usage, independent of the cursor.
    let preferred = reserve(6, "unheld-control");
    assert_eq!(preferred.name, "alice");
    drop(preferred);
    let busy = super::tests::reacquire(&alice_dir);

    env.with(|e| e.profile = env.config.profile.clone());
    let next = reserve(7, "busy-favorite");
    assert_eq!(next.name, "bob");
    assert_eq!(next.lease().unwrap().dir, env.dir);
    let job = lease::read(&env.dir).unwrap().unwrap();
    assert_eq!(job.owner, "busy-favorite");
    assert_eq!(job.issue, Some(7));
    assert_eq!(job.container_id, OTHER_ID);
    assert_eq!(job.base_revision, "a".repeat(40));

    let mut command = Command::new("/bin/sh");
    command
        .env_clear()
        .arg("-c")
        .arg(concat!(
            "test -e \"/dev/fd/${LOOM_PRIVATE_LEASE_FD:?}\" || exit 3; ",
            "printf '%s\\n' \"$LOOM_ACCOUNT_NAME\"; read -r release || :",
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    next.apply(&mut command);
    let mut child = command.spawn().unwrap();
    next.spawned();
    drop(next);
    let mut name = String::new();
    let read = BufReader::new(child.stdout.take().unwrap()).read_line(&mut name);
    let held_by_child = lease::Lease::acquire(&env.dir)
        .err()
        .is_some_and(|error| error.is::<lease::AccountBusy>());
    drop(child.stdin.take());
    let exit = child.wait().unwrap();
    assert!(read.is_ok() && exit.success());
    assert_eq!(name.trim_end(), "bob");
    assert!(held_by_child, "the child must retain Bob's lease");
    drop(super::tests::reacquire(&env.dir));
    drop(busy);
}

#[test]
#[serial]
fn concurrent_judge_draws_share_one_idle_seat() {
    use crate::runtime_preference::{
        resolve_runtime_contained, role_draw, Decision, DispatchContext, Intent,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Barrier;

    let _runtime_env = crate::env_test_support::EnvGuard::clear(&[
        "CODEX_HOME",
        "LOOM_CODEX_HOME",
        "LOOM_CODEX_PROFILE",
        "LOOM_CODEX_NO_EXEC",
        "LOOM_CODEX_SESSION_EXEC",
        "LOOM_RUNTIME",
        "LOOM_RUNTIME_JUDGE",
        "LOOM_SPAWN_NO_EXPORT",
    ]);
    let _probe = crate::env_test_support::EnvGuard::set("LOOM_CODEX_SESSION_PROBE", "0");
    let env = setup();
    super::super::session_lifecycle::mark_session_managed(
        &env.config.profile,
        &env.config.container,
    )
    .unwrap();
    env.seed_container(CONTAINER_ID, true);
    env.seed_volume();
    let root = env.workspace.path();
    crate::test_git_helpers::run(root, &["init", "-q"]);
    crate::test_git_helpers::run(root, &["remote", "add", "origin", &env.config.repository]);
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("defaults");
    for file in [
        "roles/judge.json",
        "runtimes/codex.json",
        "runtimes/claude.json",
    ] {
        let dest = root.join(".loom").join(file);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(shipped.join(file), dest).unwrap();
    }
    for runtime in ["codex", "claude"] {
        let dest = root.join(format!(".loom/scripts/spawn-{runtime}.sh"));
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let tokens = tempfile::tempdir().unwrap();
    std::fs::write(tokens.path().join("fixture.token"), "sk-ant-oat-fixture\n").unwrap();
    let _tokens_env = crate::env_test_support::EnvGuard::set(
        crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV,
        tokens.path(),
    );

    save(
        &root.join(".loom/config.json"),
        &json!({"runtimes":{"roleDraw":{"judge":{"1":{"codex":1000,"claude":1}}}}}),
    )
    .unwrap();

    let draw = |owner: String| {
        let mut preparer =
            containment::Preparer::new(root, JobKind::Role, None, owner, Box::new(|_, _| None));
        let result = role_draw::with_seed(1, || {
            resolve_runtime_contained(
                root,
                "judge",
                None,
                1_800_000_000,
                DispatchContext {
                    intent: Intent::Dispatch,
                    role_tick: true,
                },
                &mut preparer,
            )
        });
        let Decision::Preference {
            resolution, draw, ..
        } = result.unwrap()
        else {
            panic!("expected the role draw");
        };
        // The launch consumes the held reservation as `private_dispatch`
        // does for a host-admitted Codex candidate (#14512): it is handed
        // over whole, so nothing is re-prepared.
        let held = crate::sweep_registry::private_dispatch::take_held(&mut preparer, false)
            .unwrap()
            .map(|(selection, _)| selection);
        (resolution.chosen.unwrap().admitted.runtime, draw.unwrap(), held)
    };
    let assert_falls_through_with = |case: &str, reason: &str| {
        let (runtime, record, selection) = draw(case.to_owned());
        assert_eq!(runtime, "claude", "{case}: {record:?}");
        assert!(selection.is_none(), "{case}");
        assert!(
            record
                .passed_over
                .iter()
                .any(|passed| passed == &format!("codex: not-admitted({reason})")),
            "{case}: {record:?}"
        );
    };
    let assert_falls_through = |case: &str| assert_falls_through_with(case, "no idle seat");

    assert_falls_through("hook trust unproven");
    let controls = tempfile::tempdir().unwrap();
    let trusted = super::containment_tests::profile(controls.path(), Some(&[]), &["fresh"]);
    for name in bundle::PROFILE_CONTROLS {
        std::fs::copy(trusted.join(name), env.config.profile.join(name)).unwrap();
    }

    crate::test_git_helpers::run(
        root,
        &[
            "remote",
            "set-url",
            "origin",
            "https://forge.example/other/repo",
        ],
    );
    // No inventoried account serves the resolved repository: a routing miss,
    // reported apart from contention (#14512).
    assert_falls_through_with("repository unregistered", "no seat serves this repository");
    crate::test_git_helpers::run(root, &["remote", "set-url", "origin", &env.config.repository]);

    env.seed_container(CONTAINER_ID, false);
    assert_falls_through("container stopped");
    env.seed_container(CONTAINER_ID, true);

    let barrier = Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let handles = (0..2)
            .map(|index| {
                let mut engine = env.engine().clone();
                let barrier = &barrier;
                let draw = &draw;
                scope.spawn(move || {
                    let _docker = docker::fake::install(Box::new(move |args| engine.handle(args)));
                    barrier.wait();
                    let (runtime, draw, selection) = draw(format!("judge-fixture-{index}"));
                    // Keep the winning reservation open until both draws finish.
                    barrier.wait();
                    (runtime, draw, selection.is_some())
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });

    assert_eq!(
        results
            .iter()
            .filter(|(runtime, _, _)| runtime == "codex")
            .count(),
        1,
        "{results:?}"
    );
    for (runtime, draw, reserved) in &results {
        match runtime.as_str() {
            "codex" => assert!(reserved),
            "claude" => {
                assert!(!reserved);
                assert!(
                    draw.passed_over
                        .iter()
                        .any(|reason| reason == "codex: not-admitted(no idle seat)"),
                    "{draw:?}"
                );
            }
            other => panic!("unexpected runtime {other}"),
        }
    }
    // With no fallback and the same sole seat busy, both step entrypoints
    // return their captain grant before taking a local lock or calling the forge.
    use crate::fleet_dispatch::{proto, test_server::TestServer, worker};
    use crate::step_role::StepRole;
    use crate::sweep_registry::step_spawn::{StepRefusal, StepSpawnRequest};
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use std::sync::{Arc, Mutex};

    worker::reset_for_test();
    let captain = TestServer::start(|path, _| match path {
        proto::CLAIM_PATH => (
            200,
            json!({"server_epoch":"offline","granted":true,
                                         "assignment_id":"judge-7","ttl_s":1200}),
        ),
        proto::REFUSE_PATH => (200, json!({"server_epoch":"offline","known":true})),
        _ => panic!("unexpected fleet call: {path}"),
    });
    save(
        &root.join(".loom/config.json"),
        &json!({
            "runtimes": {"rolePreference": {"judge": ["codex"]}},
            "fleetDispatch": {"enabled": true, "url": captain.url()},
        }),
    )
    .unwrap();
    let forbid_forge = root.join("forge-should-not-run");
    let forge_marker = root.join("forge-contacted");
    std::fs::write(
        &forbid_forge,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", forge_marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&forbid_forge, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = SweepRegistryConfig::new(root.to_path_buf());
    config.gh_bin = Some(forbid_forge);
    let command = config.role_command_path(StepRole::Judge);
    std::fs::create_dir_all(command.parent().unwrap()).unwrap();
    std::fs::write(&command, "# Fixture Judge\n").unwrap();
    let locks = config.locks_dir();
    let registry = Arc::new(Mutex::new(SweepRegistry::new(config)));
    let held = lease::Lease::acquire(&env.dir).expect("the abandoned selection is released");
    let request = StepSpawnRequest::new(7, StepRole::Judge);
    let first = registry.lock().unwrap().spawn_step(&request).unwrap_err();
    let second = SweepRegistry::spawn_step_unlocked(&registry, &request).unwrap_err();
    for error in [first, second] {
        let refusal = error.downcast_ref::<StepRefusal>().unwrap();
        assert_eq!(refusal.reason, "runtime_rejected");
        assert!(refusal.detail.contains("no idle seat"), "{refusal}");
        assert_eq!(refusal.assignment_id.as_deref(), Some("judge-7"));
    }
    let refusals = captain.wait_for(proto::REFUSE_PATH, 2);
    assert_eq!(refusals.len(), 2);
    for sent in refusals {
        assert_eq!(sent["assignment_id"], "judge-7");
        assert_eq!(sent["reason"], crate::session_exec::refusal::SEAT_BUSY_REASON);
    }
    assert!(!locks.join("pr-7").exists());
    assert!(!forge_marker.exists());
    drop(held);
    worker::reset_for_test();
}

// ---------------------------------------------------------------------------
// #14578: a Builder draw passes a busy seat over instead of refusing — the
// bounded walk admits any eligible idle seat whose containment can be proven.
// ---------------------------------------------------------------------------

/// A second private-clone seat for the SAME repository as alice's (`setup()`),
/// with its own container, volume, profile control files and registry entry,
/// so a draw can pass a busy seat over (#14578).
struct Seat {
    dir: PathBuf,
}

fn seed_seat(env: &Env, name: &str, id: &str, trusted: bool) -> Seat {
    let workspace = env.workspace.path().to_path_buf();
    let service = AccountLifecycle::new(&workspace, ProcessCodexRunner).unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("auth.json");
    std::fs::write(&source, "fake-secret").unwrap();
    service.import_with_email(name, &source, None).unwrap();
    let account = account_inventory(&workspace, AccountProvider::Codex)
        .unwrap()
        .into_iter()
        .find(|a| a.id.name == name)
        .unwrap();
    let profile = account.credential_reference.canonicalize().unwrap();
    if trusted {
        let controls = tempfile::tempdir().unwrap();
        let trusted_profile =
            super::containment_tests::profile(controls.path(), Some(&[]), &["fresh"]);
        for file in bundle::PROFILE_CONTROLS {
            std::fs::copy(trusted_profile.join(file), profile.join(file)).unwrap();
        }
    } else {
        // A valid managed registration with NO established trust: the proof
        // fails on hook trust, the failure mode the field report shows.
        let controls = tempfile::tempdir().unwrap();
        let untrusted = super::containment_tests::profile(controls.path(), Some(&[]), &[]);
        for file in bundle::PROFILE_CONTROLS {
            std::fs::copy(untrusted.join(file), profile.join(file)).unwrap();
        }
    }
    let config = Config {
        schema_version: 1,
        account: name.into(),
        container: container_name(name),
        engine: ENGINE.into(),
        docker_desktop: false,
        repository: env.config.repository.clone(),
        base: "main".into(),
        repositories: vec![],
        volume: format!("loom-codex-workspace-{name}"),
        profile: profile.clone(),
        gh_config: None,
        session_image: None,
        extra_mounts: Vec::new(),
    };
    let dir = state_dir(&account.credential_reference).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    save(&dir.join("workspace.json"), &config).unwrap();
    // An adopted session: `adapter::check` refuses a private profile without
    // its marker before any seat is drawn.
    crate::tokens_pool::session_lifecycle::mark_session_managed(&profile, &config.container)
        .unwrap();
    env.with(|e| {
        // The control report must be measured against THIS container's own
        // profile, not the fixture's primary one.
        e.profiles.insert(config.container.clone(), profile.clone());
        e.containers
            .insert(config.container.clone(), session_container(&config, id, true, IMAGE));
        e.processes
            .insert(config.container.clone(), idle_processes());
        let mut labels = BTreeMap::new();
        labels.insert("loom.account".to_owned(), name.to_owned());
        labels.insert("loom.repository".to_owned(), config.repository.clone());
        labels.insert("loom.profile".to_owned(), profile.to_string_lossy().into_owned());
        e.volumes
            .insert(config.volume.clone(), volume_json(&config.volume, &labels));
    });
    Seat { dir }
}

/// The `concurrent_judge_draws_share_one_idle_seat` fixture, with a Builder
/// draw table and a second seat whose controls are (`trusted`) or are not
/// (`!trusted`) hook-trusted.
fn builder_draw_env(env: &Env, trusted: bool) -> Seat {
    // The fixture's own seat is an adopted session too.
    crate::tokens_pool::session_lifecycle::mark_session_managed(
        &env.config.profile,
        &env.config.container,
    )
    .unwrap();
    let bob = seed_seat(env, "bob", OTHER_ID, trusted);
    let root = env.workspace.path();
    crate::test_git_helpers::run(root, &["init", "-q"]);
    crate::test_git_helpers::run(root, &["remote", "add", "origin", &env.config.repository]);
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("defaults");
    for file in [
        "roles/builder.json",
        "runtimes/codex.json",
        "runtimes/claude.json",
    ] {
        let dest = root.join(".loom").join(file);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(shipped.join(file), dest).unwrap();
    }
    for runtime in ["codex", "claude"] {
        let dest = root.join(format!(".loom/scripts/spawn-{runtime}.sh"));
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    save(
        &root.join(".loom/config.json"),
        &json!({"runtimes":{"roleDraw":{"builder":{"1":{"codex":1000,"claude":1}}}}}),
    )
    .unwrap();
    bob
}

/// The shared two-seat test fixture: environment guards, the codex credential
/// pool, and the Builder draw over the fixture's Builder table.
fn builder_draw(
    root: &Path,
) -> impl FnMut(
    String,
) -> (
    String,
    std::boxed::Box<crate::runtime_preference::role_draw::RoleDrawRecord>,
    Option<crate::tokens_pool::private_workspace::dispatch::Selection>,
    bool,
) + '_ {
    let draw = |owner: String| {
        let mut preparer =
            containment::Preparer::new(root, JobKind::Role, None, owner, Box::new(|_, _| None));
        let result = crate::runtime_preference::role_draw::with_seed(1, || {
            crate::runtime_preference::resolve_runtime_contained(
                root,
                "builder",
                None,
                1_800_000_000,
                crate::runtime_preference::DispatchContext {
                    intent: crate::runtime_preference::Intent::Dispatch,
                    role_tick: true,
                },
                &mut preparer,
            )
        });
        let crate::runtime_preference::Decision::Preference {
            resolution, draw, ..
        } = result.unwrap()
        else {
            panic!("expected the role draw");
        };
        let seat_unavailable = preparer.seat_unavailable();
        (
            resolution.chosen.unwrap().admitted.runtime,
            draw.unwrap(),
            preparer.take().map(|(selection, _)| selection),
            seat_unavailable,
        )
    };
    move |owner| draw(owner)
}

fn builder_stage_of(doc: &serde_json::Value) -> &serde_json::Value {
    doc["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stage| stage["stage"] == "builder")
        .unwrap()
}

#[test]
#[serial]
fn a_builder_draw_admits_the_idle_seat_while_another_seat_is_busy() {
    use crate::runtime_preference::role_draw;
    let _runtime_env = crate::env_test_support::EnvGuard::clear(&[
        "CODEX_HOME",
        "LOOM_CODEX_HOME",
        "LOOM_CODEX_PROFILE",
        "LOOM_CODEX_NO_EXEC",
        "LOOM_CODEX_SESSION_EXEC",
        "LOOM_RUNTIME",
        "LOOM_RUNTIME_BUILDER",
        "LOOM_SPAWN_NO_EXPORT",
    ]);
    let _probe = crate::env_test_support::EnvGuard::set("LOOM_CODEX_SESSION_PROBE", "0");
    let env = setup();
    let bob = builder_draw_env(&env, true);
    let tokens = tempfile::tempdir().unwrap();
    std::fs::write(tokens.path().join("fixture.token"), "sk-ant-oat-fixture\n").unwrap();
    let _tokens_env = crate::env_test_support::EnvGuard::set(
        crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV,
        tokens.path(),
    );
    let root = env.workspace.path();

    // Alice's seat is exclusively held by another step; Bob's is idle.
    let held = lease::Lease::acquire(&env.dir).unwrap();

    let mut draw = builder_draw(root);
    let (runtime, _, selection, seat_unavailable) = draw("builder-7".into());
    assert_eq!(runtime, "codex");
    // The busy seat never won: the draw admitted the idle one.
    assert_eq!(selection.as_ref().unwrap().name, "bob");
    assert!(!seat_unavailable, "an admitted draw has no unavailable seat");
    // The winning seat stays exclusively leased through the handoff, and the
    // busy seat's lease is untouched by the draw.
    assert!(lease::Lease::acquire(&bob.dir).is_err());
    assert!(lease::Lease::acquire(&env.dir).is_err());
    // The containment provenance records the seat the launch would run on.
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bob.dir.join(lease::ADMISSION)).unwrap()).unwrap();
    assert_eq!(record["role"], "builder");
    assert_eq!(record["runtime"], "codex");
    assert_eq!(record["execution"]["account"], "bob");
    drop(selection);
    drop(super::tests::reacquire(&bob.dir));

    // `roles draw` names the account it measured, and releases the temporary
    // lease before returning (#14578).
    let doc = role_draw::explain::explain_json(root);
    let codex = &builder_stage_of(&doc)["roleDraw"][0]["candidates"][0];
    assert_eq!(codex["candidate"], "codex");
    assert_eq!(codex["admitted"], true);
    assert_eq!(codex["account"], "bob");
    drop(super::tests::reacquire(&bob.dir));
    drop(held);
    drop(super::tests::reacquire(&env.dir));
}

#[test]
#[serial]
fn a_builder_draw_names_every_tried_seat_when_all_are_busy() {
    use crate::runtime_preference::role_draw;
    let _runtime_env = crate::env_test_support::EnvGuard::clear(&[
        "CODEX_HOME",
        "LOOM_CODEX_HOME",
        "LOOM_CODEX_PROFILE",
        "LOOM_CODEX_NO_EXEC",
        "LOOM_CODEX_SESSION_EXEC",
        "LOOM_RUNTIME",
        "LOOM_RUNTIME_BUILDER",
        "LOOM_SPAWN_NO_EXEC",
        "LOOM_SPAWN_NO_EXPORT",
    ]);
    let _probe = crate::env_test_support::EnvGuard::set("LOOM_CODEX_SESSION_PROBE", "0");
    let env = setup();
    let bob = builder_draw_env(&env, true);
    let tokens = tempfile::tempdir().unwrap();
    std::fs::write(tokens.path().join("fixture.token"), "sk-ant-oat-fixture\n").unwrap();
    let _tokens_env = crate::env_test_support::EnvGuard::set(
        crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV,
        tokens.path(),
    );
    let root = env.workspace.path();
    let held_a = lease::Lease::acquire(&env.dir).unwrap();
    let held_b = lease::Lease::acquire(&bob.dir).unwrap();

    // The real dispatch falls through to the configured fallback with the
    // walk's per-seat report.
    let mut draw = builder_draw(root);
    let (runtime, record, selection, seat_unavailable) = draw("builder-8".into());
    assert_eq!(runtime, "claude");
    assert!(selection.is_none());
    assert!(seat_unavailable, "every seat was busy");
    let passed = record
        .passed_over
        .iter()
        .find(|reason| reason.contains("no eligible seat could be prepared"))
        .expect("the codex refusal names the walk");
    assert!(passed.contains("busy"), "{passed}");
    assert!(passed.contains("0 of 2 eligible seats idle"), "{passed}");

    // The explain row reads the same: both seats named, zero idle.
    let doc = role_draw::explain::explain_json(root);
    let attempt = &builder_stage_of(&doc)["roleDraw"][0];
    let codex = &attempt["candidates"][0];
    assert_eq!(codex["candidate"], "codex");
    assert_eq!(codex["admitted"], false);
    let reason = codex["reason"].as_str().unwrap();
    assert!(
        reason.starts_with(
            "not-admitted(worktreeIsolation: verified private-clone containment could not \
             satisfy it: no eligible seat could be prepared (0 of 2 eligible seats idle): "
        ),
        "{reason}"
    );
    assert!(reason.contains("alice busy"), "{reason}");
    assert!(reason.contains("bob busy"), "{reason}");
    assert!(!reason.contains("failed"), "{reason}");
    // Claude was never refused, and the effective table renormalises to it.
    assert_eq!(attempt["candidates"][1]["candidate"], "claude");
    assert_eq!(attempt["candidates"][1]["admitted"], true);
    assert_eq!(attempt["effective"]["candidates"][0]["candidate"], "claude");
    // The probe never kept a seat: both leases are still only the test's.
    drop(reacquire_probe(&env.dir, held_a));
    drop(reacquire_probe(&bob.dir, held_b));
}

/// A held lease this test owns is re-acquirable only by dropping the holder;
/// the bounded retry waits out unrelated forks (#9409) without hiding a leak.
fn reacquire_probe(dir: &Path, holder: lease::Lease) -> lease::Lease {
    drop(holder);
    super::tests::reacquire(dir)
}

#[test]
#[serial]
fn an_explicit_profile_pin_keeps_binding_while_its_seat_is_busy() {
    let _runtime_env = crate::env_test_support::EnvGuard::clear(&[
        "CODEX_HOME",
        "LOOM_CODEX_HOME",
        "LOOM_CODEX_NO_EXEC",
        "LOOM_CODEX_SESSION_EXEC",
        "LOOM_RUNTIME",
        "LOOM_RUNTIME_BUILDER",
        "LOOM_SPAWN_NO_EXPORT",
    ]);
    let _probe = crate::env_test_support::EnvGuard::set("LOOM_CODEX_SESSION_PROBE", "0");
    let _pin = crate::env_test_support::EnvGuard::set("LOOM_CODEX_PROFILE", "alice");
    let env = setup();
    let bob = builder_draw_env(&env, true);
    let tokens = tempfile::tempdir().unwrap();
    std::fs::write(tokens.path().join("fixture.token"), "sk-ant-oat-fixture\n").unwrap();
    let _tokens_env = crate::env_test_support::EnvGuard::set(
        crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV,
        tokens.path(),
    );
    let root = env.workspace.path();
    let held = lease::Lease::acquire(&env.dir).unwrap();

    let mut draw = builder_draw(root);
    let (_, record, selection, _) = draw("builder-9".into());
    assert!(selection.is_none(), "a pinned seat never walks to another");
    let passed = record
        .passed_over
        .iter()
        .find(|reason| reason.contains("busy"))
        .expect("the pin's busy refusal is reported");
    assert!(
        passed.contains(
            "role, sweep, interactive and lifecycle operations share one exclusive lease"
        ),
        "{passed}"
    );
    // The pinned draw never touched the idle seat.
    drop(lease::Lease::acquire(&bob.dir).unwrap());
    drop(held);
    drop(super::tests::reacquire(&env.dir));
}

#[test]
#[serial]
fn an_idle_seat_that_cannot_prove_containment_is_never_counted_as_busy() {
    let _runtime_env = crate::env_test_support::EnvGuard::clear(&[
        "CODEX_HOME",
        "LOOM_CODEX_HOME",
        "LOOM_CODEX_PROFILE",
        "LOOM_CODEX_NO_EXEC",
        "LOOM_CODEX_SESSION_EXEC",
        "LOOM_RUNTIME",
        "LOOM_RUNTIME_BUILDER",
        "LOOM_SPAWN_NO_EXPORT",
    ]);
    let _probe = crate::env_test_support::EnvGuard::set("LOOM_CODEX_SESSION_PROBE", "0");
    let env = setup();
    let bob = builder_draw_env(&env, false);
    let tokens = tempfile::tempdir().unwrap();
    std::fs::write(tokens.path().join("fixture.token"), "sk-ant-oat-fixture\n").unwrap();
    let _tokens_env = crate::env_test_support::EnvGuard::set(
        crate::tokens_pool::paths::SHARED_TOKENS_DIR_ENV,
        tokens.path(),
    );
    let root = env.workspace.path();
    let held = lease::Lease::acquire(&env.dir).unwrap();

    // Bob is idle but untrusted: the walk reports his own containment
    // refusal, never "busy", and counts his seat idle (#14578).
    let mut draw = builder_draw(root);
    let (_, record, selection, seat_unavailable) = draw("builder-10".into());
    assert!(selection.is_none());
    assert!(!seat_unavailable, "a proof failure is not a busy pool");
    let passed = record
        .passed_over
        .iter()
        .find(|reason| reason.contains("no eligible seat could be prepared"))
        .expect("the codex refusal names the walk");
    assert!(
        passed.contains("no eligible seat could be prepared (1 of 2 eligible seats idle): "),
        "{passed}"
    );
    assert!(passed.contains("alice busy"), "{passed}");
    assert!(passed.contains("bob failed: "), "{passed}");
    assert!(!passed.contains("bob busy"), "{passed}");
    drop(super::tests::reacquire(&bob.dir));
    drop(held);
    drop(super::tests::reacquire(&env.dir));
}
