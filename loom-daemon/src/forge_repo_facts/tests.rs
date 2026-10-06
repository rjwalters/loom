//! Resolver, fingerprint, record and cross-check coverage (W3a). Site-level
//! coverage (reapers, linked-PR probe, kill switch, ledger) is in
//! `tests_sites.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use super::base::BaseRepo;
use super::record::{load, record_key};
use super::test_support::{Env, Forge};
use super::*;

fn base(root: &Path, env: GhRepoEnv) -> BaseRepo {
    base_repo(root, env).unwrap_or_else(|| panic!("no base repo for {}", root.display()))
}

fn record_of(nwo: &str) -> super::record::Record {
    load(&record_key("github.com", nwo)).expect("a record")
}

/// One resolver case: the remotes, extra `git config` pairs, the env, and
/// what gh would answer under each rule.
struct Case {
    name: &'static str,
    remotes: &'static [(&'static str, &'static str)],
    config: &'static [(&'static str, &'static str)],
    env: &'static [(&'static str, &'static str)],
    honour: &'static str,
    ignore: &'static str,
    ambiguous_honour: bool,
    ambiguous_ignore: bool,
}

const ORIGIN: (&str, &str) = ("origin", "https://github.com/acme/app.git");

/// The table mirrors gh's own resolution (reproduced against gh 2.101; see
/// `write_scope::target`). `GH_REPO`/`LOOM_REPO` overrides, an ssh alias and
/// an `insteadOf` rewrite are the four shapes this port marks ambiguous on a
/// single-remote checkout; URL spelling alone is not.
const CASES: &[Case] = &[
    Case {
        name: "origin only",
        remotes: &[ORIGIN],
        config: &[],
        env: &[],
        honour: "acme/app",
        ignore: "acme/app",
        ambiguous_honour: false,
        ambiguous_ignore: false,
    },
    Case {
        name: "origin + upstream (a fork)",
        remotes: &[ORIGIN, ("upstream", "https://github.com/up/app.git")],
        config: &[],
        env: &[],
        honour: "up/app",
        ignore: "up/app",
        ambiguous_honour: true,
        ambiguous_ignore: true,
    },
    Case {
        name: "origin + github",
        remotes: &[ORIGIN, ("github", "git@github.com:gh/app.git")],
        config: &[],
        env: &[],
        honour: "gh/app",
        ignore: "gh/app",
        ambiguous_honour: true,
        ambiguous_ignore: true,
    },
    Case {
        name: "set-default = base on origin",
        remotes: &[ORIGIN, ("upstream", "https://github.com/up/app.git")],
        config: &[("remote.origin.gh-resolved", "base")],
        env: &[],
        honour: "acme/app",
        ignore: "acme/app",
        ambiguous_honour: true,
        ambiguous_ignore: true,
    },
    Case {
        name: "set-default = an explicit owner/repo",
        remotes: &[ORIGIN, ("upstream", "https://github.com/up/app.git")],
        config: &[("remote.upstream.gh-resolved", "pinned/tool")],
        env: &[],
        honour: "pinned/tool",
        ignore: "pinned/tool",
        ambiguous_honour: true,
        ambiguous_ignore: true,
    },
    Case {
        name: "upstream, no set-default",
        remotes: &[("upstream", "https://github.com/up/app.git"), ORIGIN],
        config: &[],
        env: &[],
        honour: "up/app",
        ignore: "up/app",
        ambiguous_honour: true,
        ambiguous_ignore: true,
    },
    Case {
        name: "GH_REPO set",
        remotes: &[ORIGIN],
        config: &[],
        env: &[("GH_REPO", "env/app")],
        honour: "env/app",
        ignore: "acme/app",
        ambiguous_honour: true,
        ambiguous_ignore: false,
    },
    Case {
        name: "LOOM_REPO set (wins over GH_REPO, as the facade maps it)",
        remotes: &[ORIGIN],
        config: &[],
        env: &[("LOOM_REPO", "loom/app"), ("GH_REPO", "env/app")],
        honour: "loom/app",
        ignore: "acme/app",
        ambiguous_honour: true,
        ambiguous_ignore: false,
    },
    Case {
        name: "scp url",
        remotes: &[("origin", "git@github.com:acme/app.git")],
        config: &[],
        env: &[],
        honour: "acme/app",
        ignore: "acme/app",
        ambiguous_honour: false,
        ambiguous_ignore: false,
    },
    Case {
        name: "ssh:// url",
        remotes: &[("origin", "ssh://git@github.com/acme/app")],
        config: &[],
        env: &[],
        honour: "acme/app",
        ignore: "acme/app",
        ambiguous_honour: false,
        ambiguous_ignore: false,
    },
    Case {
        name: "ssh alias host",
        remotes: &[("origin", "git@gh-work:acme/app.git")],
        config: &[],
        env: &[],
        honour: "acme/app",
        ignore: "acme/app",
        ambiguous_honour: true,
        ambiguous_ignore: true,
    },
];

