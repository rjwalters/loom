//! #8876 Part A: daemon-assisted issue-state reads, the trust checks around
//! them, and the once-per-run route choice. Fake secrets only; no network.

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use super::client::{
    ask_daemon, check_socket_trust_as, DaemonLink, RoutedLookup, StandaloneRead as LocalSide,
};
use super::daemon::{serve, Backend, GhRead};
use super::{standalone, valid_slug, IssueStateReply, ReadOutcome};
use crate::forge_egress::policy::PolicySources;
use crate::stash_retirement::IssueStateLookup;
use crate::types::{Request, Response};
use crate::workspace_registry::{normalize_path, Workspace, WorkspaceRegistry};

// ---- daemon handler -------------------------------------------------------

struct FakeBackend {
    slug: Option<&'static str>,
    refusal: Option<&'static str>,
    stdout: Option<&'static str>,
    reads: RefCell<Vec<(PathBuf, String, u64)>>,
}

impl FakeBackend {
    fn closed() -> Self {
        Self {
            slug: Some("acme/private"),
            refusal: None,
            stdout: Some("CLOSED\n"),
            reads: RefCell::default(),
        }
    }
}

impl Backend for FakeBackend {
    fn remote_slug(&self, _: &Path) -> Option<String> {
        self.slug.map(str::to_string)
    }
    fn admission_refusal(&self, _: &Path) -> Option<String> {
        self.refusal.map(str::to_string)
    }
    fn read(&self, root: &Path, slug: &str, issue: u64) -> GhRead {
        self.reads
            .borrow_mut()
            .push((root.to_path_buf(), slug.to_string(), issue));
        GhRead {
            stdout: self.stdout.map(|s| s.as_bytes().to_vec()),
            source: "policy",
        }
    }
}

fn registry_with(root: &Path) -> WorkspaceRegistry {
    WorkspaceRegistry {
        workspaces: vec![Workspace {
            root: normalize_path(root),
            ..serde_json::from_value(serde_json::json!({"root": "/x"})).unwrap()
        }],
        ..WorkspaceRegistry::default()
    }
}

fn root_str(dir: &tempfile::TempDir) -> String {
    dir.path().display().to_string()
}

#[test]
fn a_registered_root_reads_closed_through_the_daemon_route() {
    let dir = tempfile::tempdir().unwrap();
    let backend = FakeBackend::closed();
    let reply = serve(&registry_with(dir.path()), &backend, &root_str(&dir), "acme/private", 969);
    assert_eq!(reply.outcome, ReadOutcome::Closed);
    assert_eq!(reply.route, "daemon-gh/policy");
    assert_eq!(backend.reads.borrow().len(), 1);
    assert_eq!(backend.reads.borrow()[0].1, "acme/private");
}

#[test]
fn open_and_garbage_state_are_not_closed() {
    let dir = tempfile::tempdir().unwrap();
    for (stdout, want) in [
        (Some("OPEN"), ReadOutcome::Open),
        (Some("<html>404</html>"), ReadOutcome::Unknown),
        (None, ReadOutcome::Unknown),
    ] {
        let backend = FakeBackend {
            stdout,
            ..FakeBackend::closed()
        };
        let reply = serve(&registry_with(dir.path()), &backend, &root_str(&dir), "acme/private", 1);
        assert_eq!(reply.outcome, want);
        assert_eq!(reply.outcome.is_closed().is_some(), want != ReadOutcome::Unknown);
    }
}

#[test]
fn an_unregistered_root_is_not_managed_and_never_reads() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let backend = FakeBackend::closed();
    let reply = serve(&registry_with(other.path()), &backend, &root_str(&dir), "acme/private", 1);
    assert_eq!(reply.outcome, ReadOutcome::NotManaged);
    assert!(backend.reads.borrow().is_empty());
}

#[test]
fn a_wrong_forge_identity_is_unknown_and_never_reads() {
    let dir = tempfile::tempdir().unwrap();
    for own in [Some("acme/other"), None] {
        let backend = FakeBackend {
            slug: own,
            ..FakeBackend::closed()
        };
        let reply = serve(&registry_with(dir.path()), &backend, &root_str(&dir), "acme/private", 1);
        assert_eq!(reply.outcome, ReadOutcome::Unknown);
        assert_eq!(reply.reason.as_deref(), Some("identity-mismatch"));
        assert!(backend.reads.borrow().is_empty());
    }
}

#[test]
fn a_routing_refusal_is_unknown_and_never_reads() {
    let dir = tempfile::tempdir().unwrap();
    let backend = FakeBackend {
        refusal: Some("toolchain.launcher-not-first"),
        ..FakeBackend::closed()
    };
    let reply = serve(&registry_with(dir.path()), &backend, &root_str(&dir), "acme/private", 1);
    assert_eq!(reply.outcome, ReadOutcome::Unknown);
    assert!(reply.reason.unwrap().starts_with("routing-refused:"));
    assert!(backend.reads.borrow().is_empty());
}

