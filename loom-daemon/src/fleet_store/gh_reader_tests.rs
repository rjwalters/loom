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
            load_failed: false,
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

    // Through the constructor `GhTransport::new` delegates to: a transport
    // for the managed checkout starts on the reader published under the
    // daemon's workspace root.
    let t = GhTransport::new_with_reader_root(
        checkout.path(),
        Some(daemon.path()),
        "acme/widgets",
        roster,
    );
    assert!(t.pooled);
    assert_eq!(
        *t.reader.borrow(),
        Some((dir, "951263".to_string())),
        "the reader pool resolves at the daemon's workspace root, not the checkout's"
    );
    // The writer credential still resolves at the checkout's root.
    assert_eq!(t.workspace_root, checkout.path());

    // The checkout's own root has no reader token: resolving there (the
    // pre-#12630 behaviour) finds none.
    let (pooled, next) = reader_pool(checkout.path().to_path_buf(), "acme/widgets", roster);
    assert!(pooled);
    assert_eq!(next(), None, "no reader token is ever published under a managed checkout");

    // A process with no daemon or front root keeps the root it was given.
    assert_eq!(crate::forge_identity::reader_root_with(checkout.path(), None), checkout.path());
}

// ---- #14730: the bootstrap mint on a never-started host --------------------

/// The reader App a never-started host is configured with (`owners: None`).
const BOOT_APP: &str = "147001";
/// The store repository the bootstrap reads.
const BOOT_REPO: &str = "o/fleet-14730";
/// Commit A (40×`1`): the provisioned snapshot, host `boot-h` stopped.
const COMMIT_A: &str = "1111111111111111111111111111111111111111";
/// Commit B (40×`2`): the store moves `boot-h` to running.
const COMMIT_B: &str = "2222222222222222222222222222222222222222";

fn boot_roster(_: &Path) -> crate::forge_identity::Roster {
    crate::forge_identity::Roster {
        writer: None,
        readers: vec![crate::forge_identity::Identity {
            app_id: BOOT_APP.into(),
            slug: None,
            private_key_path: PathBuf::from("/keys/147001.pem"),
            owners: None,
        }],
        legacy_logins: vec![],
        writers: Vec::new(),
        load_failed: false,
    }
}

/// A compiled `fleet.json` recording `boot-h` in `state`.
fn boot_fleet_json(state: &str) -> String {
    format!(
        r#"{{"_generated":{{"schema_version":1}},"root":"/srv","repos":[],"config":{{"defaults":{{}}}},"state":{{"fleet":{{"state":"running"}},"hosts":{{"boot-h":{{"state":"{state}"}}}}}}}}"#
    )
}

/// A never-started host's workspace: a configured reader, no published reader
/// token, a stub `gh` serving a two-commit store, and a fake
/// `github-app-token.sh` whose mint fails while `mint_fail` exists.
struct Boot {
    tmp: tempfile::TempDir,
    log: PathBuf,
    mint_log: PathBuf,
    mint_fail: PathBuf,
    rate_limited: PathBuf,
    commit: PathBuf,
    gh: PathBuf,
}

fn b64(bytes: &str) -> String {
    use base64::{engine::general_purpose, Engine as _};
    general_purpose::STANDARD.encode(bytes)
}

impl Boot {
    fn new() -> Boot {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let mint_log = tmp.path().join("mints.log");
        let mint_fail = tmp.path().join("mint.fail");
        let rate_limited = tmp.path().join("rate-limited");
        let commit = tmp.path().join("commit");
        let blob_a = "a".repeat(40);
        let blob_b = "b".repeat(40);
        let now = chrono::Utc::now();
        let expires = (now + chrono::Duration::minutes(55)).to_rfc3339();
        let reset = (now + chrono::Duration::minutes(10))
            .timestamp()
            .to_string();
        // The stub store: `commits/main` answers the current commit file, the
        // tree and blob endpoints answer the files that commit names.
        let gh = tmp.path().join("gh-store-stub");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
path=""; for a in "$@"; do path="$a"; done
dir="${GH_CONFIG_DIR:-writer}"
echo "$(basename "$dir") $path" >> '@@LOG@@'
if [ -f '@@RATE_LIMITED@@' ]; then
  printf 'HTTP/2.0 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: @@RESET@@\r\n\r\n{"message":"API rate limit exceeded"}'
  echo 'gh: API rate limit exceeded (HTTP 403)' >&2
  exit 1
fi
cur=$(cat '@@COMMIT@@')
case "$path" in
  */git/ref/*)
    printf 'HTTP/2.0 404 Not Found\r\n\r\n{"message":"Not Found"}'
    echo 'gh: Not Found (HTTP 404)' >&2; exit 1;;
  */commits/*)
    printf 'HTTP/2.0 200 OK\r\n\r\n%s' "$cur";;
  */git/trees/*)
    case "$cur" in
      1*) printf 'HTTP/2.0 200 OK\r\n\r\n{"tree":[{"path":"fleet.json","type":"blob","sha":"@@BLOBA@@"}],"truncated":false}';;
      *) printf 'HTTP/2.0 200 OK\r\n\r\n{"tree":[{"path":"fleet.json","type":"blob","sha":"@@BLOBB@@"}],"truncated":false}';;
    esac;;
  */git/blobs/@@BLOBA@@)
    printf 'HTTP/2.0 200 OK\r\n\r\n{"content":"@@B64A@@","encoding":"base64"}';;
  *)
    printf 'HTTP/2.0 200 OK\r\n\r\n{"content":"@@B64B@@","encoding":"base64"}';;