#[test]
fn the_resolver_matches_gh_and_marks_what_it_cannot_model() {
    for case in CASES {
        let env = Env::new(case.env);
        let root = env.repo("r", case.remotes);
        for (k, v) in case.config {
            env.git(&root, &["config", k, v]);
        }
        let honour = base(&root, GhRepoEnv::Honour);
        let ignore = base(&root, GhRepoEnv::Ignore);
        assert_eq!(honour.nwo, case.honour, "{}: Honour", case.name);
        assert_eq!(ignore.nwo, case.ignore, "{}: Ignore", case.name);
        assert_eq!(honour.ambiguous, case.ambiguous_honour, "{}: Honour ambiguous", case.name);
        assert_eq!(ignore.ambiguous, case.ambiguous_ignore, "{}: Ignore ambiguous", case.name);

        // gh agrees (the fake gh answers each rule's own command with the
        // expected repo): an ambiguous root is cross-checked and then served.
        for (rule, want) in [
            (GhRepoEnv::Honour, case.honour),
            (GhRepoEnv::Ignore, case.ignore),
        ] {
            let forge = Forge::new(env.tmp.path(), want);
            match canonical_with(&forge.gh, &root, rule) {
                Lookup::Fact(f) => assert_eq!(f.full_name(), want, "{}: {rule:?}", case.name),
                other => panic!("{}: {rule:?} gave {other:?}", case.name),
            }
            std::fs::remove_dir_all(&forge.dir).unwrap();
        }
    }
}

/// An `insteadOf` rewrite in a file the global config includes changes the
/// URL git (and so gh) uses: such a root is ambiguous.
#[test]
fn an_insteadof_rewrite_in_an_included_file_is_ambiguous() {
    let env = Env::new(&[]);
    let inc = env.tmp.path().join("inc.gitconfig");
    std::fs::write(
        &inc,
        "[url \"https://github.com/newco/\"]\n\tinsteadOf = https://github.com/acme/\n",
    )
    .unwrap();
    std::fs::write(&env.global, format!("[include]\n\tpath = {}\n", inc.display())).unwrap();
    let root = env.repo("r", &[ORIGIN]);
    assert!(base(&root, GhRepoEnv::Ignore).ambiguous);
}

#[test]
fn a_root_with_no_remote_or_no_checkout_is_legacy() {
    let env = Env::new(&[]);
    let bare = env.repo("r", &[]);
    assert_eq!(base_repo(&bare, GhRepoEnv::Ignore), None);
    let forge = Forge::new(env.tmp.path(), "acme/app");
    assert_eq!(canonical_with(&forge.gh, &bare, GhRepoEnv::Ignore), Lookup::Legacy);
    let not_a_checkout = env.tmp.path().join("plain");
    std::fs::create_dir_all(&not_a_checkout).unwrap();
    assert_eq!(canonical_with(&forge.gh, &not_a_checkout, GhRepoEnv::Ignore), Lookup::Legacy);
    assert!(forge.calls().is_empty(), "no forge call without a local answer");
}

