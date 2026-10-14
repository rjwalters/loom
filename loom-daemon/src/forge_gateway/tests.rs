#![allow(clippy::unwrap_used)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::Instant;

use serde_json::json;

use super::*;
use crate::forge_bucket_book::Resource;

const KEY: &str = "gp_0123456789abcdef0123456789abcdef";

/// The published-profile state is process-global: tests that move it run
/// one at a time.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn cfg(owners: &[(&str, &str)]) -> Value {
    let refs: serde_json::Map<String, Value> = owners
        .iter()
        .map(|(o, r)| ((*o).to_string(), json!(r)))
        .collect();
    json!({"forge": {"gateway": {"origin": "https://gw.example.com", "keyRefs": refs}}})
}

/// A fake outside world: a fixed gh, keys by ref.
struct Fake {
    gh: Option<GhVersion>,
    keys: BTreeMap<String, Result<String, String>>,
    reads: RefCell<u32>,
}

impl Fake {
    fn new(gh: Option<GhVersion>) -> Self {
        Self {
            gh,
            keys: BTreeMap::new(),
            reads: RefCell::new(0),
        }
    }
    fn key(mut self, r: &str, k: Result<&str, &str>) -> Self {
        self.keys
            .insert(r.to_string(), k.map(str::to_string).map_err(str::to_string));
        self
    }
}

impl Deps for Fake {
    fn gh(&self, _: &Path) -> Option<(PathBuf, GhVersion)> {
        self.gh.map(|v| (PathBuf::from("/opt/gh/bin/gh"), v))
    }
    fn read_key(&self, r: &KeyRef) -> Result<String, String> {
        *self.reads.borrow_mut() += 1;
        let KeyRef::File(p) = r else {
            return Err("ssm not faked".into());
        };
        self.keys
            .get(&p.to_string_lossy().into_owned())
            .cloned()
            .unwrap_or_else(|| Err("no such fake key".into()))
    }
}

fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[test]
fn an_absent_or_null_block_is_off_without_a_word() {
    for v in [
        json!({}),
        json!({"forge": {}}),
        json!({"forge": {"gateway": null}}),
    ] {
        assert_eq!(parse(&v), (None, Vec::<String>::new()), "{v}");
    }
}

#[test]
fn a_valid_block_parses_owners_lowercased_and_accepts_key_refs() {
    let (c, w) = parse(&cfg(&[("2AMLogic", "file:/k/a"), ("rjwalters", "ssm:///gp/r/key")]));
    let c = c.unwrap();
    assert!(w.is_empty(), "{w:?}");
    assert_eq!(c.api_host, "gw.example.com");
    assert_eq!(c.key_refs["2amlogic"], KeyRef::File("/k/a".into()));
    assert_eq!(c.key_refs["rjwalters"], KeyRef::Ssm("/gp/r/key".into()));
    let alias = json!({"forge": {"gateway": {"origin": "gw.example.com",
        "key_refs": {"acme": "file:/k"}}}});
    assert_eq!(parse(&alias).0.unwrap().key_refs.len(), 1);
}

#[test]
fn an_origin_that_could_send_a_key_elsewhere_is_refused() {
    for bad in [
        "http://gw.example.com",
        "https://api.github.com",
        "github.com",
        "https://gw.example.com/path",
        "https://user@gw.example.com",
        "https://gw.example.com:8443",
        "localhost",
    ] {
        assert!(api_host_of(bad).is_err(), "{bad}");
        let v = json!({"forge": {"gateway": {"origin": bad, "keyRefs": {"acme": "file:/k"}}}});
        let (c, w) = parse(&v);
        assert!(c.is_none() && !w.is_empty(), "{bad}");
    }
    assert_eq!(api_host_of("https://GW.example.com/").unwrap(), "gw.example.com");
}

