//! Hermetic tests for the host-owned mirror store (#14680).
//!
//! The publication/admission decisions are exercised through scripted
//! fetches and proofs built by the same hermetic verification seams as
//! `read_authority_tests` (no network). The store's git mechanics — bare
//! creation, retention across upstream ref loss, the config allowlist, and
//! the additive behavior of a failed fetch — run real `git` against
//! disposable local origins, which is exactly what the store shells out to.
use super::mirror::{
    fetch_origin, refresh_session_mirrors, MirrorFetch, MirrorStore, Refreshed, MIRROR_ROOT_ENV,
    PROTOCOL as MIRROR_PROTOCOL,
};
use super::read_authority::{self, Credential, ReadProof, Verification};
use super::*;
use crate::credential_preflight::{Minted, ScopedIssuance};
use crate::test_git_helpers::{configure, run as git};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use super::forge_auth::ProvedReader;

const GHS_A: &str = "ghs_scopedAAAA TESTONLY";
const GHS_B: &str = "ghs_scopedBBBB TESTONLY";
const REPO_URL: &str = "https://github.com/acme/widget";

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

// --- Hermetic verification seams (mirrors read_authority_tests) ----------

struct FakeHttp {
    body: String,
}

impl read_authority::ProviderHttp for FakeHttp {
    fn get(
        &self,
        _url: &str,
        _scheme: read_authority::AuthScheme,
        _accept: &str,
        _credential: &Credential,
    ) -> read_authority::ProbeAnswer {
        read_authority::ProbeAnswer::Ok(self.body.clone())
    }
}

struct FakeMinter {
    repository_id: u64,
    installation: &'static str,
    token: &'static str,
    expires_at: &'static str,
}

impl FakeMinter {
    fn default_for(id: u64) -> Self {
        Self {
            repository_id: id,
            installation: "151241341",
            token: GHS_A,
            expires_at: "2035-01-01T00:00:00Z",
        }
    }
}

impl read_authority::ScopedMinter for FakeMinter {
    fn mint_scoped(&self, _owner_repo: &str) -> read_authority::ScopedOutcome {
        read_authority::ScopedOutcome::Minted(Minted::read_authority(
            self.token.to_owned(),
            "4486636".to_owned(),
            self.installation.to_owned(),
            "acme".to_owned(),
            self.expires_at.to_owned(),
            ScopedIssuance {
                repository_ids: vec![self.repository_id.to_string()],
                permissions: [("contents".to_owned(), "read".to_owned())]
                    .into_iter()
                    .collect(),
            },
        ))
    }
}