/// Every input of the resolution re-resolves; an unchanged hit forks nothing.
#[test]
fn the_config_fingerprint_sees_every_change_and_a_hit_forks_no_git() {
    let env = Env::new(&[]);
    let inc = env.tmp.path().join("inc.gitconfig");
    std::fs::write(&inc, "").unwrap();
    std::fs::write(&env.global, format!("[include]\n\tpath = {}\n", inc.display())).unwrap();
    let root = env.repo("r", &[ORIGIN]);
    assert_eq!(base(&root, GhRepoEnv::Ignore).nwo, "acme/app");

    let forks = test_git_forks();
    for _ in 0..5 {
        assert_eq!(base(&root, GhRepoEnv::Ignore).nwo, "acme/app");
    }
    assert_eq!(test_git_forks(), forks, "a memo hit restats; it never forks git");

    env.git(
        &root,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/newco/app.git",
        ],
    );
    assert_eq!(base(&root, GhRepoEnv::Ignore).nwo, "newco/app", "set-url");

    env.git(&root, &["remote", "add", "upstream", "https://github.com/up/app.git"]);
    assert_eq!(base(&root, GhRepoEnv::Ignore).nwo, "up/app", "remote add");

    env.git(&root, &["config", "remote.origin.gh-resolved", "base"]);
    assert_eq!(base(&root, GhRepoEnv::Ignore).nwo, "newco/app", "gh-resolved");

    env.git(&root, &["remote", "remove", "upstream"]);
    env.git(&root, &["config", "--unset", "remote.origin.gh-resolved"]);
    assert!(!base(&root, GhRepoEnv::Ignore).ambiguous);
    // An included file that was empty when the memo was taken.
    std::fs::write(&inc, "[url \"https://github.com/x/\"]\n\tinsteadOf = https://github.com/y/\n")
        .unwrap();
    assert!(base(&root, GhRepoEnv::Ignore).ambiguous, "edited include file");

    // The effective GH_REPO is part of the fingerprint too.
    set_test_env(Some(&[("GH_REPO", "env/app")]));
    assert_eq!(base(&root, GhRepoEnv::Honour).nwo, "env/app");
    set_test_env(Some(&[]));
    assert_eq!(base(&root, GhRepoEnv::Honour).nwo, "newco/app");
}

/// `remote_identity` (the ETag store's key) sees an origin move without a
/// restart — and with `LOOM_REPO_FACTS=0` keeps its exact legacy memo.
#[test]
fn remote_identity_follows_an_origin_move_unless_the_kill_switch_is_set() {
    let env = Env::new(&[]);
    let root = env.repo("r", &[("origin", "https://github.com/acme/x.git")]);
    let id = |r: &Path| crate::forge_etag_store::remote_identity(r).map(|(_, n)| n);
    assert_eq!(id(&root).as_deref(), Some("acme/x"));
    env.git(
        &root,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/newco/x.git",
        ],
    );
    assert_eq!(id(&root).as_deref(), Some("newco/x"));

    set_test_env(Some(&[("LOOM_REPO_FACTS", "0")]));
    let legacy = env.repo("legacy", &[("origin", "https://github.com/acme/x.git")]);
    assert_eq!(id(&legacy).as_deref(), Some("acme/x"));
    env.git(
        &legacy,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/newco/x.git",
        ],
    );
    assert_eq!(id(&legacy).as_deref(), Some("acme/x"), "legacy memo is process-lifetime");
}

#[test]
fn a_verified_record_is_reused_until_its_ttl_then_revalidated_with_its_etag() {
    let env = Env::new(&[("LOOM_REPO_FACTS_VERIFY_SECS", "100")]);
    let root = env.repo("r", &[ORIGIN]);
    let forge = Forge::new(env.tmp.path(), "acme/app");
    for _ in 0..3 {
        assert!(matches!(canonical_with(&forge.gh, &root, GhRepoEnv::Ignore), Lookup::Fact(_)));
    }
    assert_eq!(forge.repo_reads(), 1, "one verify, then the record");
    advance_test_clock(101);
    forge.set("notmodified", "");
    let Lookup::Fact(f) = canonical_with(&forge.gh, &root, GhRepoEnv::Ignore) else {
        panic!("expected a fact");
    };
    assert!(f.fresh);
    assert_eq!(forge.repo_reads(), 2);
    assert!(forge
        .calls()
        .last()
        .unwrap()
        .contains("If-None-Match: \"v1\""));
}