#[test]
fn bad_entries_are_dropped_and_no_usable_owner_is_off() {
    let (c, w) = parse(&cfg(&[
        ("bad owner", "file:/k"),
        ("acme", "vault:x"),
        ("_note", "x"),
    ]));
    assert!(c.is_none());
    assert_eq!(w.len(), 3, "{w:?}");
}

#[test]
fn key_refs_parse_and_a_key_is_one_url_safe_token() {
    assert_eq!(KeyRef::parse("file: /x ").unwrap(), KeyRef::File("/x".into()));
    assert!(KeyRef::parse("ssm://").is_err());
    assert!(KeyRef::parse("/plain/path").is_err());
    assert!(valid_key(KEY));
    for bad in [
        "short",
        "gp_has space 0123456789",
        "gp_0123456789abcdef\nx: y",
    ] {
        assert!(!valid_key(bad), "{bad:?}");
    }
}

#[test]
fn a_file_key_must_be_owner_only_and_never_appears_in_the_error() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("acme.key");
    std::fs::write(&path, format!("{KEY}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let r = KeyRef::File(path.clone());
    let err = r.read().unwrap_err();
    assert!(!err.contains(KEY), "{err}");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(r.read().unwrap(), KEY);
    std::fs::write(&path, "not a key at all\nsecond line").unwrap();
    let err = r.read().unwrap_err();
    assert!(!err.contains("not a key"), "{err}");
}

#[test]
fn gh_versions_parse_and_compare() {
    let v = parse_gh_version("gh version 2.102.0 (2026-09-30)\nhttps://github.com/cli/cli\n");
    assert_eq!(v, Some(GhVersion(2, 102, 0)));
    assert_eq!(parse_gh_version("gh version 2.100.0-rc.1 (x)"), Some(GhVersion(2, 100, 0)));
    assert_eq!(parse_gh_version("gh version 2.96.0"), Some(GhVersion(2, 96, 0)));
    assert!(GhVersion(2, 96, 0) < MIN_GH_VERSION && GhVersion(2, 100, 0) >= MIN_GH_VERSION);
    assert_eq!(parse_gh_version("bash: gh: not found"), None);
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

#[test]
fn a_profile_carries_the_key_and_api_host_privately() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".loom").join(DIR_NAME).join("acme");
    publish_profile(&dir, KEY, "gw.example.com").unwrap();
    let hosts = std::fs::read_to_string(dir.join("hosts.yml")).unwrap();
    assert_eq!(hosts, crate::credential_preflight::gh_hosts_yaml(KEY, Some("gw.example.com")));
    assert!(hosts.contains("    api_host: gw.example.com\n"), "{hosts}");
    assert_eq!(
        std::fs::read_to_string(dir.join("config.yml")).unwrap(),
        crate::credential_preflight::GH_CONFIG_YAML
    );
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("hosts.yml")), 0o600);
    assert_eq!(mode(&dir.join("config.yml")), 0o600);
    assert!(publish_profile(&dir, "bad key", "gw.example.com").is_err());
    assert!(publish_profile(&dir, KEY, "api.github.com").is_err());
    // A gateway profile books as the gateway, never as an App bucket.
    let cred = crate::gh_invocation::accounting::cred_of_with(Some(&dir), false);
    assert_eq!((cred.kind, cred.account.as_str()), (CRED_KIND, GATEWAY_APP_ID));
    assert_eq!(cred.owner.as_deref(), Some("acme"));
    assert!(!cred.is_app());
    assert_eq!(
        crate::forge_bucket_book::classify_dir(&dir),
        crate::forge_bucket_book::DirClass::Other
    );
}

