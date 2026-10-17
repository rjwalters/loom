//! #12584: every fleet store read runs on a reader App, never the writer
//! (#12394), except on a host with no reader pool at all.
//!
//! A stub `gh` logs the `GH_CONFIG_DIR` and path of each call and answers by
//! the credential directory's name: `limited` is rate-limited, `blind` is not
//! installed on the repository, any other reader serves it; a claim ref
//! (`git/ref/…`) does not exist.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;

struct Stub {
    tmp: tempfile::TempDir,
    log: PathBuf,
    gh: PathBuf,
}

fn stub() -> Stub {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("calls.log");
    let gh = tmp.path().join("gh-stub");
    let script = format!(
        r#"#!/bin/sh
path=""; for a in "$@"; do path="$a"; done
dir="${{GH_CONFIG_DIR:-writer}}"
echo "$(basename "$dir") $path" >> '{log}'
case "$(basename "$dir"):$path" in
  limited:*)
    printf 'HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 0\r\nX-Ratelimit-Resource: core\r\n\r\n{{"message":"API rate limit exceeded"}}'
    echo 'gh: API rate limit exceeded (HTTP 403)' >&2; exit 1;;
  blind:*)
    printf 'HTTP/2.0 404 Not Found\r\n\r\n{{"message":"Not Found"}}'
    echo 'gh: Not Found (HTTP 404)' >&2; exit 1;;
  *:*/git/ref/*)
    printf 'HTTP/2.0 404 Not Found\r\n\r\n{{"message":"Not Found"}}'
    echo 'gh: Not Found (HTTP 404)' >&2; exit 1;;
  *)
    printf 'HTTP/2.0 200 OK\r\nEtag: "e1"\r\n\r\n{{"ok":true}}';;
esac
"#,
        log = log.display()
    );
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    Stub { tmp, log, gh }
}

impl Stub {
    /// `(credential dir name, path)` per call.
    fn calls(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let (d, p) = l.split_once(' ').unwrap();
                (d.to_string(), p.to_string())
            })
            .collect()
    }

    fn reader_dir(&self, name: &str) -> PathBuf {
        let dir = self.tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A transport for `repo` whose pool hands out `readers` in order, one
    /// per lookup, and records every withdrawal.
    fn transport(
        &self,
        repo: &str,
        pooled: bool,
        readers: &[&str],
    ) -> (GhTransport, Rc<RefCell<Vec<String>>>) {
        let queue: Vec<(PathBuf, String)> = readers
            .iter()
            .map(|n| (self.reader_dir(n), format!("app-{n}-{repo}")))
            .collect();
        let queue = RefCell::new(queue.into_iter());
        let withdrawn = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&withdrawn);
        let mut t = GhTransport::with_test_readers(
            self.tmp.path(),
            repo,
            pooled,
            Box::new(move || queue.borrow_mut().next()),
            Box::new(move |app: &str, _: &str, _: Failure, _: &str| {
                seen.borrow_mut().push(app.to_string());
            }),
        );
        t.gh_bin = self.gh.display().to_string();
        (t, withdrawn)
    }
}

fn never_the_writer(calls: &[(String, String)]) {
    assert!(
        calls.iter().all(|(dir, _)| dir != "writer"),
        "a pooled host never reads on the writer (#12394): {calls:?}"
    );
}

#[test]
fn a_store_read_runs_on_the_reader() {
    let s = stub();
    let (t, withdrawn) = s.transport("o/store-a", true, &["r1"]);
    let reply = t.get("repos/o/store-a/commits/main", None, None).unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(s.calls(), vec![("r1".into(), "repos/o/store-a/commits/main".into())]);
    assert!(withdrawn.borrow().is_empty());
    assert_eq!(t.last_credential().unwrap().label(), "reader app app-r1-o/store-a");
}

#[test]
fn a_rate_limited_reader_hands_the_read_to_the_next_reader_not_the_writer() {
    let s = stub();
    let (t, withdrawn) = s.transport("o/store-b", true, &["limited", "r2"]);
    let reply = t.get("repos/o/store-b/commits/main", None, None).unwrap();
    assert_eq!(reply.status, 200);
    let calls = s.calls();
    never_the_writer(&calls);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(*withdrawn.borrow(), vec!["app-limited-o/store-b".to_string()]);
    // The next read starts on the reader that served.
    t.get("repos/o/store-b/commits/main", None, None).unwrap();
    assert_eq!(s.calls().last().unwrap().0, "r2");
}

#[test]
fn a_store_read_no_reader_can_serve_is_shed_not_sent_to_the_writer() {
    let s = stub();
    let (t, _) = s.transport("o/store-c", true, &["limited"]);
    let err = t
        .get("repos/o/store-c/commits/main", None, None)
        .unwrap_err();
    assert!(err.to_string().contains("shed"), "{err}");
    never_the_writer(&s.calls());
    // A pool with no usable reader at all sends nothing.
    let s = stub();
    let (t, _) = s.transport("o/store-c2", true, &[]);
    assert!(t.get("repos/o/store-c2/commits/main", None, None).is_err());
    assert!(s.calls().is_empty());
}

#[test]
fn a_missing_claim_ref_is_the_answer_from_a_reader_that_sees_the_repo() {
    let s = stub();
    let (t, withdrawn) = s.transport("o/store-d", true, &["r1"]);
    let reply = t
        .get_fresh("repos/o/store-d/git/ref/loom/resync-claim")
        .unwrap();
    assert_eq!(reply.status, 404, "the claim does not exist");
    let calls = s.calls();
    never_the_writer(&calls);
    assert_eq!(
        calls.iter().map(|(_, p)| p.as_str()).collect::<Vec<_>>(),
        vec![
            "repos/o/store-d/git/ref/loom/resync-claim",
            "repos/o/store-d"
        ],
        "one coverage check, no other credential"
    );
    assert!(withdrawn.borrow().is_empty());
    // With evidence the reader sees the repository, no check is needed.
    t.get("repos/o/store-d/commits/main", None, None).unwrap();
    t.get_fresh("repos/o/store-d/git/ref/loom/resync-claim")
        .unwrap();
    assert_eq!(s.calls().len(), 4);
}

#[test]
fn a_404_from_a_reader_blind_to_the_repo_is_never_read_as_absent() {
    let s = stub();
    let (t, withdrawn) = s.transport("o/store-e", true, &["blind"]);
    let err = t
        .get_fresh("repos/o/store-e/git/ref/loom/resync-claim")
        .unwrap_err();
    assert!(err.to_string().contains("shed"), "{err}");
    never_the_writer(&s.calls());
    assert_eq!(*withdrawn.borrow(), vec!["app-blind-o/store-e".to_string()]);
}

#[test]
fn a_host_with_no_reader_pool_reads_on_its_only_credential() {
    let s = stub();
    let (t, _) = s.transport("o/store-f", false, &["r1"]);
    let reply = t.get("repos/o/store-f/commits/main", None, None).unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(s.calls(), vec![("writer".into(), "repos/o/store-f/commits/main".into())]);
}

#[test]
fn a_store_read_on_an_explicit_writer_credential_is_still_reader_only() {
    let tmp = tempfile::tempdir().unwrap();
    let t = GhTransport::with_test_readers(
        tmp.path(),
        "o/store-g",
        true,
        Box::new(|| None),
        Box::new(|_: &str, _: &str, _: Failure, _: &str| {}),
    );
    let cred = Credential::ConfigDir {
        dir: tmp.path().to_path_buf(),
        label: "writer app".into(),
    };
    // The bare builder: a read on an explicit credential is still
    // reader-only, so the facade would route it, never run it on the writer.
    let inv = t
        .invocation("fleet_store", ops::GIT_READ_OBJECTS, AccessIntent::Read, &cred)
        .args(["api", "--include", "repos/o/store-g/commits/main"]);
    assert!(inv.is_reader_only());
}

/// #12630: the workspace resync claims (and the head check) build their
/// transport with a managed checkout's root, but reader tokens are
/// published under the daemon's workspace root only. Resolving the pool at
/// the checkout found every reader "stale" (no token directory there) and
/// shed each claim read before any reader was asked.
#[test]
fn a_managed_checkout_resolves_its_readers_at_the_daemon_workspace_root() {
    fn roster(_: &Path) -> crate::forge_identity::Roster {
        crate::forge_identity::Roster {
            writer: None,
            readers: vec![crate::forge_identity::Identity {
                app_id: "951263".into(),
                slug: None,
                private_key_path: PathBuf::from("/keys/951263.pem"),
                owners: None,
            }],
            legacy_logins: vec![],
            writers: Vec::new(),
        }
    }
    let daemon = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let dir = crate::forge_read_pool::gh_config_dir_for_owner_app(daemon.path(), "acme", "951263");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hosts.yml"), "github.com:\n").unwrap();
    crate::forge_identity::sidecar::write_sidecar(
        &dir,
        &crate::forge_identity::Sidecar {
            app_id: "951263".into(),
            installation_id: "1".into(),
            owner: Some("acme".into()),
            role: crate::forge_identity::SidecarRole::Reader,
            expires_at: (chrono::Utc::now() + chrono::Duration::minutes(50)).to_rfc3339(),
            ..Default::default()
        },
    )
    .unwrap();

    // The pre-fix lookup, at the checkout's own root: no reader.
    let (pooled, next) = reader_pool(checkout.path().to_path_buf(), "acme/widgets", roster);
    assert!(pooled);
    assert_eq!(next(), None, "no reader token is ever published under a managed checkout");

    // At the daemon's workspace root, as `GhTransport::new` now resolves it.
    let root = crate::forge_identity::reader_root_with(checkout.path(), Some(daemon.path()));
    assert_eq!(root, daemon.path());
    let (pooled, next) = reader_pool(root, "acme/widgets", roster);
    assert!(pooled);
    assert_eq!(next(), Some((dir, "951263".to_string())));

    // A process with no daemon or front root keeps the root it was given.
    assert_eq!(crate::forge_identity::reader_root_with(checkout.path(), None), checkout.path());
}