/// The record persists in the private store, shared with other processes.
#[test]
fn the_record_is_persisted_owner_only_and_read_back() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new(&[]);
    let store = env.tmp.path().join("store");
    crate::forge_etag_store::set_test_daemon_store_dir(Some(store.clone()));
    let root = env.repo("r", &[ORIGIN]);
    let forge = Forge::new(env.tmp.path(), "acme/app");
    assert!(matches!(canonical_with(&forge.gh, &root, GhRepoEnv::Ignore), Lookup::Fact(_)));
    let files: Vec<_> = std::fs::read_dir(&store)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let name = files[0].file_name().unwrap().to_string_lossy().to_string();
    assert!(name.starts_with("repofacts-") && name.ends_with(".json"), "{name}");
    assert_eq!(std::fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777, 0o600);
    // A fresh process (state reset) reads it back without a call.
    set_test_enabled(true);
    set_test_env(Some(&[]));
    assert!(matches!(canonical_with(&forge.gh, &root, GhRepoEnv::Ignore), Lookup::Fact(_)));
    assert_eq!(forge.repo_reads(), 1);
    crate::forge_etag_store::set_test_daemon_store_dir(None);
}

/// After a renamed/transferred repo the canonical value is the forge's (what
/// a REST 301 gives), and the redirect is warned about and counted once.
#[test]
fn a_redirect_returns_the_canonical_owner_and_is_counted_once_per_root() {
    let env = Env::new(&[]);
    let root = env.repo("r", &[ORIGIN]);
    let forge = Forge::new(env.tmp.path(), "newco/app");
    for _ in 0..3 {
        let Lookup::Fact(f) = canonical_with(&forge.gh, &root, GhRepoEnv::Honour) else {
            panic!("expected a fact");
        };
        assert_eq!((f.owner.as_str(), f.configured_nwo.as_str()), ("newco", "acme/app"));
    }
    assert_eq!(crate::forge_call_stats::counters::get(REDIRECTED), 1);
    let d = disagreement(&root).unwrap();
    assert_eq!(d.canonical_nwo.as_deref(), Some("newco/app"));
    assert_eq!(d.gh_base_nwo, "acme/app");
}

#[test]
fn observe_refreshes_on_a_match_marks_suspect_on_a_mismatch_and_is_monotonic() {
    let env = Env::new(&[]);
    let root = env.repo("r", &[ORIGIN]);
    let forge = Forge::new(env.tmp.path(), "acme/app");
    assert!(matches!(canonical_with(&forge.gh, &root, GhRepoEnv::Ignore), Lookup::Fact(_)));
    let before = record_of("acme/app");

    observe("github.com", "acme/app", "acme/app", before.response_sent_at + 50);
    let refreshed = record_of("acme/app");
    assert_eq!(refreshed.verified_at, before.verified_at + 50);

    // Older than the newest accepted answer: ignored.
    observe("github.com", "acme/app", "other/app", before.response_sent_at);
    assert!(!record_of("acme/app").suspect);

    // A mismatch casts doubt, never rewrites.
    observe("github.com", "acme/app", "newco/app", before.response_sent_at + 60);
    let doubted = record_of("acme/app");
    assert!(doubted.suspect);
    assert_eq!(doubted.full_name(), "acme/app", "never rewritten from an observation");

    // The next use re-reads (and the forge clears the doubt).
    let reads = forge.repo_reads();
    assert!(matches!(canonical_with(&forge.gh, &root, GhRepoEnv::Ignore), Lookup::Fact(_)));
    assert_eq!(forge.repo_reads(), reads + 1);
    assert!(!record_of("acme/app").suspect);
}

/// After a failed read nothing is asked for `SUSPECT_BACKOFF_SECS`, then
/// exactly one retry.
#[test]
fn a_failed_verify_backs_off_then_retries_once() {
    let env = Env::new(&[]);
    let root = env.repo("r", &[ORIGIN]);
    let forge = Forge::new(env.tmp.path(), "acme/app");
    forge.set("mode", "404");
    assert_eq!(canonical_with(&forge.gh, &root, GhRepoEnv::Ignore), Lookup::Unavailable);
    assert_eq!(forge.repo_reads(), 1);
    advance_test_clock(SUSPECT_BACKOFF_SECS - 1);
    for _ in 0..5 {
        assert_eq!(canonical_with(&forge.gh, &root, GhRepoEnv::Ignore), Lookup::Unavailable);
    }
    assert_eq!(forge.repo_reads(), 1, "no call inside the backoff");
    advance_test_clock(2);
    forge.set("mode", "ok");
    assert!(matches!(canonical_with(&forge.gh, &root, GhRepoEnv::Ignore), Lookup::Fact(_)));
    assert_eq!(forge.repo_reads(), 2, "one retry after it");
}

