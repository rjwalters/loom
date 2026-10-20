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
#[derive(Default)]
struct Engine {
    id: String,
    containers: BTreeMap<String, Value>,
    /// The `args` column of `docker top` per container.
    processes: BTreeMap<String, Vec<String>>,
    volumes: BTreeMap<String, Value>,
    images: Vec<String>,
    /// The account profile a control report is cross-checked against.
    profile: PathBuf,
    /// `docker start` leaves the container stopped (its process exits at once).
    start_dies: bool,
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
                let report = bundle::Report {
                    protocol: bundle::CONTROL_PROTOCOL.into(),
                    control_version: bundle::CONTROL_VERSION,
                    status: bundle::Status::Ready,
                    codex_cli: "codex 0.0.0".into(),
                    identity: "ab".repeat(32),
                    profile: bundle::profile_digests(&self.profile),
                    managed: true,
                };
                Ok(serde_json::to_string(&report)?)
            }
            ["exec", rest @ ..] if rest.contains(&"prepare") => {
                Ok(json!({"protocol": PROTOCOL, "revision": "deadbeef"}).to_string())
            }
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
    let error = env.recover(Some(IMAGE)).unwrap_err();
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
    let error = env.recover(Some(IMAGE)).unwrap_err();
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