#[test]
fn a_pass_publishes_each_owner_and_rewrites_api_host_on_every_publish() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let fake = Fake::new(Some(GhVersion(2, 102, 0)))
        .key("/k/a", Ok(KEY))
        .key("/k/b", Err("permission denied"));
    let mut r = Refresher::default();
    let now = Instant::now();
    let out = r.pass_with(ws, &cfg(&[("gwt-a", "file:/k/a"), ("gwt-b", "file:/k/b")]), now, &fake);
    assert_eq!(
        out,
        PassOutcome::Published {
            owners: vec!["gwt-a".into()],
            failed: vec!["gwt-b".into()],
        }
    );
    let base = base_dir(ws);
    assert!(base.join("gwt-a/hosts.yml").is_file());
    assert!(!base.join("gwt-b").exists(), "a failed owner keeps today's routing");
    assert_eq!(profile_for("GWT-A"), Some(base.join("gwt-a")));
    assert_eq!(profile_for("gwt-b"), None);
    let env = agent_env();
    assert_eq!(env[1], (DIR_ENV, Some(base.clone().into_os_string())));

    // Nothing due within the republish interval.
    assert_eq!(
        r.pass_with(ws, &cfg(&[("gwt-a", "file:/k/a"), ("gwt-b", "file:/k/b")]), now, &fake),
        PassOutcome::Unchanged
    );
    // A new origin is due at once, and every rewritten hosts.yml carries it.
    let moved = json!({"forge": {"gateway": {"origin": "gw2.example.com",
        "keyRefs": {"gwt-a": "file:/k/a"}}}});
    assert!(matches!(r.pass_with(ws, &moved, now, &fake), PassOutcome::Published { .. }));
    let hosts = std::fs::read_to_string(base.join("gwt-a/hosts.yml")).unwrap();
    assert!(hosts.contains("api_host: gw2.example.com\n"), "{hosts}");
    // A hand-removed profile is republished on the next pass.
    std::fs::remove_dir_all(base.join("gwt-a")).unwrap();
    assert!(matches!(r.pass_with(ws, &moved, now, &fake), PassOutcome::Published { .. }));
    assert!(base.join("gwt-a/hosts.yml").is_file());

    // Unset: everything goes, and nothing routes.
    assert_eq!(r.pass_with(ws, &json!({}), now, &fake), PassOutcome::Off);
    assert!(!base.exists());
    assert_eq!(profile_for("gwt-a"), None);
    assert_eq!(agent_env()[1], (DIR_ENV, None));
}

#[test]
fn an_old_gh_publishes_nothing_and_removes_what_was_there() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let stale = base_dir(ws).join("gwt-old");
    publish_profile(&stale, KEY, "gw.example.com").unwrap();
    for gh in [Some(GhVersion(2, 96, 0)), None] {
        let fake = Fake::new(gh).key("/k/a", Ok(KEY));
        let mut r = Refresher::default();
        let out = r.pass_with(ws, &cfg(&[("gwt-old", "file:/k/a")]), Instant::now(), &fake);
        assert_eq!(out, PassOutcome::GhTooOld(gh));
        assert_eq!(*fake.reads.borrow(), 0, "no key is read for a gh that would leak it");
        assert!(!base_dir(ws).exists());
        assert_eq!(profile_for("gwt-old"), None);
    }
}

#[test]
fn a_dropped_owner_s_profile_is_removed() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let fake = Fake::new(Some(MIN_GH_VERSION))
        .key("/k/a", Ok(KEY))
        .key("/k/c", Ok(KEY));
    let mut r = Refresher::default();
    let now = Instant::now();
    r.pass_with(ws, &cfg(&[("gwt-c", "file:/k/c"), ("gwt-d", "file:/k/a")]), now, &fake);
    assert!(base_dir(ws).join("gwt-d").exists());
    r.pass_with(ws, &cfg(&[("gwt-c", "file:/k/c")]), now, &fake);
    assert!(!base_dir(ws).join("gwt-d").exists());
    assert!(base_dir(ws).join("gwt-c").exists());
    r.pass_with(ws, &json!({}), now, &fake);
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