esac
"#
            .replace("@@LOG@@", &log.display().to_string())
            .replace("@@COMMIT@@", &commit.display().to_string())
            .replace("@@RATE_LIMITED@@", &rate_limited.display().to_string())
            .replace("@@RESET@@", &reset)
            .replace("@@BLOBA@@", &blob_a)
            .replace("@@BLOBB@@", &blob_b)
            .replace("@@B64A@@", &b64(&boot_fleet_json("stopped")))
            .replace("@@B64B@@", &b64(&boot_fleet_json("running"))),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The fake minter the workspace ships at the seam the real one uses.
        let mint = tmp.path().join(".loom/scripts/lib/github-app-token.sh");
        std::fs::create_dir_all(mint.parent().unwrap()).unwrap();
        std::fs::write(
            &mint,
            r#"#!/bin/sh
echo "$LOOM_GITHUB_APP_ID $*" >> '@@MINTLOG@@'
if [ -f '@@MINTFAIL@@' ]; then
  printf '{"status":"error","message":"private key not readable"}'
  exit 0
fi
printf '{"status":"ok","token":"ghs_bootstrap","installation_id":"77","app_id":"%s","expires_at":"@@EXPIRES@@"}' "$LOOM_GITHUB_APP_ID"
"#
            .replace("@@MINTLOG@@", &mint_log.display().to_string())
            .replace("@@MINTFAIL@@", &mint_fail.display().to_string())
            .replace("@@EXPIRES@@", &expires),
        )
        .unwrap();
        std::fs::set_permissions(&mint, std::fs::Permissions::from_mode(0o755)).unwrap();
        Boot {
            tmp,
            log,
            mint_log,
            mint_fail,
            rate_limited,
            commit,
            gh,
        }
    }

    /// The production wiring (`GhTransport::new`'s constructor with no
    /// daemon-published root): a pooled workspace that has never published a
    /// reader token, with the real bootstrap mint attached.
    fn transport(&self) -> GhTransport {
        self.transport_for(BOOT_REPO, boot_roster)
    }

    fn transport_for(
        &self,
        repo: &str,
        roster: fn(&Path) -> crate::forge_identity::Roster,
    ) -> GhTransport {
        let mut t = GhTransport::new_with_reader_root(self.tmp.path(), None, repo, roster);
        t.gh_bin = self.gh.display().to_string();
        t
    }

    /// Where the routing expects this host's reader token.
    fn reader_dir(&self) -> PathBuf {
        crate::forge_read_pool::gh_config_dir_for_owner_app(self.tmp.path(), "o", BOOT_APP)
    }

    /// `(credential dir name, path)` per `gh` call.
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

    /// `app id get-token OWNER/REPO` per mint invocation.
    fn mints(&self) -> Vec<String> {
        std::fs::read_to_string(&self.mint_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// The store location the bootstrap reads.
fn boot_location() -> crate::fleet_store::StoreLocation {
    crate::fleet_store::StoreLocation {
        repo: BOOT_REPO.into(),
        reference: "main".into(),
    }
}

#[test]
fn a_pooled_host_with_no_published_reader_mints_one_before_shedding() {
    let b = Boot::new();
    let t = b.transport();
    assert!(
        !b.reader_dir().join("hosts.yml").exists(),
        "no daemon ever published a reader token here"
    );
    let reply = t
        .get(&format!("repos/{BOOT_REPO}/commits/main"), None, None)
        .unwrap();
    assert_eq!(reply.status, 200);
    let calls = b.calls();
    never_the_writer(&calls);
    assert_eq!(
        calls,
        vec![(BOOT_APP.into(), format!("repos/{BOOT_REPO}/commits/main"))],
        "the read ran on the just-minted reader"
    );
    assert_eq!(t.last_credential().unwrap().label(), format!("reader app {BOOT_APP}"));
    assert!(
        b.reader_dir().join("hosts.yml").is_file(),
        "the token was published where the routing looks"
    );
    assert!(t.bootstrap_failure().is_none());
    assert_eq!(b.mints(), vec![format!("{BOOT_APP} get-token {BOOT_REPO}")]);
}

#[test]
fn a_failed_bootstrap_mint_fails_the_read_naming_the_step_without_any_request() {
    let b = Boot::new();
    std::fs::write(&b.mint_fail, "").unwrap();
    let t = b.transport();
    let err = t
        .get(&format!("repos/{BOOT_REPO}/commits/main"), None, None)
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("shed"), "{msg}");
    assert!(msg.contains("private key not readable"), "{msg}");
    assert!(msg.contains("before starting the daemon"), "{msg}");
    assert!(b.calls().is_empty(), "no request was made at all — zero writer reads");
    let why = t
        .bootstrap_failure()
        .expect("the failure is reported for the CLI");
    assert!(why.contains("private key not readable"), "{why}");
}

#[test]
fn a_bootstrap_fetch_advances_a_cached_stop_and_lets_the_host_run() {
    let b = Boot::new();
    let cache = tempfile::tempdir().unwrap();
    let loc = boot_location();
    let now = chrono::Utc::now();

    // The provisioned state: a cached snapshot at commit A recording the host
    // stopped (fetched while a reader token briefly existed), then the token
    // gone — no daemon has run since.
    std::fs::write(&b.commit, COMMIT_A).unwrap();
    crate::fleet_store::fetch::sync(&b.transport(), cache.path(), &loc, now).unwrap();
    assert_eq!(b.mints().len(), 1, "the seed fetch minted its reader");
    std::fs::remove_dir_all(b.reader_dir()).unwrap();

    // The store moves the host to running at commit B; nothing refreshes
    // reader tokens but the read itself. `fetch::load` under AllowStale is
    // exactly what an online `fleet-config render` runs.
    std::fs::write(&b.commit, COMMIT_B).unwrap();
    let later = now + chrono::Duration::hours(3);
    let loaded = crate::fleet_store::fetch::load(
        &b.transport(),
        cache.path(),
        &loc,
        crate::fleet_store::fetch::Policy::AllowStale,
        later,
    )
    .unwrap();
    assert!(
        matches!(loaded.freshness, crate::fleet_store::fetch::Freshness::Live { changed: true }),
        "the online render confirms the store, not the cache"
    );
    assert_eq!(loaded.snapshot.manifest.commit, COMMIT_B);
    let calls = b.calls();
    assert!(!calls.is_empty());
    never_the_writer(&calls);
    assert!(
        calls.iter().all(|(d, _)| d == BOOT_APP),
        "every request was reader-authenticated: {calls:?}"
    );

    // The daemon's start-time state check over the advanced cache — on a
    // fresh process that finds the token the render published — lets it run.
    let mints_before_start = b.mints().len();
    let pass =
        crate::fleet_state::state_pass(&b.transport(), cache.path(), &loc, "boot-h", None, later);
    assert!(!pass.cached);
    assert_eq!(pass.desired, Some(crate::fleet_store::state::RunState::Running));
    assert_eq!(pass.enforcement(), crate::fleet_state::Enforcement::Proceed);
    assert_eq!(
        b.mints().len(),
        mints_before_start,
        "the start check reuses the token the render published"
    );
}

#[test]
fn a_withdrawn_reader_keeps_the_ordinary_cached_fallback() {
    const APP: &str = "147002";
    const REPO: &str = "outage-fixture-14730/fleet";
    fn roster(ws: &Path) -> crate::forge_identity::Roster {
        let mut roster = boot_roster(ws);
        roster.readers[0].app_id = APP.into();
        roster
    }

    let b = Boot::new();
    let cache = tempfile::tempdir().unwrap();
    let location = crate::fleet_store::StoreLocation {
        repo: REPO.into(),
        reference: "main".into(),
    };
    let now = chrono::Utc::now();
    std::fs::write(&b.commit, COMMIT_A).unwrap();
    crate::fleet_store::fetch::sync(&b.transport_for(REPO, roster), cache.path(), &location, now)
        .unwrap();
    let mints = b.mints();
    assert_eq!(mints.len(), 1);
    std::fs::write(&b.log, "").unwrap();
    std::fs::write(&b.rate_limited, "").unwrap();

    let transport = b.transport_for(REPO, roster);
    let expect_cached = |candidate: &GhTransport| {
        let loaded = crate::fleet_store::fetch::load(
            candidate,
            cache.path(),
            &location,
            crate::fleet_store::fetch::Policy::AllowStale,
            now,
        )
        .unwrap();
        assert!(matches!(loaded.freshness, crate::fleet_store::fetch::Freshness::Cached { .. }));
        assert_eq!(loaded.snapshot.manifest.commit, COMMIT_A);
        assert_eq!(
            candidate.bootstrap_failure(),
            None,
            "an ordinary withdrawal is not a bootstrap failure"
        );
        assert_eq!(b.mints(), mints, "a withdrawn published reader must not be minted again");
    };
    expect_cached(&transport);
    expect_cached(&transport);
    expect_cached(&b.transport_for(REPO, roster));
    let calls = b.calls();
    never_the_writer(&calls);
    assert_eq!(calls, vec![(APP.into(), format!("repos/{REPO}/commits/main"))]);
}

#[test]
fn a_bootstrap_mint_that_cannot_route_has_a_nonempty_failure() {
    const APP: &str = "147003";
    const REPO: &str = "blocked-bootstrap-fixture-14730/fleet";
    fn roster(ws: &Path) -> crate::forge_identity::Roster {
        let mut roster = boot_roster(ws);
        roster.readers[0].app_id = APP.into();
        roster
    }

    let b = Boot::new();
    crate::forge_identity::withdraw_after(
        APP,
        REPO,
        crate::forge_identity::Failure::Credential,
        "fixture withdrawal",
    );
    let transport = b.transport_for(REPO, roster);
    let error = transport
        .get(&format!("repos/{REPO}/commits/main"), None, None)
        .unwrap_err();
    let reason = format!("published bootstrap credentials still cannot route a read of {REPO}");
    assert_eq!(transport.bootstrap_failure().as_deref(), Some(reason.as_str()));
    assert!(error.to_string().contains(&reason));
    assert_eq!(b.mints().len(), 1);
    assert!(b.calls().is_empty(), "no reader or writer may serve the withdrawn route");
}

#[test]
fn a_blocked_bootstrap_keeps_the_cached_stop_effective() {
    let b = Boot::new();
    let cache = tempfile::tempdir().unwrap();
    let loc = boot_location();
    let now = chrono::Utc::now();
    std::fs::write(&b.commit, COMMIT_A).unwrap();
    crate::fleet_store::fetch::sync(&b.transport(), cache.path(), &loc, now).unwrap();
    std::fs::remove_dir_all(b.reader_dir()).unwrap();
    // The store moved on, but this host cannot provision a reader.
    std::fs::write(&b.commit, COMMIT_B).unwrap();
    std::fs::write(&b.mint_fail, "").unwrap();
    std::fs::write(&b.log, "").unwrap();

    let later = now + chrono::Duration::hours(3);
    let t = b.transport();
    let loaded = crate::fleet_store::fetch::load(
        &t,
        cache.path(),
        &loc,
        crate::fleet_store::fetch::Policy::AllowStale,
        later,
    )
    .unwrap();
    assert!(matches!(loaded.freshness, crate::fleet_store::fetch::Freshness::Cached { .. }));
    t.bootstrap_failure()
        .expect("the blocked bootstrap is reported for the render's refusal");
    assert!(
        b.calls().is_empty(),
        "the blocked bootstrap made zero forge requests — zero writer reads"
    );
    assert!(!b.mints().is_empty(), "the mint was attempted");

    // The cached stop stays effective: the daemon's start-time state check
    // still refuses over the unadvanced cache.
    let pass = crate::fleet_state::state_pass(&t, cache.path(), &loc, "boot-h", None, later);
    assert!(pass.cached);
    assert_eq!(pass.desired, Some(crate::fleet_store::state::RunState::Stopped));
    assert_eq!(
        pass.enforcement(),
        crate::fleet_state::Enforcement::Stop,
        "a blocked bootstrap must never resume a stopped host"
    );
}