/// A proved GitHub read authority for `REPO_URL`, entirely hermetic.
fn proved(minter: &FakeMinter, repository_id: u64) -> ReadProof {
    let http = FakeHttp {
        body: format!(r#"{{"id": {repository_id}, "private": true}}"#),
    };
    match read_authority::verify(
        &read_authority::VerificationRequest {
            url: REPO_URL.to_owned(),
            github_host: None,
            gitea_credential: None,
        },
        &http,
        minter,
        at(1_800_000_000),
    ) {
        Verification::Proved(proof) => proof,
        Verification::Unproved(reason) => panic!("expected a proof, got {reason:?}"),
    }
}

fn default_proof() -> ReadProof {
    proved(&FakeMinter::default_for(907216439), 907216439)
}

fn id() -> RepoId {
    identify(REPO_URL).unwrap()
}

/// A scripted fetch leg: records whether it ran, never touches git.
struct RecordingFetch {
    ran: Mutex<Vec<String>>,
}

impl RecordingFetch {
    fn ok() -> Self {
        Self {
            ran: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.ran.lock().unwrap().len()
    }
}

impl MirrorFetch for RecordingFetch {
    fn fetch(&self, _git_dir: &Path, reader: &ProvedReader) -> Result<()> {
        self.ran.lock().unwrap().push(reader.url().to_owned());
        Ok(())
    }
}

fn store(root: &Path) -> MirrorStore {
    MirrorStore::open(root, &id()).unwrap()
}

fn store_after_marker(root: &Path) -> MirrorStore {
    let id = id();
    let dir = root.join(&id.slug);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mirror.json"),
        serde_json::json!({"protocol": MIRROR_PROTOCOL, "key": id.key}).to_string(),
    )
    .unwrap();
    store(root)
}

// --- Creation, pinning, convergence --------------------------------------

#[test]
#[serial_test::serial(private_workspace_fork)]
fn a_fresh_store_is_created_bare_retention_first_and_pinned_to_the_first_proof() {
    let _fork = tests::fork_guard();
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let proof = default_proof();
    assert_eq!(
        store
            .refresh_with(&proof, at(1_800_000_000), &RecordingFetch::ok())
            .unwrap(),
        Refreshed::Created
    );
    // Bare mirror with retention-first configuration.
    let git_dir = store.git_dir();
    assert!(git_dir.join("HEAD").is_file());
    assert!(git_dir.join("objects").is_dir());
    let config = std::fs::read_to_string(git_dir.join("config")).unwrap();
    assert!(config.contains("bare = true"));
    assert!(config.contains("auto = 0"), "automatic gc must be disabled");
    assert!(config.contains("prune = false"), "fetch pruning must be off");
    // The FIRST proved grant is pinned; no credential byte is stored.
    let pinned = store.pinned_grant().unwrap().unwrap();
    assert_eq!(pinned, proof.grant().clone());
    let grant_text = std::fs::read_to_string(store.dir().join("grant.json")).unwrap();
    assert!(!grant_text.contains(GHS_A), "the credential must never be persisted");
}

#[test]
#[serial_test::serial(private_workspace_fork)]
fn a_second_seat_converges_on_the_existing_store_and_a_different_authority_is_refused() {
    let _fork = tests::fork_guard();
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    store
        .refresh_with(&default_proof(), at(1_800_000_000), &RecordingFetch::ok())
        .unwrap();
    // Rotation under the SAME authority (a fresh credential, same
    // installation) refreshes the same store.
    let rotating = FakeMinter {
        token: GHS_B,
        ..FakeMinter::default_for(907216439)
    };
    let fetch = RecordingFetch::ok();
    assert_eq!(
        store
            .refresh_with(&proved(&rotating, 907216439), at(1_800_000_100), &fetch)
            .unwrap(),
        Refreshed::Fetched
    );
    assert_eq!(fetch.calls(), 1);
    // A DIFFERENT authority (another installation at the same slug) never
    // inherits the store: publication is withheld and nothing is fetched.
    let other_installation = FakeMinter {
        installation: "999999999",
        ..FakeMinter::default_for(907216439)
    };
    let fetch = RecordingFetch::ok();
    let refused = store
        .refresh_with(&proved(&other_installation, 907216439), at(1_800_000_200), &fetch)
        .unwrap_err();
    assert!(
        refused.to_string().contains("DifferentAuthority"),
        "a different authority must be named: {refused}"
    );
    assert_eq!(fetch.calls(), 0, "a withheld publication must not fetch");
    // The store is still the first authority's, and still usable.
    assert_eq!(store.pinned_grant().unwrap().unwrap(), default_proof().grant().clone());
}

#[test]
#[serial_test::serial(private_workspace_fork)]
fn a_reused_slug_whose_repository_id_moved_is_a_replacement_not_this_store() {
    let _fork = tests::fork_guard();
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    store
        .refresh_with(&default_proof(), at(1_800_000_000), &RecordingFetch::ok())
        .unwrap();
    let replacement = FakeMinter::default_for(424242);
    let fetch = RecordingFetch::ok();
    let refused = store
        .refresh_with(&proved(&replacement, 424242), at(1_800_000_300), &fetch)
        .unwrap_err();
    assert!(
        refused.to_string().contains("DifferentRepository"),
        "a moved repository id must read as a replacement repository: {refused}"
    );
    assert_eq!(fetch.calls(), 0);
}

#[test]
#[serial_test::serial(private_workspace_fork)]
fn an_interrupted_creation_is_completed_by_a_repeat_and_never_half_kept() {
    let _fork = tests::fork_guard();
    let proof = default_proof();
    struct RetentionCheckingFetch(RecordingFetch);
    impl MirrorFetch for RetentionCheckingFetch {
        fn fetch(&self, git_dir: &Path, reader: &ProvedReader) -> Result<()> {
            let config = std::fs::read_to_string(git_dir.join("config"))?;
            assert!(config.contains("\tauto = 0\n"));
            assert!(config.contains("\tprune = false\n"));
            assert!(config.contains("\tprunetags = false\n"));
            self.0.fetch(git_dir, reader)
        }
    }

    for initial_config in [
        None,
        Some("[core]\n\tbare = true\n"),
        Some("[core]\n\tbare = true\n[gc]\n\tauto = 0\n[fetch]\n\tprune = false\n"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let store = store_after_marker(root.path());
        if let Some(config) = initial_config {
            // Synthetic config from a later interruption, before pinning the grant.
            let git_dir = store.git_dir();
            std::fs::create_dir(&git_dir).unwrap();
            std::fs::write(git_dir.join("config"), config).unwrap();
        }
        let fetch = RetentionCheckingFetch(RecordingFetch::ok());
        assert_eq!(
            store
                .refresh_with(&proof, at(1_800_000_000), &fetch)
                .unwrap(),
            Refreshed::Fetched
        );
        assert_eq!(fetch.0.calls(), 1);
        assert_eq!(store.pinned_grant().unwrap().unwrap(), proof.grant().clone());
    }
}

#[test]
#[serial_test::serial(private_workspace_fork)]
fn an_existing_config_with_conflicting_retention_refuses_before_the_fetch_leg() {
    let _fork = tests::fork_guard();
    let root = tempfile::tempdir().unwrap();
    let store = store_after_marker(root.path());
    let git_dir = store.git_dir();
    std::fs::create_dir(&git_dir).unwrap();
    let path = git_dir.join("config");
    let fetch = RecordingFetch::ok();
    let proof = default_proof();
    for config in [
        "[core]\n\tbare = true\n[fetch]\n\tprune = true\n",
        "[core]\n\tbare = true\n[gc]\n\tauto = 0\n\tauto = 0\n",
    ] {
        std::fs::write(&path, config).unwrap();
        assert!(store
            .refresh_with(&proof, at(1_800_000_000), &fetch)
            .is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), config);
        assert_eq!(fetch.calls(), 0);
        assert!(store.pinned_grant().unwrap().is_none());
    }
}

#[test]
#[serial_test::serial(private_workspace_fork)]
fn a_foreign_directory_at_the_slug_is_refused_never_adopted() {
    let _fork = tests::fork_guard();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join(id().slug);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("someone-elses"), "work").unwrap();
    let refused = MirrorStore::open(root.path(), &id()).err().unwrap();
    assert!(refused.to_string().contains("retain it for recovery"));
    // A marker for another repository is equally refused.
    std::fs::remove_file(dir.join("someone-elses")).unwrap();
    std::fs::write(
        dir.join("mirror.json"),
        serde_json::to_string(&serde_json::json!({
            "protocol": MIRROR_PROTOCOL,
            "key": "github.com/other/repo",
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(MirrorStore::open(root.path(), &id()).is_err());
}

// --- Publication and admission decisions ----------------------------------

#[test]
#[serial_test::serial(private_workspace_fork)]
fn an_expired_proof_withholds_publication_without_touching_the_store() {
    let _fork = tests::fork_guard();
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    store
        .refresh_with(&default_proof(), at(1_800_000_000), &RecordingFetch::ok())
        .unwrap();
    // A proof verified earlier whose credential lifetime has since ended
    // (mint expiry 2035-01-01 is epoch 2051222400).
    let stale = proved(&FakeMinter::default_for(907216439), 907216439);
    let fetch = RecordingFetch::ok();
    let refused = store
        .refresh_with(&stale, at(2_600_000_000), &fetch)
        .unwrap_err();
    assert!(refused.to_string().contains("Expired"), "got {refused}");
    assert_eq!(fetch.calls(), 0);
}

#[test]
#[serial_test::serial(private_workspace_fork)]
fn publication_is_withheld_when_a_borrower_lost_or_hides_its_authority() {
    let _fork = tests::fork_guard();
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let proof = default_proof();
    store
        .refresh_with(&proof, at(1_800_000_000), &RecordingFetch::ok())
        .unwrap();
    store.attach("seat-a", &proof, at(1_800_000_050)).unwrap();
    // A borrower whose record cannot be parsed is MISSING, not fine.
    std::fs::write(store.dir().join("borrowers").join("seat-a.json"), "not json").unwrap();
    let fetch = RecordingFetch::ok();
    let refused = store
        .refresh_with(&proof, at(1_800_000_100), &fetch)
        .unwrap_err();
    assert!(refused.to_string().contains("BorrowerStateMissing"), "got {refused}");
    assert_eq!(fetch.calls(), 0);
    // A borrower pinning a different grant withholds new bytes.
    let other = FakeMinter {
        installation: "999999999",
        ..FakeMinter::default_for(907216439)
    };
    let other_grant = proved(&other, 907216439).grant().clone();
    std::fs::write(
        store.dir().join("borrowers").join("seat-a.json"),
        serde_json::to_string(&serde_json::json!({ "grant": other_grant })).unwrap(),
    )
    .unwrap();
    let refused = store
        .refresh_with(&proof, at(1_800_000_200), &RecordingFetch::ok())
        .unwrap_err();
    assert!(refused.to_string().contains("BorrowerAuthorityLost"), "got {refused}");
    // Retiring the borrower record restores publication.
    store.detach("seat-a").unwrap();
    assert_eq!(
        store
            .refresh_with(&proof, at(1_800_000_300), &RecordingFetch::ok())
            .unwrap(),
        Refreshed::Fetched
    );
}

#[test]
#[serial_test::serial(private_workspace_fork)]
fn admission_requires_a_pinned_grant_and_the_same_authority() {
    let _fork = tests::fork_guard();
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let proof = default_proof();
    // A store that pins no grant admits nobody.
    let refused = store
        .attach("seat-a", &proof, at(1_800_000_000))
        .unwrap_err();
    assert!(refused.to_string().contains("StoreNotProved"), "got {refused}");
    store
        .refresh_with(&proof, at(1_800_000_000), &RecordingFetch::ok())
        .unwrap();
    // Rotation (a fresh credential, same authority) admits.
    let rotating = FakeMinter {
        token: GHS_B,
        ..FakeMinter::default_for(907216439)
    };
    store
        .attach("seat-a", &proved(&rotating, 907216439), at(1_800_000_100))
        .unwrap();
    assert!(store.dir().join("borrowers").join("seat-a.json").is_file());
    // A different authority is refused admission and writes nothing.
    let other = FakeMinter {
        installation: "999999999",
        ..FakeMinter::default_for(907216439)
    };
    let refused = store
        .attach("seat-b", &proved(&other, 907216439), at(1_800_000_200))
        .unwrap_err();
    assert!(refused.to_string().contains("DifferentAuthority"), "got {refused}");
    assert!(!store.dir().join("borrowers").join("seat-b.json").exists());
    // An unsafe account name never becomes a path component.
    store
        .attach("../escape", &proof, at(1_800_000_300))
        .unwrap();
    let names: Vec<String> = std::fs::read_dir(store.dir().join("borrowers"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"seat-a.json".to_owned()));
    assert_eq!(names.len(), 2, "the unsafe name hashed to one record: {names:?}");
    assert!(
        names
            .iter()
            .all(|name| name != "escape.json" && name != "../escape.json"),
        "the unsafe account name must not name its record: {names:?}"
    );
}

// --- Real git mechanics ----------------------------------------------------

/// A disposable local origin with one commit on `main`, plus its seed clone.
struct Origin {
    temp: tempfile::TempDir,
    path: PathBuf,
}

impl Origin {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("origin");
        std::fs::create_dir(&path).unwrap();
        git(&path, &["init", "-b", "main", "--bare"]);
        let seed = temp.path().join("seed");
        std::fs::create_dir(&seed).unwrap();
        git(&seed, &["init", "-b", "main"]);
        configure(&seed);
        std::fs::write(seed.join("fixture"), "base").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-m", "base"]);
        git(&seed, &["remote", "add", "origin", path.to_str().unwrap()]);
        git(&seed, &["push", "-q", "origin", "main"]);
        Self { temp, path }
    }

    fn tip(&self) -> String {
        git(&self.path, &["rev-parse", "refs/heads/main"])
    }
}

/// A minimal local fetch environment for the mechanics tests: the store's
/// transport hardening minus the credential (a local path uses none).
fn local_env() -> Vec<(String, String)> {
    vec![
        ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
        ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
        ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
    ]
}

#[test]
#[serial_test::serial(private_workspace_fork)]
fn a_real_fetch_borrows_published_objects_and_retains_them_across_upstream_loss() {
    let _fork = tests::fork_guard();
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    store
        .refresh_with(&default_proof(), at(1_800_000_000), &RecordingFetch::ok())
        .unwrap();
    let origin = Origin::new();
    let url = origin.path.to_string_lossy().into_owned();
    fetch_origin(&store.git_dir(), &url, &local_env(), None).unwrap();
    let tip = origin.tip();
    assert_eq!(git(&store.git_dir(), &["rev-parse", "refs/heads/main"]), tip);
    assert!(store.objects_bytes().unwrap() > 0);
    // Upstream moves on and the old branch disappears entirely: the mirror
    // keeps both tips and its copy of the deleted branch's ref (no prune,
    // no gc — operator ruling 2026-11-19, condition 3).
    let seed = origin.temp.path().join("seed");
    std::fs::write(seed.join("fixture"), "second").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-m", "second"]);
    git(&seed, &["push", "-q", "--force", "origin", "main:gone"]);
    git(&origin.path, &["update-ref", "-d", "refs/heads/main"]);
    fetch_origin(&store.git_dir(), &url, &local_env(), None).unwrap();
    git(&store.git_dir(), &["cat-file", "-e", &format!("{tip}^{{commit}}")]);
    assert_eq!(
        git(&store.git_dir(), &["rev-parse", "refs/heads/gone"]),
        git(&seed, &["rev-parse", "HEAD"])
    );
    assert_eq!(git(&store.git_dir(), &["rev-parse", "refs/heads/main"]), tip);
}

#[test]
#[serial_test::serial(private_workspace_fork)]
fn a_failed_fetch_leaves_the_last_usable_store_intact() {
    let _fork = tests::fork_guard();
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let proof = default_proof();
    store
        .refresh_with(&proof, at(1_800_000_000), &RecordingFetch::ok())
        .unwrap();
    let origin = Origin::new();
    let url = origin.path.to_string_lossy().into_owned();
    fetch_origin(&store.git_dir(), &url, &local_env(), None).unwrap();
    let tip = origin.tip();
    // A fetch from an origin that does not resolve fails, and the store
    // keeps serving everything it already holds.
    assert!(fetch_origin(
        &store.git_dir(),
        &origin.temp.path().join("vanished").to_string_lossy(),
        &local_env(),
        None,
    )
    .is_err());
    git(&store.git_dir(), &["cat-file", "-e", &format!("{tip}^{{commit}}")]);
    assert_eq!(store.pinned_grant().unwrap().unwrap(), proof.grant().clone());
}

#[test]
#[serial_test::serial(private_workspace_fork)]
fn a_config_this_module_never_wrote_refuses_the_fetch_before_git_runs() {
    let _fork = tests::fork_guard();
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    store
        .refresh_with(&default_proof(), at(1_800_000_000), &RecordingFetch::ok())
        .unwrap();
    let origin = Origin::new();
    let url = origin.path.to_string_lossy().into_owned();
    fetch_origin(&store.git_dir(), &url, &local_env(), None).unwrap();
    // Every injection channel the proved environment does not mask: an
    // insteadOf rewrite, an http proxy, a credential helper, an unknown
    // remote. Each refuses the fetch, with the key (never its value) named.
    for poison in [
        "[url \"https://attacker.example/\"]\n\tinsteadOf = https://github.com\n",
        "[http]\n\tproxy = http://attacker.example\n",
        "[credential]\n\thelper = !leak\n",
        "[remote \"origin\"]\n\turl = https://attacker.example/acme/widget\n",
    ] {
        let mut config = std::fs::read_to_string(store.git_dir().join("config")).unwrap();
        config.push_str(poison);
        std::fs::write(store.git_dir().join("config"), &config).unwrap();
        let refused = fetch_origin(&store.git_dir(), &url, &local_env(), None).unwrap_err();
        assert!(
            refused.to_string().contains("never wrote"),
            "poison {poison:?} must refuse: {refused}"
        );
        std::fs::write(store.git_dir().join("config"), config.strip_suffix(poison).unwrap())
            .unwrap();
    }
    // The clean config still fetches: the gate refuses only what it should.
    fetch_origin(&store.git_dir(), &url, &local_env(), None).unwrap();
}

// --- Session wiring --------------------------------------------------------

#[test]
#[serial_test::serial(private_workspace_fork)]
fn refreshing_a_session_skips_unproved_mirrors_without_creating_anything() {
    let _fork = tests::fork_guard();
    let root = tempfile::tempdir().unwrap();
    // This workspace ships no GitHub App issuer script, so the GitHub route
    // is unproved and the wiring must skip quietly: no mirror directory, no
    // error, nothing touched.
    std::env::set_var(MIRROR_ROOT_ENV, root.path());
    let config = Config {
        schema_version: 2,
        account: "account".into(),
        container: "session".into(),
        engine: "engine".into(),
        docker_desktop: false,
        repository: REPO_URL.into(),
        base: "main".into(),
        repositories: vec![Registered {
            repository: "https://github.com/acme/other".into(),
            base: "main".into(),
            slug: identify("https://github.com/acme/other").unwrap().slug,
        }],
        volume: "private-volume".into(),
        profile: "/external/account".into(),
        gh_config: None,
        session_image: None,
        extra_mounts: Vec::new(),
    };
    refresh_session_mirrors(root.path(), &config);
    std::env::remove_var(MIRROR_ROOT_ENV);
    assert!(
        !root.path().join(id().slug).exists(),
        "no mirror may be created for an unproved route"
    );
    assert!(!root
        .path()
        .join(identify("https://github.com/acme/other").unwrap().slug)
        .exists());
    assert!(root.path().read_dir().unwrap().next().is_none());
}