#[test]
fn a_live_profile_routes_reads_as_the_gateway_reader() {
    let tmp = tempfile::tempdir().unwrap();
    let live = |o: &str| (o == "gwt-route").then(|| tmp.path().to_path_buf());
    let now = SystemTime::now();
    let req = RouteRequest::gate("GWT-Route/repo", None, Resource::Core);
    let d = decide(&req, now, &live).unwrap();
    assert_eq!(
        d,
        RouteDecision::Reader {
            dir: tmp.path().to_path_buf(),
            app_id: GATEWAY_APP_ID.into(),
            placement: Placement::Home,
        }
    );
    // Another forge host, another owner: no gateway.
    let ghe = RouteRequest::gate("gwt-route/repo", Some("ghe.example.com"), Resource::Core);
    assert!(decide(&ghe, now, &live).is_none());
    let other = RouteRequest::gate("someone/repo", None, Resource::Core);
    assert!(decide(&other, now, &live).is_none());
    // A withdrawal (after a failure through the reader chain) takes it out.
    crate::forge_identity::withdraw_reader_for_repo_until(
        GATEWAY_APP_ID,
        "gwt-route/withdrawn",
        now + Duration::from_secs(60),
    );
    let w = RouteRequest::gate("gwt-route/withdrawn", None, Resource::Core);
    assert!(decide(&w, now, &live).is_none());
}

/// Break-glass (2am#4367 P4): with `forge.gateway` unset the router is
/// exactly today's — the same answer as `forge_identity::route_read` for
/// every request — even when a previous run left profiles on disk, and no
/// agent is told about a gateway.
#[test]
fn with_the_gateway_unset_routing_is_byte_identical_to_today() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let leftover = base_dir(ws).join("gwt-bg");
    publish_profile(&leftover, KEY, "gw.example.com").unwrap();
    let mut r = Refresher::default();
    let fake = Fake::new(Some(GhVersion(2, 102, 0))).key("/k/a", Ok(KEY));
    let off = json!({"forge": {"githubApp": {"appId": "1"}}});
    assert_eq!(r.pass_with(ws, &off, Instant::now(), &fake), PassOutcome::Off);
    assert!(!base_dir(ws).exists(), "leftover profiles are removed");
    assert_eq!(*fake.reads.borrow(), 0);
    let now = SystemTime::now();
    for (repo, host, res) in [
        ("gwt-bg/repo", None, Resource::Core),
        ("gwt-bg/repo", Some("github.com"), Resource::Graphql),
        ("rjwalters/loom", None, Resource::Search),
    ] {
        let req = RouteRequest::gate(repo, host, res).affinity(Some("k"));
        assert_eq!(route_read(&req, now), crate::forge_identity::route_read(&req, now), "{repo}");
        assert_eq!(crate::gh_invocation::read_router(false)(&req, now), route_read(&req, now));
    }
    assert_eq!(profile_for("gwt-bg"), None);
    assert!(!any_live(), "the agent front does no gateway work at all");
    assert_eq!(agent_env(), vec![(NO_UPDATE_NOTIFIER_ENV, Some("1".into())), (DIR_ENV, None)]);
}

#[test]
fn route_labels_split_gateway_direct_reads_and_the_rest() {
    assert_eq!(route_label(CRED_KIND, true), "gateway");
    assert_eq!(route_label("writer", true), "direct_read");
    assert_eq!(route_label("reader", true), "direct_read");
    assert_eq!(route_label("writer", false), "direct");
    for r in ["gateway", "direct_read", "direct"] {
        assert!(ROUTES.contains(&r));
    }
}

#[test]
fn a_front_routes_only_when_its_gh_is_the_checked_one() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = tmp.path().join("gh");
    std::fs::write(&gh, "").unwrap();
    let other = tmp.path().join("gh-other");
    std::fs::write(&other, "").unwrap();
    std::fs::write(
        tmp.path().join("gh-ok"),
        std::fs::canonicalize(&gh)
            .unwrap()
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    assert!(publish::gh_marker_matches(tmp.path(), &gh));
    assert!(!publish::gh_marker_matches(tmp.path(), &other));
    assert!(!publish::gh_marker_matches(&tmp.path().join("none"), &gh));
}
