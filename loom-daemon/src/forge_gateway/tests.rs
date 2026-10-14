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
    fn read_key(&self, r: &KeyRef, _aws: &AwsAccess<'_>) -> Result<String, String> {
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
        "github.com.",
        "api.github.com.",
        "https://API.GitHub.com./",
    ] {
        assert!(api_host_of(bad).is_err(), "{bad}");
        let v = json!({"forge": {"gateway": {"origin": bad, "keyRefs": {"acme": "file:/k"}}}});
        let (c, w) = parse(&v);
        assert!(c.is_none() && !w.is_empty(), "{bad}");
    }
    assert_eq!(api_host_of("https://GW.example.com/").unwrap(), "gw.example.com");
    assert_eq!(api_host_of("gw.example.com.").unwrap(), "gw.example.com");
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
    let err = r.read(&AwsAccess::default(), CONFIG_KEY).unwrap_err();
    assert!(!err.contains(KEY), "{err}");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(r.read(&AwsAccess::default(), CONFIG_KEY).unwrap(), KEY);
    std::fs::write(&path, "not a key at all\nsecond line").unwrap();
    let err = r.read(&AwsAccess::default(), CONFIG_KEY).unwrap_err();
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
    assert_eq!(
        hosts,
        format!("github.com:\n    oauth_token: {KEY}\n    api_host: gw.example.com\n")
    );
    // Never a git credential: no user, no git_protocol.
    assert!(!hosts.contains("user:") && !hosts.contains("git_protocol"), "{hosts}");
    assert_eq!(std::fs::read_to_string(dir.join("config.yml")).unwrap(), "version: 1\n");
    assert_eq!(profile_api_host(&dir).as_deref(), Some("gw.example.com"));
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

/// 2AMLogic/2am#4367: a CLI run by a host job routes through the daemon's
/// published profiles only on the front's terms: the variable is set, its
/// marker names the `gh` the facade will exec, and no front outage holds.
#[test]
fn a_cli_routes_only_through_a_checked_gh_and_outside_an_outage() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join(".loom").join(DIR_NAME);
    std::fs::create_dir_all(&base).unwrap();
    set_mode(&base, 0o700);
    let gh = tmp.path().join("gh");
    std::fs::write(&gh, "").unwrap();
    let other = tmp.path().join("gh-other");
    std::fs::write(&other, "").unwrap();
    std::fs::write(
        base.join("gh-ok"),
        std::fs::canonicalize(&gh)
            .unwrap()
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    let env = Some(base.clone().into_os_string());
    let now = SystemTime::now();
    let gh_s = gh.to_str().unwrap();

    assert_eq!(cli_base(env.clone(), gh_s, now), Some(base.clone()));
    // Unset or empty: today's routing.
    assert_eq!(cli_base(None, gh_s, now), None);
    assert_eq!(cli_base(Some(OsString::new()), gh_s, now), None);
    // A different gh than the daemon checked: it might ignore api_host.
    assert_eq!(cli_base(env.clone(), other.to_str().unwrap(), now), None);
    // A gh that does not resolve (a bare name on no PATH entry).
    assert_eq!(cli_base(env.clone(), "loom-test-no-such-gh-binary", now), None);
    // A directory the daemon never published into.
    let empty = tmp.path().join(".loom").join("empty");
    std::fs::create_dir(&empty).unwrap();
    assert_eq!(cli_base(Some(empty.into_os_string()), gh_s, now), None);
    // A front outage the daemon or a front recorded holds for its window.
    outage::note_front_outage(&base);
    assert_eq!(cli_base(env.clone(), gh_s, now), None);
    assert_eq!(cli_base(env, gh_s, now + Duration::from_secs(3600)), Some(base));
}

fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A published base the way the daemon leaves it, with a `gh-ok` for `gh`.
fn published(root: &std::path::Path, gh: &std::path::Path) -> PathBuf {
    let base = root.join(".loom").join(DIR_NAME);
    let profile = base.join("acme");
    std::fs::create_dir_all(&profile).unwrap();
    set_mode(&base, 0o700);
    set_mode(&profile, 0o700);
    std::fs::write(profile.join("hosts.yml"), "github.com: {}").unwrap();
    set_mode(&profile.join("hosts.yml"), 0o600);
    std::fs::write(
        base.join("gh-ok"),
        std::fs::canonicalize(gh)
            .unwrap()
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    base
}

/// A consumer's base comes from its environment, so the directory itself is
/// checked: the shape the daemon publishes, ours, closed to others, no links.
#[test]
fn a_front_or_cli_refuses_an_untrusted_base_or_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = tmp.path().join("gh");
    std::fs::write(&gh, "").unwrap();
    let now = SystemTime::now();
    let base = published(tmp.path(), &gh);
    let env = |b: &std::path::Path| Some(b.to_path_buf().into_os_string());
    assert_eq!(front_base(env(&base), &gh, now), Some(base.clone()));
    assert!(publish::safe_profile(&base.join("acme")));

    // A symlink to a valid published dir.
    let link_root = tmp.path().join("link");
    std::fs::create_dir_all(link_root.join(".loom")).unwrap();
    let link = link_root.join(".loom").join(DIR_NAME);
    std::os::unix::fs::symlink(&base, &link).unwrap();
    assert_eq!(front_base(env(&link), &gh, now), None);

    // Not named `.loom/gh-config-gateway`.
    let odd = tmp.path().join("elsewhere");
    let odd_gw = odd.join(".loom").join("gateway");
    std::fs::create_dir_all(&odd_gw).unwrap();
    set_mode(&odd_gw, 0o700);
    std::fs::copy(base.join("gh-ok"), odd_gw.join("gh-ok")).unwrap();
    assert_eq!(front_base(env(&odd_gw), &gh, now), None);
    let tmp_x = tmp.path().join("x").join(DIR_NAME);
    std::fs::create_dir_all(&tmp_x).unwrap();
    set_mode(&tmp_x, 0o700);
    std::fs::copy(base.join("gh-ok"), tmp_x.join("gh-ok")).unwrap();
    assert_eq!(front_base(env(&tmp_x), &gh, now), None);

    // Group- or world-writable base, profile dir or hosts.yml.
    for mode in [0o770, 0o777] {
        set_mode(&base, mode);
        assert_eq!(front_base(env(&base), &gh, now), None, "base {mode:o}");
        set_mode(&base, 0o700);
    }
    let profile = base.join("acme");
    let hosts = profile.join("hosts.yml");
    set_mode(&hosts, 0o666);
    assert!(!publish::safe_profile(&profile), "hosts.yml 666");
    set_mode(&hosts, 0o600);
    set_mode(&profile, 0o770);
    assert!(!publish::safe_profile(&profile), "profile 770");
    set_mode(&profile, 0o700);
    assert!(publish::safe_profile(&profile));

    // `hosts.yml` (or the owner dir) a symlink.
    let real = tmp.path().join("real-hosts.yml");
    std::fs::write(&real, "github.com: {}").unwrap();
    set_mode(&real, 0o600);
    std::fs::remove_file(&hosts).unwrap();
    std::os::unix::fs::symlink(&real, &hosts).unwrap();
    assert!(!publish::safe_profile(&profile));
    std::fs::remove_file(&hosts).unwrap();
    std::fs::rename(&real, &hosts).unwrap();
    let real_dir = tmp.path().join("real-acme");
    std::fs::rename(&profile, &real_dir).unwrap();
    std::os::unix::fs::symlink(&real_dir, &profile).unwrap();
    assert!(!publish::safe_profile(&profile));
}