#[test]
fn hostile_inputs_are_rejected_before_any_read() {
    let dir = tempfile::tempdir().unwrap();
    let registry = registry_with(dir.path());
    let backend = FakeBackend::closed();
    for (root, repo, issue) in [
        (root_str(&dir), "acme/private --repo evil/x", 1),
        (root_str(&dir), "evil.example/acme/private", 1),
        (root_str(&dir), "-acme/private", 1),
        (root_str(&dir), "acme/private", 0),
        ("relative/path".to_string(), "acme/private", 1),
    ] {
        let reply = serve(&registry, &backend, &root, repo, issue);
        assert_eq!(reply.outcome, ReadOutcome::Unknown, "{root} {repo} {issue}");
    }
    assert!(backend.reads.borrow().is_empty());
}

#[test]
fn slug_validation_is_strict() {
    assert!(valid_slug("acme/private"));
    assert!(valid_slug("a.b-c_d/e.f"));
    for bad in [
        "", "acme", "a/b/c", "/b", "a/", "a b/c", "a/b;ls", "..", "a/..", "a/-b",
    ] {
        assert!(!valid_slug(bad), "{bad:?}");
    }
}

#[test]
fn the_reply_wire_form_carries_no_credential_field() {
    let reply = IssueStateReply::new(ReadOutcome::Closed, "daemon-gh/policy");
    assert_eq!(
        serde_json::to_string(&Response::IssueState(reply)).unwrap(),
        r#"{"type":"IssueState","payload":{"outcome":"closed","route":"daemon-gh/policy"}}"#
    );
    let frame = serde_json::to_string(&Request::ReadIssueState {
        workspace_root: "/r".into(),
        repo: "a/b".into(),
        issue: 3,
    })
    .unwrap();
    assert!(matches!(
        serde_json::from_str::<Request>(&frame).unwrap(),
        Request::ReadIssueState { issue: 3, .. }
    ));
}

// ---- socket trust ----------------------------------------------------------

fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").unwrap().uid()
}

fn locked_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

#[test]
fn an_owner_only_socket_outside_the_checkout_is_trusted() {
    let home = locked_dir();
    let checkout = locked_dir();
    let sock = home.path().join("loom-daemon.sock");
    let _l = UnixListener::bind(&sock).unwrap();
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(check_socket_trust_as(&sock, checkout.path(), uid()), Ok(()));
}

#[test]
fn a_socket_under_the_checkout_is_refused() {
    let checkout = locked_dir();
    let sock = checkout.path().join("planted.sock");
    let _l = UnixListener::bind(&sock).unwrap();
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        check_socket_trust_as(&sock, checkout.path(), uid()),
        Err("socket-inside-checkout")
    );
}

#[test]
fn wrong_owner_loose_modes_and_non_sockets_are_refused() {
    let home = locked_dir();
    let checkout = locked_dir();
    let sock = home.path().join("s.sock");
    let _l = UnixListener::bind(&sock).unwrap();
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        check_socket_trust_as(&sock, checkout.path(), uid() + 1),
        Err("socket-not-owner-only")
    );
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(
        check_socket_trust_as(&sock, checkout.path(), uid()),
        Err("socket-not-owner-only")
    );
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        check_socket_trust_as(&sock, checkout.path(), uid()),
        Err("socket-dir-not-owner-only")
    );
    let file = home.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    assert_eq!(check_socket_trust_as(&file, checkout.path(), uid()), Err("not-a-socket"));
    assert_eq!(
        check_socket_trust_as(&home.path().join("none"), checkout.path(), uid()),
        Err("socket-absent")
    );
}

// ---- client round trip against a fake daemon -------------------------------

fn fake_daemon(reply: Response) -> (tempfile::TempDir, PathBuf, std::thread::JoinHandle<String>) {
    use std::io::{BufRead, BufReader, Write};
    let dir = locked_dir();
    let sock = dir.path().join("d.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let mut w = &stream;
        writeln!(w, "{}", serde_json::to_string(&reply).unwrap()).unwrap();
        line
    });
    (dir, sock, handle)
}

#[test]
fn the_client_sends_only_the_bounded_read_frame() {
    let (_d, sock, server) = fake_daemon(Response::IssueState(IssueStateReply::new(
        ReadOutcome::Closed,
        "daemon-gh/policy",
    )));
    let reply =
        ask_daemon(&sock, Path::new("/r"), "acme/private", 9, std::time::Duration::from_secs(5))
            .unwrap();
    assert_eq!(reply.outcome, ReadOutcome::Closed);
    let sent: serde_json::Value = serde_json::from_str(&server.join().unwrap()).unwrap();
    assert_eq!(
        sent,
        serde_json::json!({"type":"ReadIssueState","payload":
            {"workspace_root":"/r","repo":"acme/private","issue":9}})
    );
}