/// The cross-check runs the command each rule replaced: the placeholder with
/// `GH_REPO` = `LOOM_REPO` for Honour, `gh repo view` without `GH_REPO` for
/// Ignore. A disagreement pins the root to legacy and is counted.
#[test]
#[serial_test::serial]
fn the_ambiguous_cross_check_uses_each_rules_own_command_and_pins_legacy_on_disagreement() {
    std::env::set_var("LOOM_REPO", "up/app");
    let env = Env::new(&[("LOOM_REPO", "up/app")]);
    let root = env.repo("fork", &[ORIGIN, ("upstream", "https://github.com/up/app.git")]);
    let forge = Forge::new(env.tmp.path(), "up/app");
    assert!(matches!(canonical_with(&forge.gh, &root, GhRepoEnv::Honour), Lookup::Fact(_)));
    let honour = forge
        .calls()
        .into_iter()
        .find(|c| c.contains("{owner}/{repo}"))
        .unwrap();
    assert!(honour.ends_with("GH_REPO=up/app"), "{honour}");

    assert!(matches!(canonical_with(&forge.gh, &root, GhRepoEnv::Ignore), Lookup::Fact(_)));
    let ignore = forge
        .calls()
        .into_iter()
        .find(|c| c.starts_with("repo view"))
        .unwrap();
    assert!(ignore.ends_with("GH_REPO=unset"), "gh repo view never sees GH_REPO: {ignore}");

    // Once per fingerprint: no further cross-check calls.
    let before = forge.calls().len();
    for _ in 0..3 {
        canonical_with(&forge.gh, &root, GhRepoEnv::Ignore);
    }
    assert_eq!(forge.calls().len(), before);
    std::env::remove_var("LOOM_REPO");

    // A second fork checkout where gh's own answer differs.
    set_test_enabled(true);
    set_test_env(Some(&[]));
    let other = env.repo("fork2", &[ORIGIN, ("upstream", "https://github.com/up/app.git")]);
    forge.set("gh_answer", "acme/app");
    assert_eq!(canonical_with(&forge.gh, &other, GhRepoEnv::Ignore), Lookup::Legacy);
    assert_eq!(crate::forge_call_stats::counters::get(RESOLVER_DISAGREE), 1);
    // Pinned for the process: legacy without asking again.
    let calls = forge.calls().len();
    assert_eq!(canonical_with(&forge.gh, &other, GhRepoEnv::Ignore), Lookup::Legacy);
    assert_eq!(forge.calls().len(), calls);
}

#[test]
fn the_kill_switch_makes_every_lookup_legacy() {
    let env = Env::new(&[("LOOM_REPO_FACTS", "0")]);
    let root = env.repo("r", &[ORIGIN]);
    let forge = Forge::new(env.tmp.path(), "acme/app");
    assert!(!enabled());
    assert_eq!(canonical_with(&forge.gh, &root, GhRepoEnv::Honour), Lookup::Legacy);
    assert!(forge.calls().is_empty());
}

#[test]
fn repository_urls_parse_to_full_names() {
    assert_eq!(
        full_name_from_repository_url("https://api.github.com/repos/acme/app").as_deref(),
        Some("acme/app")
    );
    assert_eq!(full_name_from_repository_url("https://api.github.com/repos/acme"), None);
    assert_eq!(full_name_from_repository_url("https://x/acme/app"), None);
}

#[test]
fn config_z_output_parses_keys_values_and_valueless_keys() {
    let raw = b"file:/etc/gitconfig\0core.bare\nfalse\0file:.git/config\0remote.origin.url\nhttps://github.com/acme/app\0command line:\0flag\0";
    let entries = super::base::parse_config_z(raw);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[1].1, "remote.origin.url");
    assert_eq!(entries[1].2, "https://github.com/acme/app");
    assert_eq!(entries[2], ("command line:".into(), "flag".into(), String::new()));
}