/// The marker and the outage file are read by path: a symlink (to a copy
/// of a valid marker, or to a long-expired outage) is refused, as is a
/// writable marker, so no other file can answer for either.
#[test]
fn a_front_or_cli_refuses_a_linked_or_writable_marker_or_outage_file() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = tmp.path().join("gh");
    std::fs::write(&gh, "").unwrap();
    let gh_s = gh.to_str().unwrap();
    let now = SystemTime::now();
    let adopt = |b: &std::path::Path| cli_base(Some(b.as_os_str().to_owned()), gh_s, now);

    let marked = published(&tmp.path().join("ws1"), &gh);
    assert_eq!(adopt(&marked), Some(marked.clone()));
    let copy = tmp.path().join("marker-copy");
    std::fs::copy(marked.join("gh-ok"), &copy).unwrap();
    std::fs::remove_file(marked.join("gh-ok")).unwrap();
    std::os::unix::fs::symlink(&copy, marked.join("gh-ok")).unwrap();
    assert_eq!(adopt(&marked), None, "a symlinked marker");

    let loose = published(&tmp.path().join("ws2"), &gh);
    set_mode(&loose.join("gh-ok"), 0o666);
    assert_eq!(adopt(&loose), None, "a 0666 marker");

    let outaged = published(&tmp.path().join("ws3"), &gh);
    let expired = tmp.path().join("long-ago");
    std::fs::write(&expired, "0").unwrap();
    std::os::unix::fs::symlink(&expired, outaged.join(outage::FRONT_OUTAGE)).unwrap();
    assert_eq!(adopt(&outaged), None, "a symlinked outage file");
    std::fs::remove_file(outaged.join(outage::FRONT_OUTAGE)).unwrap();
    assert_eq!(adopt(&outaged), Some(outaged.clone()));
    // An outage file the front itself writes (0600) is honoured, not refused.
    outage::note_front_outage(&outaged);
    assert_eq!(adopt(&outaged), None);
    assert_eq!(
        cli_base(Some(outaged.as_os_str().to_owned()), gh_s, now + Duration::from_secs(3600)),
        Some(outaged)
    );
}

/// #7818's leak class: a profile holds a live key, so its path must be
/// ignored by the deployed `.gitignore` block AND refused by every stager's
/// credential list, without either list naming it.
#[test]
fn a_profile_path_is_never_committable() {
    let path = format!(".loom/{DIR_NAME}/acme/hosts.yml");
    assert!(crate::init::is_credential_path(&path), "{path}");
    assert!(crate::init::is_credential_path(&format!(".loom/{DIR_NAME}/")));
    let tmp = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    crate::init::update_gitignore(tmp.path()).unwrap();
    for p in [path.as_str(), &format!(".loom/{DIR_NAME}/gh-ok")] {
        assert!(git(&["check-ignore", "-q", p]).status.success(), "{p} is not ignored");
    }
    // And no App-credential classifier mistakes it for one of its own.
    let dir = tmp.path().join(".loom").join(DIR_NAME).join("acme");
    assert_eq!(
        crate::forge_bucket_book::classify_dir(&dir),
        crate::forge_bucket_book::DirClass::Other
    );
    assert_eq!(owner_of_profile_dir(&dir).as_deref(), Some("acme"));
    assert_eq!(owner_of_profile_dir(&tmp.path().join(".loom/gh-config-by-owner/acme")), None);
}

/// `--include` stdout for `status` with `headers`.
fn included(status: u16, headers: &[&str], body: &str) -> String {
    let mut h = format!("HTTP/2.0 {status} X\r\n");
    for line in headers {
        h.push_str(line);
        h.push_str("\r\n");
    }
    format!("{h}\r\n{body}")
}