#[test]
fn an_old_daemon_that_rejects_the_frame_is_a_transport_error() {
    let (_d, sock, server) = fake_daemon(Response::Pong);
    assert!(
        ask_daemon(&sock, Path::new("/r"), "a/b", 1, std::time::Duration::from_secs(5)).is_err()
    );
    server.join().unwrap();
}

// ---- route choice ----------------------------------------------------------

struct Link(RefCell<Vec<Result<IssueStateReply, String>>>, RefCell<u32>);

impl Link {
    fn new(answers: Vec<Result<IssueStateReply, String>>) -> Self {
        Self(RefCell::new(answers), RefCell::new(0))
    }
}

impl DaemonLink for Link {
    fn ask(&self, _: u64) -> Result<IssueStateReply, String> {
        *self.1.borrow_mut() += 1;
        self.0.borrow_mut().remove(0)
    }
}

struct Local(Option<bool>, RefCell<u32>);

impl LocalSide for Local {
    fn read(&self, _: u64) -> standalone::StandaloneRead {
        *self.1.borrow_mut() += 1;
        standalone::StandaloneRead {
            outcome: match self.0 {
                Some(true) => ReadOutcome::Closed,
                Some(false) => ReadOutcome::Open,
                None => ReadOutcome::Unknown,
            },
            route: "standalone-gh/path".to_string(),
        }
    }
}

fn reply(outcome: ReadOutcome) -> Result<IssueStateReply, String> {
    Ok(IssueStateReply::new(outcome, "daemon-gh/policy"))
}

#[test]
fn a_daemon_answer_is_final_and_ambient_auth_is_never_tried() {
    // Ambient would say CLOSED; the daemon's unknown must stay unknown.
    let link = Link::new(vec![reply(ReadOutcome::Unknown), reply(ReadOutcome::Closed)]);
    let local = Local(Some(true), RefCell::default());
    let mut lookup = RoutedLookup::new(Some(&link), &local);
    assert_eq!(lookup.is_closed(1), None);
    assert_eq!(lookup.is_closed(2), Some(true));
    assert_eq!(*local.1.borrow(), 0);
    assert_eq!(lookup.route_description(), "daemon-gh/policy");
}

#[test]
fn a_down_daemon_selects_standalone_once_for_the_whole_run() {
    let link = Link::new(vec![Err("connect failed: NotFound".into())]);
    let local = Local(Some(true), RefCell::default());
    let mut lookup = RoutedLookup::new(Some(&link), &local);
    assert_eq!(lookup.is_closed(1), Some(true));
    assert_eq!(lookup.is_closed(2), Some(true));
    assert_eq!(*link.1.borrow(), 1, "the daemon was tried once, not per issue");
    assert!(lookup
        .route_description()
        .starts_with("standalone-gh/path (daemon not used"));
}

#[test]
fn a_daemon_that_does_not_manage_the_root_falls_back_to_standalone() {
    let link = Link::new(vec![reply(ReadOutcome::NotManaged)]);
    let local = Local(Some(false), RefCell::default());
    let mut lookup = RoutedLookup::new(Some(&link), &local);
    assert_eq!(lookup.is_closed(1), Some(false));
    assert_eq!(lookup.is_closed(1), Some(false), "memoized");
    assert_eq!(*local.1.borrow(), 1);
}

#[test]
fn no_link_is_standalone_and_a_failing_standalone_is_unknown() {
    let local = Local(None, RefCell::default());
    let mut lookup = RoutedLookup::new(None, &local);
    assert_eq!(lookup.is_closed(1), None);
}

#[test]
fn a_transport_failure_after_the_daemon_was_chosen_is_unknown_not_a_switch() {
    let link = Link::new(vec![
        reply(ReadOutcome::Closed),
        Err("read failed: TimedOut".into()),
    ]);
    let local = Local(Some(true), RefCell::default());
    let mut lookup = RoutedLookup::new(Some(&link), &local);
    assert_eq!(lookup.is_closed(1), Some(true));
    assert_eq!(lookup.is_closed(2), None);
    assert_eq!(*local.1.borrow(), 0);
}

// ---- standalone gate -------------------------------------------------------

#[test]
fn a_refusing_policy_makes_the_standalone_read_unknown_without_running_gh() {
    let dir = tempfile::tempdir().unwrap();
    let sources = PolicySources {
        env_path: Some(dir.path().join("missing.json")),
        ..PolicySources::default()
    };
    let read = standalone::read(dir.path(), &sources, 7);
    assert_eq!(read.outcome, ReadOutcome::Unknown);
    assert!(read.route.starts_with("standalone-refused:"), "{}", read.route);
}

#[test]
fn an_unconfigured_host_still_reads_through_the_ambient_facade() {
    let dir = tempfile::tempdir().unwrap();
    let read = standalone::read(dir.path(), &PolicySources::default(), 7);
    // The unit-test build routes `gh` to a loud-failing stub: unknown, but the
    // route proves the ungoverned path was taken (and no refusal happened).
    assert_eq!(read.outcome, ReadOutcome::Unknown);
    assert!(read.route.starts_with("standalone-gh/"), "{}", read.route);
}