/// Only an outage the gateway itself signals withdraws it: a 5xx GitHub
/// returned through the relay, or a gh that failed before sending anything,
/// never does.
#[test]
fn only_an_outage_the_gateway_signals_withdraws_it() {
    use crate::forge_listing::parse_http_response;
    use outage::{http_status_in, is_outage};
    assert_eq!(http_status_in("gh: Bad Gateway (HTTP 502)"), Some(502));
    let check = |stdout: String, stderr: &str| {
        let r = parse_http_response(&stdout);
        is_outage(r.as_ref(), &stdout, stderr)
    };
    let version = "x-github-proxy-version: 0.4.0";
    // With headers.
    let own = included(502, &[version, "x-github-proxy-refusal: upstream_error"], "{}");
    assert!(check(own, "gh: HTTP 502"), "the gateway's own 502");
    let edge = included(502, &[], "<html>bad gateway</html>");
    assert!(check(edge, "gh: HTTP 502"), "no gateway header: the Worker was never reached");
    let relayed = included(502, &[version], r#"{"message":"Server Error"}"#);
    assert!(!check(relayed, "gh: HTTP 502"), "GitHub's 502 through the relay");
    let refused = included(403, &[version, "x-github-proxy-refusal: write_not_permitted"], "{}");
    assert!(!check(refused, "gh: HTTP 403"), "a refusal is an answer, not an outage");
    // Without headers: the body on stdout says whose 5xx it is.
    assert!(check(r#"{"error":"upstream_error","request_id":"abc"}"#.into(), "gh: HTTP 502"));
    assert!(!check(
        r#"{"message":"Server Error","documentation_url":"x"}"#.into(),
        "gh: HTTP 502"
    ));
    assert!(
        !check(String::new(), "gh: Bad Gateway (HTTP 502)"),
        "unmarked: not the gateway's"
    );
    assert!(check(String::new(), "gh: HTTP 522"), "a Cloudflare edge status");
    // No HTTP status at all: a transport error, or a gh that never sent.
    for stderr in [
        "Get \"https://gw/repos/a/b\": dial tcp: connection refused",
        "error connecting to gw.example.com: no such host",
    ] {
        assert!(check(String::new(), stderr), "{stderr}");
    }
    for stderr in [
        "unknown flag: --nope",
        "accepts at most 1 arg(s), received 2",
        "GraphQL: Could not resolve to an Issue with the number of 9.",
        "",
    ] {
        assert!(!check(String::new(), stderr), "{stderr}");
    }
    assert!(!check(included(404, &[version], "{}"), "gh: Not Found (HTTP 404)"));

    // Rerun vs withdraw: any 5xx reruns; only the gateway's own evidence
    // withdraws.
    use outage::{fault, Fault};
    let f = |stdout: String, stderr: &str| {
        let r = parse_http_response(&stdout);
        fault(r.as_ref(), &stdout, stderr)
    };
    assert_eq!(f(String::new(), "HTTP 500 (https://gw/graphql)"), Fault::Rerun);
    assert_eq!(f(included(502, &[version], "{}"), "gh: HTTP 502"), Fault::Rerun);
    assert_eq!(
        f(included(500, &[version, "x-github-proxy-refusal: misconfigured"], "{}"), ""),
        Fault::Withdraw
    );
    assert_eq!(f(String::new(), "dial tcp: connection refused"), Fault::Withdraw);
    assert_eq!(f(String::new(), "gh: Not Found (HTTP 404)"), Fault::None);
    assert_eq!(f(String::new(), "unknown flag: --nope"), Fault::None);
}

#[test]
fn a_front_outage_marker_holds_for_its_window() {
    let tmp = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    assert!(!outage::front_outage_active(tmp.path(), now));
    outage::note_front_outage(tmp.path());
    assert!(outage::front_outage_active(tmp.path(), now));
    assert!(!outage::front_outage_active(tmp.path(), now + Duration::from_secs(3600)));
    assert_eq!(mode(&tmp.path().join("outage-until")), 0o600);
}

/// The canary's refs (fleet-gitops#276) parse, and an `ssm://` read always
/// names its region: configured first, then the AWS CLI's own env order.
#[test]
fn ssm_refs_parse_and_are_read_in_an_explicit_region() {
    let v = json!({"forge": {"gateway": {"origin": "https://github-proxy.example.com",
        "region": "us-east-1",
        "keyRefs": {
            "2AMLogic": "ssm:///github-proxy/profiles/relay-loom-worker-3-2amlogic/key",
            "rjwalters": "ssm:///github-proxy/profiles/relay-loom-worker-3-rjwalters/key"}}}});
    let (c, w) = parse(&v);
    let c = c.unwrap();
    assert!(w.is_empty(), "{w:?}");
    assert_eq!(
        c.key_refs["2amlogic"],
        KeyRef::Ssm("/github-proxy/profiles/relay-loom-worker-3-2amlogic/key".into())
    );
    assert_eq!(c.region.as_deref(), Some("us-east-1"));

    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
        }
    };
    let none = env(&[]);
    let both = env(&[
        ("AWS_REGION", "eu-west-1"),
        ("AWS_DEFAULT_REGION", "us-west-2"),
    ]);
    let dflt = env(&[("AWS_DEFAULT_REGION", "us-west-2")]);
    assert_eq!(resolve_region(Some("us-east-1"), &both).as_deref(), Some("us-east-1"));
    assert_eq!(resolve_region(None, &both).as_deref(), Some("eu-west-1"));
    assert_eq!(resolve_region(None, &dflt).as_deref(), Some("us-west-2"));
    assert_eq!(resolve_region(None, &none), None);

    let args = ssm_args("/p/k", "us-east-1", None);
    let at = args.iter().position(|a| a == "--region").unwrap();
    assert_eq!(args[at + 1], "us-east-1");
    assert!(args.contains(&"--with-decryption".to_string()));

    // No region anywhere: refused before any AWS call, saying how to fix it.
    let err = KeyRef::Ssm("/p/k".into())
        .read(&AwsAccess::default(), CONFIG_KEY)
        .unwrap_err();
    assert!(err.contains("forge.gateway.region") && err.contains("AWS_REGION"), "{err}");

    let bad = json!({"forge": {"gateway": {"origin": "gw.example.com", "region": "US East",
        "keyRefs": {"acme": "ssm:///p"}}}});
    let (c, w) = parse(&bad);
    assert_eq!(c.unwrap().region, None);
    assert_eq!(w.len(), 1, "{w:?}");
}

/// An `ssm://` read runs as the host's instance role: every AWS identity
/// selector in the daemon's environment (a worker's `AWS_PROFILE=sccache`)
/// is removed from the child. `forge.gateway.awsProfile` is the one way to
/// name another profile, and it is passed as `--profile`.
#[test]
fn ssm_reads_use_the_instance_role_unless_a_profile_is_configured() {
    let cmd = ssm_command("/p/k", "us-east-1", None);
    let removed: Vec<String> = cmd
        .get_envs()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    for key in AWS_IDENTITY_ENV {
        if crate::aws_ssm::AWS_FILE_ENV.contains(key) {
            continue; // set to /dev/null, below
        }
        assert!(removed.iter().any(|r| r == key), "{key} reaches the aws child: {removed:?}");
    }
    // With no profile the shared files are /dev/null: no `default` profile
    // in ~/.aws can stand in for the instance role.
    for key in crate::aws_ssm::AWS_FILE_ENV {
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == *key && v == Some(std::ffi::OsStr::new("/dev/null"))),
            "{key}"
        );
    }
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(!args.iter().any(|a| a.starts_with("--profile")), "{args:?}");

    let named = ssm_command("/p/k", "us-east-1", Some("relay-reader"));
    let args: Vec<String> = named
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(args.contains(&"--profile=relay-reader".to_string()), "{args:?}");
    // The profile is named on the command line, so the environment can't
    // move it either.
    assert!(named
        .get_envs()
        .any(|(k, v)| k == "AWS_PROFILE" && v.is_none()));

    let v = json!({"forge": {"gateway": {"origin": "gw.example.com", "awsProfile": "relay-reader",
        "keyRefs": {"acme": "ssm:///p"}}}});
    assert_eq!(parse(&v).0.unwrap().aws_profile.name(), Some("relay-reader"));
    let bad = json!({"forge": {"gateway": {"origin": "gw.example.com", "awsProfile": "a b",
        "keyRefs": {"acme": "ssm:///p"}}}});
    let (c, w) = parse(&bad);
    assert_eq!((c.unwrap().aws_profile, w.len()), (AwsProfile::Invalid, 1));
}

#[test]
fn an_ssm_failure_names_the_exit_and_the_first_stderr_line() {
    assert_eq!(
        first_line("\n  An error occurred (AccessDeniedException) when calling the GetParameter operation: User: arn:x is not authorized\nmore"),
        "An error occurred (AccessDeniedException) when calling the GetParameter operation: User: arn:x is not authorized"
    );
    assert_eq!(first_line(""), "(no stderr)");
    assert_eq!(first_line(&"x".repeat(1000)).len(), 240);
}
