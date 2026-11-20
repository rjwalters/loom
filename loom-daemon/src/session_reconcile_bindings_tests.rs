//! #14111: declared account → private-clone bindings. The pass is driven with
//! closures standing in for `private_workspace`, so no Docker is needed; the
//! HTTPS/base validation exercised by `parse` is the real one.

use super::*;
use crate::session_reconcile::BACKOFF_BASE_SECS;
use crate::tokens_pool::account_registry::{
    AccountDescriptor, AccountId, AccountProvider, CredentialKind, InventoryProvenance,
};
use crate::tokens_pool::session_hold;
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use BindingReadiness::{Missing, Ready, Unverified};

const URL: &str = "https://github.com/acme/widgets";
const OTHER: &str = "https://github.com/acme/gadgets";

fn config(clones: &Value) -> Value {
    json!({"autonomous": {"sessionReconcile": {"privateClones": clones}}})
}

/// #14699: like [`config`], with `privateCloneSets` declared too.
fn config_with_sets(sets: &Value, clones: &Value) -> Value {
    json!({"autonomous": {"sessionReconcile": {"privateCloneSets": sets, "privateClones": clones}}})
}

fn descriptor(name: &str, profile: &Path, enabled: bool) -> AccountDescriptor {
    AccountDescriptor {
        id: AccountId {
            provider: AccountProvider::Codex,
            name: name.into(),
        },
        credential_kind: CredentialKind::CodexHome,
        credential_reference: profile.to_path_buf(),
        enabled,
        provenance: InventoryProvenance::Repo,
        email: None,
    }
}

fn index_of(accounts: &[AccountDescriptor]) -> AccountIndex {
    AccountIndex::from_inventories(&[accounts])
}

type Answer = Box<dyn FnMut(&Binding) -> anyhow::Result<Converged>>;

/// The calls the pass made to `converge`, and what each answers.
struct Fake {
    calls: RefCell<Vec<(String, String)>>,
    answer: RefCell<Answer>,
    registered: RefCell<HashMap<String, Vec<(String, String)>>>,
}

impl Fake {
    fn new(answer: impl FnMut(&Binding) -> anyhow::Result<Converged> + 'static) -> Self {
        Self {
            calls: RefCell::default(),
            answer: RefCell::new(Box::new(answer)),
            registered: RefCell::default(),
        }
    }
    fn run(
        &self,
        declared: &Declared,
        index: &AccountIndex,
        state: &mut ReconcileState,
        now: u64,
    ) -> Pass {
        self.run_ready(declared, index, state, now, &mut |_, _| Unverified)
    }

    fn run_ready(
        &self,
        declared: &Declared,
        index: &AccountIndex,
        state: &mut ReconcileState,
        now: u64,
        ready: &mut dyn FnMut(&str, &WaitReason) -> BindingReadiness,
    ) -> Pass {
        let converge = |binding: &Binding, _hold: &dyn Fn() -> anyhow::Result<()>| {
            self.calls
                .borrow_mut()
                .push((binding.account.clone(), binding.repository.clone()));
            (self.answer.borrow_mut())(binding)
        };
        let registered = |account: &str| Ok(self.registered.borrow().get(account).cloned());
        converge_pass(
            declared,
            &mut Inputs {
                index,
                converge: &converge,
                ready,
                registered: &registered,
            },
            state,
            now,
        )
    }
    fn calls(&self) -> usize {
        self.calls.borrow().len()
    }
}

#[test]
fn a_new_account_wakes_every_binding_during_the_capped_backoff() {
    let declared = parse(&config(&json!({
        "alice": [{"repository": URL}, {"repository": OTHER}],
    })));
    let present = Rc::new(Cell::new(false));
    let account_exists = Rc::clone(&present);
    let fake = Fake::new(move |_| {
        if account_exists.get() {
            Ok(Converged::Registered)
        } else {
            Err(AccountAbsent::ActingRoots.into())
        }
    });
    let mut state = ReconcileState::default();
    let index = index_of(&[]);
    let mut now = 1_000;
    for delay in [120, 240, 480, 960, 1_800] {
        let outcomes = outcomes(&fake.run(&declared, &index, &mut state, now));
        assert!(outcomes
            .iter()
            .all(|outcome| matches!(outcome, Outcome::Failed { .. })));
        now += delay;
        assert!(state.bindings.memory.values().all(|m| m.retry_at == now));
    }
    let calls = fake.calls();
    let pass = fake.run(&declared, &index, &mut state, now - 1_799);
    assert_eq!(
        outcomes(&pass),
        [
            Outcome::BackingOff { retry_at: now },
            Outcome::BackingOff { retry_at: now }
        ]
    );
    assert_eq!(fake.calls(), calls);

    present.set(true);
    let pass =
        fake.run_ready(&declared, &index, &mut state, now - 1_798, &mut |account, reason| {
            assert_eq!(account, "alice");
            assert_eq!(reason, &WaitReason::AccountAbsent);
            Ready
        });
    assert_eq!(outcomes(&pass), [Outcome::Registered, Outcome::Registered]);
    assert_eq!(fake.calls(), calls + 2);
}

#[test]
fn absent_account_wait_transitions_to_the_recorded_seat_and_retains_a_raced_deadline() {
    let declared = parse(&config(&json!({"alice": [{"repository": URL}]})));
    let private = private_workspace::Config {
        schema_version: 1,
        account: "alice".into(),
        container: "seat-alice".into(),
        engine: "engine".into(),
        docker_desktop: false,
        repository: URL.into(),
        base: "main".into(),
        repositories: Vec::new(),
        volume: "volume-alice".into(),
        profile: "/fixture/alice".into(),
        gh_config: None,
        session_image: None,
        extra_mounts: Vec::new(),
    };
    let missing = ConfiguredSeatMissing::of(&private);
    let expected = missing.clone();
    let phase = Rc::new(Cell::new(0));
    let current = Rc::clone(&phase);
    let fake = Fake::new(move |_| match current.get() {
        0 => Err(AccountAbsent::ActingRoots.into()),
        1 => Err(missing.clone().into()),
        3 => Err(AccountAbsent::SelectedRoot.into()),
        _ => Ok(Converged::Registered),
    });
    let mut state = ReconcileState::default();
    let index = index_of(&[]);
    fake.run(&declared, &index, &mut state, 1_000);
    let account_ready = &mut |_: &str, wait: &WaitReason| {
        if *wait == WaitReason::AccountAbsent {
            Ready
        } else {
            Unverified
        }
    };
    phase.set(3);
    // The selected root no longer lists the account when convergence re-reads it.
    fake.run_ready(&declared, &index, &mut state, 1_005, account_ready);
    assert!(state.bindings.memory.values().all(|m| m.retry_at == 1_120));
    for (now, observed) in [
        (1_006, Ready),
        (1_007, Unverified),
        (1_008, Ready),
        (1_009, Missing),
    ] {
        let skipped = fake.run_ready(&declared, &index, &mut state, now, &mut |_, _| observed);
        assert_eq!(outcomes(&skipped), [Outcome::BackingOff { retry_at: 1_120 }]);
        assert_eq!(fake.calls(), 2, "only one attempt per positive observation");
    }
    phase.set(1);
    fake.run_ready(&declared, &index, &mut state, 1_010, account_ready);
    let retry_at = 1_250;
    let skipped = fake.run_ready(&declared, &index, &mut state, 1_020, account_ready);
    assert_eq!(outcomes(&skipped), [Outcome::BackingOff { retry_at }]);
    assert_eq!(fake.calls(), 3);

    let seat_ready = &mut |_: &str, wait: &WaitReason| {
        if *wait == WaitReason::ConfiguredSeatMissing(expected.clone()) {
            Ready
        } else {
            Unverified
        }
    };
    // A matching observation raced a disappearance before convergence.
    fake.run_ready(&declared, &index, &mut state, 1_030, seat_ready);
    assert!(state
        .bindings
        .memory
        .values()
        .all(|m| m.retry_at == retry_at));
    let skipped = fake.run_ready(&declared, &index, &mut state, 1_031, seat_ready);
    assert_eq!(outcomes(&skipped), [Outcome::BackingOff { retry_at }]);
    assert_eq!(fake.calls(), 4);
    fake.run_ready(&declared, &index, &mut state, 1_032, &mut |_, _| Missing);
    phase.set(2);
    let done = fake.run_ready(&declared, &index, &mut state, 1_040, seat_ready);
    assert_eq!(outcomes(&done), [Outcome::Registered]);
}

#[test]
#[serial_test::serial]
fn unavailable_profile_root_does_not_rearm_an_account_retry() {
    use crate::tokens_pool::account_registry::codex_inventory_observed;
    use crate::tokens_pool::profile_root_env::ProfileRootEnv;
    use std::fs;
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let profiles = host.path().join("profiles");
    let unavailable = host.path().join("unavailable");
    let root = host.path().join("declared-root");
    fs::create_dir(&profiles).unwrap();
    symlink(&profiles, &root).unwrap();
    let _root = ProfileRootEnv::set(&root);

    fs::create_dir(workspace.path().join(".loom")).unwrap();
    fs::write(
        workspace.path().join(".loom/accounts.json"),
        r#"{"version":1,"accounts":[{"provider":"codex","name":"alice","credential_kind":"codex_home","credential_reference":"alice","enabled":true}]}"#,
    )
    .unwrap();
    let mut observe = |_: &str, _: &WaitReason| {
        let (accounts, complete) = codex_inventory_observed(workspace.path()).unwrap();
        if accounts.iter().any(|account| account.id.name == "alice") {
            Ready
        } else if complete {
            Missing
        } else {
            Unverified
        }
    };

    let declared = parse(&config(&json!({"alice": [{"repository": URL}]})));
    let fake = Fake::new(|_| Err(AccountAbsent::SelectedRoot.into()));
    let index = index_of(&[]);
    let mut state = ReconcileState::default();
    assert_eq!(observe("alice", &WaitReason::AccountAbsent), Missing);
    fake.run(&declared, &index, &mut state, 1_000);

    fs::create_dir(profiles.join("alice")).unwrap();
    assert_eq!(observe("alice", &WaitReason::AccountAbsent), Ready);
    fake.run_ready(&declared, &index, &mut state, 1_005, &mut observe);
    assert_eq!(fake.calls(), 2);

    fs::rename(&profiles, &unavailable).unwrap();
    let outage = observe("alice", &WaitReason::AccountAbsent);
    fake.run_ready(&declared, &index, &mut state, 1_006, &mut observe);

    fs::rename(&unavailable, &profiles).unwrap();
    assert_eq!(observe("alice", &WaitReason::AccountAbsent), Ready);
    let still_waiting = fake.run_ready(&declared, &index, &mut state, 1_007, &mut observe);
    assert_eq!((outage, fake.calls()), (Unverified, 2));
    assert_eq!(outcomes(&still_waiting), [Outcome::BackingOff { retry_at: 1_120 }]);

    fake.run_ready(&declared, &index, &mut state, 1_120, &mut observe);
    assert_eq!(fake.calls(), 3, "the scheduled retry still runs");
}

#[test]
fn an_already_consumed_positive_cannot_hold_the_binding_past_its_deadline() {
    let declared = parse(&config(&json!({"alice": [{"repository": URL}]})));
    let fake = Fake::new(|_| Err(AccountAbsent::SelectedRoot.into()));
    let index = index_of(&[]);
    let mut state = ReconcileState::default();
    fake.run(&declared, &index, &mut state, 1_000);
    fake.run_ready(&declared, &index, &mut state, 1_001, &mut |_, _| Ready);
    let skipped = fake.run_ready(&declared, &index, &mut state, 1_119, &mut |_, _| Ready);
    assert_eq!(outcomes(&skipped), [Outcome::BackingOff { retry_at: 1_120 }]);
    assert_eq!(fake.calls(), 2);
    fake.run_ready(&declared, &index, &mut state, 1_120, &mut |_, _| {
        panic!("a due retry does not need a readiness probe")
    });
    assert_eq!(fake.calls(), 3);
    assert!(state.bindings.memory.values().all(|m| m.retry_at == 1_360));
}

#[test]
fn unchanged_generic_backoff_logs_once_per_seat_and_is_not_probed_early() {
    let both = parse(&config(&json!({
        "alice": [{"repository": URL}, {"repository": OTHER}],
    })));
    let first = Declared {
        bindings: both.bindings[..1].to_vec(),
        invalid: Vec::new(),
    };
    let second = Declared {
        bindings: both.bindings[1..].to_vec(),
        invalid: Vec::new(),
    };
    let fake = Fake::new(|_| anyhow::bail!("raw registration error"));
    let index = index_of(&[]);
    let mut state = ReconcileState::default();
    let log = crate::test_log_capture::capture_logs(|| {
        fake.run(&first, &index, &mut state, 1_000);
        fake.run(&second, &index, &mut state, 1_010);
        for now in [1_011, 1_020, 1_119] {
            let pass = fake.run_ready(&both, &index, &mut state, now, &mut |_, _| {
                panic!("an ordinary failure must not poll for readiness")
            });
            assert_eq!(
                outcomes(&pass),
                [
                    Outcome::BackingOff { retry_at: 1_120 },
                    Outcome::BackingOff { retry_at: 1_130 }
                ]
            );
        }
        assert_eq!(fake.calls(), 2);
        fake.run(&both, &index, &mut state, 1_120);
        assert_eq!(fake.calls(), 3, "only the first binding is due");
        fake.run(&both, &index, &mut state, 1_130);
        assert_eq!(fake.calls(), 4, "the second kept its own deadline");
        fake.run(&both, &index, &mut state, 1_131);
    });
    let messages: Vec<_> = log
        .iter()
        .filter(|(_, message)| message.contains("skipping binding retry"))
        .collect();
    assert_eq!(messages.len(), 2);
    for ((level, text), remaining) in messages.into_iter().zip([119, 230]) {
        assert_eq!(*level, log::Level::Info);
        assert!(text.contains(&format!(
            "alice: skipping binding retry (binding_failure); {remaining}s remaining"
        )));
        assert!(!text.contains("raw registration error"));
    }
}

#[test]
fn a_newly_available_busy_account_defers_without_the_old_backoff() {
    let declared = parse(&config(&json!({"alice": [{"repository": URL}]})));
    let phase = Rc::new(Cell::new(0));
    let current = Rc::clone(&phase);
    let fake = Fake::new(move |_| match current.get() {
        0 => Err(AccountAbsent::ActingRoots.into()),
        1 => Err(AccountBusy("busy fixture").into()),
        _ => Ok(Converged::Registered),
    });
    let index = index_of(&[]);
    let mut state = ReconcileState::default();
    fake.run(&declared, &index, &mut state, 1_000);
    phase.set(1);
    let deferred = fake.run_ready(&declared, &index, &mut state, 1_001, &mut |_, _| Ready);
    assert_eq!(outcomes(&deferred), [Outcome::Deferred]);
    phase.set(2);
    let done = fake.run(&declared, &index, &mut state, 1_002);
    assert_eq!(outcomes(&done), [Outcome::Registered]);
}

fn outcomes(pass: &Pass) -> Vec<Outcome> {
    pass.reports.iter().map(|r| r.outcome.clone()).collect()
}

#[test]
fn bindings_parse_from_any_tier_with_a_default_base() {
    let declared = parse(&config(&json!({
        "alice": [{"repository": URL}, {"repository": OTHER, "base": "develop"}],
    })));
    assert!(declared.invalid.is_empty(), "{:?}", declared.invalid);
    let got: Vec<_> = declared
        .bindings
        .iter()
        .map(|b| (b.account.as_str(), b.repository.as_str(), b.base.as_str()))
        .collect();
    assert_eq!(got, [("alice", URL, "main"), ("alice", OTHER, "develop")]);
}

#[test]
fn no_key_means_nothing_declared() {
    assert_eq!(parse(&json!({})), Declared::default());
    assert_eq!(
        parse(&json!({"autonomous": {"sessionReconcile": {"enabled": true}}})),
        Declared::default()
    );
}

#[test]
fn an_invalid_entry_names_the_key_and_is_never_applied() {
    let declared = parse(&config(&json!({
        "alice": [
            {"repository": "http://github.com/acme/widgets"},
            {"repository": "ssh://git@github.com/acme/widgets"},
            {"repository": "https://user:secret@github.com/acme/widgets"},
            {"base": "main"},
            {"repository": URL, "base": "--upload-pack=x"},
            {"repository": URL, "base": 7},
            {"repository": OTHER},
        ],
        "bob": "not a list",
    })));
    assert_eq!(declared.bindings.len(), 1, "only the valid entry survives");
    assert_eq!(declared.bindings[0].repository, OTHER);
    assert_eq!(declared.invalid.len(), 7);
    for (i, invalid) in declared.invalid.iter().take(6).enumerate() {
        assert!(
            invalid.message.starts_with(&format!("{KEY}.alice[{i}]: ")),
            "{}",
            invalid.message
        );
    }
    assert!(
        declared.invalid[6]
            .message
            .starts_with(&format!("{KEY}.bob: ")),
        "{}",
        declared.invalid[6].message
    );
    let secret: Vec<_> = declared
        .invalid
        .iter()
        .filter(|i| i.message.contains("secret"))
        .collect();
    assert!(secret.is_empty(), "a credential in a URL is never echoed: {secret:?}");
}

#[test]
fn a_non_object_key_and_a_conflicting_duplicate_are_reported() {
    let declared = parse(&config(&json!([1])));
    assert_eq!(declared.invalid.len(), 1);
    assert!(declared.invalid[0].message.starts_with(KEY));

    let declared = parse(&config(&json!({"alice": [
        {"repository": URL}, {"repository": "https://GitHub.com/acme/widgets.git"},
        {"repository": URL, "base": "develop"},
    ]})));
    assert_eq!(declared.bindings.len(), 1, "an identical spelling collapses");
    assert_eq!(declared.invalid.len(), 1, "a conflicting base is refused");
    assert!(declared.invalid[0].message.contains("different bases"));
}

#[test]
fn a_set_reference_resolves_into_the_inline_list_s_bindings() {
    let entries = json!([{"repository": URL}, {"repository": OTHER, "base": "develop"}]);
    let via_set = parse(&config_with_sets(
        &json!({"fleet": entries}),
        &json!({
            "alice": {"set": "fleet"},
        }),
    ));
    assert!(via_set.invalid.is_empty(), "{:?}", via_set.invalid);
    assert_eq!(via_set, parse(&config(&json!({"alice": entries}))));
}

#[test]
fn two_accounts_referencing_one_set_each_get_every_binding() {
    let declared = parse(&config_with_sets(
        &json!({"fleet": [{"repository": URL}, {"repository": OTHER}]}),
        &json!({"seat-1": {"set": "fleet"}, "seat-2": {"set": "fleet"}}),
    ));
    assert!(declared.invalid.is_empty(), "{:?}", declared.invalid);
    let got: Vec<_> = declared
        .bindings
        .iter()
        .map(|b| (b.account.as_str(), b.repository.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("seat-1", URL),
            ("seat-1", OTHER),
            ("seat-2", URL),
            ("seat-2", OTHER)
        ]
    );
}

#[test]
fn an_empty_set_declares_nothing_and_is_no_error() {
    let declared =
        parse(&config_with_sets(&json!({"fleet": []}), &json!({"alice": {"set": "fleet"}})));
    assert_eq!(declared, Declared::default());
}

#[test]
fn a_set_reference_to_an_undefined_set_fails_closed_for_that_account_only() {
    let declared = parse(&config_with_sets(
        &json!({"fleet": [{"repository": URL}]}),
        &json!({"alice": {"set": "missing"}, "bob": [{"repository": OTHER}]}),
    ));
    assert_eq!(declared.bindings.len(), 1, "bob's inline list still applies");
    assert_eq!(declared.bindings[0].account, "bob");
    assert_eq!(declared.invalid.len(), 1);
    assert_eq!(declared.invalid[0].account.as_deref(), Some("alice"));
    assert!(
        declared.invalid[0]
            .message
            .starts_with(&format!("{KEY}.alice: ")),
        "{}",
        declared.invalid[0].message
    );
    assert!(
        declared.invalid[0]
            .message
            .contains("undefined set \"missing\""),
        "{}",
        declared.invalid[0].message
    );
}

#[test]
fn a_malformed_account_value_fails_closed() {
    let declared = parse(&config_with_sets(
        &json!({"fleet": [{"repository": URL}]}),
        &json!({
            "a-string": "not a list",
            "a-wrong-key": {"sets": "fleet"},
            "an-extra-key": {"set": "fleet", "base": "develop"},
            "a-non-string-set": {"set": 7},
        }),
    ));
    assert!(declared.bindings.is_empty());
    assert_eq!(declared.invalid.len(), 4);
    for invalid in &declared.invalid {
        assert!(
            invalid
                .message
                .contains(&format!("or an object {{\"set\": \"<name>\"}} referencing {SETS_KEY}")),
            "{}",
            invalid.message
        );
    }
}

#[test]
fn a_malformed_sets_block_is_reported_and_fails_every_reference() {
    let declared = parse(&config_with_sets(&json!([1]), &json!({"alice": {"set": "fleet"}})));
    assert!(declared.bindings.is_empty());
    assert_eq!(declared.invalid.len(), 2, "the block, then the reference");
    assert!(
        declared.invalid[0]
            .message
            .starts_with(&format!("{SETS_KEY}: must be an object")),
        "{}",
        declared.invalid[0].message
    );
    assert!(
        declared.invalid[1]
            .message
            .contains("undefined set \"fleet\""),
        "{}",
        declared.invalid[1].message
    );
}

#[test]
fn a_set_that_is_not_a_list_fails_only_its_referencing_accounts() {
    let declared = parse(&config_with_sets(
        &json!({"fleet": "nope", "good": [{"repository": OTHER}]}),
        &json!({"alice": {"set": "fleet"}, "bob": {"set": "good"}}),
    ));
    assert_eq!(
        declared.bindings,
        [Binding {
            account: "bob".into(),
            repository: OTHER.into(),
            base: DEFAULT_BASE.into(),
            key: "github.com/acme/gadgets".into(),
        }]
    );
    assert_eq!(declared.invalid.len(), 2, "the set, then the reference");
    assert!(
        declared.invalid[0]
            .message
            .starts_with(&format!("{SETS_KEY}.fleet: must be a list")),
        "{}",
        declared.invalid[0].message
    );
    assert!(
        declared.invalid[1].message.contains("is not a list"),
        "{}",
        declared.invalid[1].message
    );
}

/// A malformed entry shared by two accounts: each account gets its own
/// `Invalid`, and every message names both the account (`{KEY}.ACCOUNT`) and
/// the set entry path — the pass WARNs by distinct message, so both reports
/// survive its deduplication (#14699 review ask).
#[test]
fn a_malformed_set_entry_is_reported_once_per_referencing_account() {
    let declared = parse(&config_with_sets(
        &json!({"fleet": [{"repository": URL}, {"repository": "http://insecure/x/y"}]}),
        &json!({"alice": {"set": "fleet"}, "bob": {"set": "fleet"}}),
    ));
    let got: Vec<_> = declared
        .bindings
        .iter()
        .map(|b| b.account.as_str())
        .collect();
    assert_eq!(got, ["alice", "bob"], "the valid entry applies for both");
    assert_eq!(declared.invalid.len(), 2, "one Invalid per referencing account");
    let mut messages: Vec<&str> = declared
        .invalid
        .iter()
        .map(|i| i.message.as_str())
        .collect();
    messages.sort();
    for (account, message) in [("alice", messages[0]), ("bob", messages[1])] {
        assert!(message.contains(&format!("{KEY}.{account}")), "{message}");
        assert!(
            message.contains(&format!("{SETS_KEY}.fleet[1]: ")),
            "the set entry path: {message}"
        );
    }
    assert_ne!(messages[0], messages[1], "dedup by message keeps both reports");
}

#[test]
fn a_duplicate_repository_in_a_set_with_different_bases_is_refused() {
    let declared = parse(&config_with_sets(
        &json!({"fleet": [
            {"repository": URL},
            {"repository": "https://GitHub.com/acme/widgets.git", "base": "develop"},
        ]}),
        &json!({"alice": {"set": "fleet"}}),
    ));
    assert_eq!(declared.bindings.len(), 1, "the first spelling still applies");
    assert_eq!(declared.bindings[0].base, DEFAULT_BASE);
    assert_eq!(declared.invalid.len(), 1);
    assert!(
        declared.invalid[0].message.contains("different bases"),
        "{}",
        declared.invalid[0].message
    );
}

#[test]
fn an_idle_account_missing_the_repository_gets_it_registered_once() {
    let declared = parse(&config(&json!({"alice": [{"repository": URL}]})));
    let registered = Fake::new(|_| Ok(Converged::Registered));
    let mut state = ReconcileState::default();
    let index = index_of(&[]);
    let pass = registered.run(&declared, &index, &mut state, 0);
    assert_eq!(outcomes(&pass), [Outcome::Registered]);
    assert_eq!(registered.calls(), 1);

    // Converged: the next pass is told it is present.
    let present = Fake::new(|_| Ok(Converged::Present));
    assert_eq!(outcomes(&present.run(&declared, &index, &mut state, 60)), [Outcome::Present]);
}

#[test]
fn a_busy_account_is_deferred_without_a_failure_and_retried_next_pass() {
    let declared = parse(&config(&json!({"alice": [{"repository": URL}]})));
    let fake = Fake::new(|_| Err(AccountBusy("account session is busy").into()));
    let mut state = ReconcileState::default();
    let index = index_of(&[]);
    for now in [0, 60, 120] {
        assert_eq!(outcomes(&fake.run(&declared, &index, &mut state, now)), [Outcome::Deferred]);
    }
    assert_eq!(fake.calls(), 3, "retried every pass: no backoff for a busy account");
    // The worker finishes.
    *fake.answer.borrow_mut() = Box::new(|_| Ok(Converged::Registered));
    assert_eq!(outcomes(&fake.run(&declared, &index, &mut state, 180)), [Outcome::Registered]);
}

#[test]
fn an_account_without_a_binding_is_never_touched() {
    let declared = parse(&config(&json!({"alice": [{"repository": URL}]})));
    let fake = Fake::new(|b| {
        assert_eq!(b.account, "alice", "only the bound account is converged");
        Ok(Converged::Present)
    });
    let mut state = ReconcileState::default();
    fake.registered
        .borrow_mut()
        .insert("bob".into(), vec![("https://github.com/x/y".into(), "github.com/x/y".into())]);
    let pass = fake.run(&declared, &index_of(&[]), &mut state, 0);
    assert!(pass.drift.is_empty(), "bob has no binding, so no drift is reported for him");
    assert_eq!(fake.calls(), 1);
    // And nothing at all when nothing is declared.
    let none = Fake::new(|_| panic!("must not be called"));
    let pass = none.run(&Declared::default(), &index_of(&[]), &mut state, 0);
    assert_eq!(pass, Pass::default());
}

#[test]
fn a_removed_binding_deletes_nothing_and_is_reported_as_drift_once() {
    // alice still has one binding (OTHER); URL was registered and is no longer declared.
    let declared = parse(&config(&json!({"alice": [{"repository": OTHER}]})));
    let fake = Fake::new(|_| Ok(Converged::Present));
    fake.registered.borrow_mut().insert(
        "alice".into(),
        vec![
            (URL.into(), "github.com/acme/widgets".into()),
            (OTHER.into(), "github.com/acme/gadgets".into()),
        ],
    );
    let mut state = ReconcileState::default();
    let pass = fake.run(&declared, &index_of(&[]), &mut state, 0);
    assert_eq!(
        pass.drift,
        [Drift {
            account: "alice".into(),
            undeclared: vec![URL.into()],
        }]
    );
    assert_eq!(outcomes(&pass), [Outcome::Present], "nothing removed or re-registered");
    assert_eq!(
        state.bindings.drift_reported["alice"],
        [URL.to_owned()],
        "WARNed once per distinct set"
    );
    // Redeclared: the drift clears.
    let declared = parse(&config(&json!({"alice": [{"repository": OTHER}, {"repository": URL}]})));
    assert!(fake
        .run(&declared, &index_of(&[]), &mut state, 60)
        .drift
        .is_empty());
    assert!(state.bindings.drift_reported.is_empty());
}

#[test]
fn an_account_with_an_invalid_entry_reports_no_drift() {
    let declared = parse(&config(&json!({"alice": [
        {"repository": OTHER}, {"repository": "http://insecure/x/y"},
    ]})));
    let fake = Fake::new(|_| Ok(Converged::Present));
    fake.registered
        .borrow_mut()
        .insert("alice".into(), vec![(URL.into(), "github.com/acme/widgets".into())]);
    let pass = fake.run(&declared, &index_of(&[]), &mut ReconcileState::default(), 0);
    assert!(pass.drift.is_empty(), "the bad entry may be the one that names it");
}

#[test]
fn a_refusal_names_the_key_backs_off_and_does_not_hold_other_bindings() {
    let declared = parse(&config(&json!({
        "alice": [{"repository": URL}],
        "carol": [{"repository": OTHER}],
    })));
    let fake = Fake::new(|b| {
        if b.account == "alice" {
            anyhow::bail!("Codex account not found")
        }
        Ok(Converged::Registered)
    });
    let mut state = ReconcileState::default();
    let index = index_of(&[]);
    let pass = fake.run(&declared, &index, &mut state, 1_000);
    let [Outcome::Failed { error }, Outcome::Registered] = &outcomes(&pass)[..] else {
        panic!("{:?}", pass.reports)
    };
    assert!(error.starts_with(&format!("{KEY}.alice ({URL}): ")), "{error}");
    assert!(error.contains("Codex account not found"), "{error}");

    // Backoff: no call for alice until it is due; carol is converged again.
    let before = fake.calls();
    let pass = fake.run(&declared, &index, &mut state, 1_060);
    assert!(
        matches!(outcomes(&pass)[0], Outcome::BackingOff { retry_at } if retry_at == 1_000 + BACKOFF_BASE_SECS)
    );
    assert_eq!(fake.calls(), before + 1, "only carol's repeat call");
    let pass = fake.run(&declared, &index, &mut state, 1_000 + BACKOFF_BASE_SECS);
    assert!(matches!(outcomes(&pass)[0], Outcome::Failed { .. }), "due again");
}

#[test]
fn a_held_or_disabled_account_is_skipped_before_any_call() {
    let profiles = tempfile::tempdir().unwrap();
    let held: PathBuf = profiles.path().join("alice");
    std::fs::create_dir_all(&held).unwrap();
    let off = profiles.path().join("bob");
    std::fs::create_dir_all(&off).unwrap();
    let accounts = [
        descriptor("alice", &held, true),
        descriptor("bob", &off, false),
    ];
    let index = index_of(&accounts);
    session_hold::write_hold(&held, session_hold::now_unix_ms()).unwrap();

    let declared = parse(&config(&json!({
        "alice": [{"repository": URL}], "bob": [{"repository": URL}],
    })));
    let fake = Fake::new(|_| panic!("a held or disabled account is never converged"));
    let pass = fake.run(&declared, &index, &mut ReconcileState::default(), 0);
    assert_eq!(
        outcomes(&pass),
        [
            Outcome::Skipped("held (operator stop)"),
            Outcome::Skipped("account disabled")
        ]
    );
}

/// #14287: the config key is an account *reference*; `account_matches_reference`
/// accepts the email as well as the name, so the skip guards must too.
#[test]
fn an_email_spelled_key_is_skipped_when_the_account_is_disabled_or_held() {
    let profiles = tempfile::tempdir().unwrap();
    let held: PathBuf = profiles.path().join("alice");
    std::fs::create_dir_all(&held).unwrap();
    let off = profiles.path().join("bob");
    std::fs::create_dir_all(&off).unwrap();
    let mut alice = descriptor("alice", &held, true);
    alice.email = Some("alice@example.com".into());
    let mut bob = descriptor("bob", &off, false);
    bob.email = Some("Bob@Example.com".into());
    let index = index_of(&[alice, bob]);
    session_hold::write_hold(&held, session_hold::now_unix_ms()).unwrap();

    let declared = parse(&config(&json!({
        "alice@example.com": [{"repository": URL}],
        "bob@example.com": [{"repository": URL}],
    })));
    let fake = Fake::new(|_| panic!("a held or disabled account is never converged"));
    let pass = fake.run(&declared, &index, &mut ReconcileState::default(), 0);
    assert_eq!(
        outcomes(&pass),
        [
            Outcome::Skipped("held (operator stop)"),
            Outcome::Skipped("account disabled")
        ]
    );
    assert_eq!(fake.calls(), 0);
}

#[test]
fn an_email_spelled_key_for_an_enabled_account_still_converges() {
    let profiles = tempfile::tempdir().unwrap();
    let profile = profiles.path().join("alice");
    std::fs::create_dir_all(&profile).unwrap();
    let mut alice = descriptor("alice", &profile, true);
    alice.email = Some("alice@example.com".into());
    let index = index_of(&[alice]);
    let declared = parse(&config(&json!({"alice@example.com": [{"repository": URL}]})));
    let fake = Fake::new(|_| Ok(Converged::Registered));
    let pass = fake.run(&declared, &index, &mut ReconcileState::default(), 0);
    assert_eq!(outcomes(&pass), [Outcome::Registered]);
}

#[test]
fn a_stop_landing_after_the_check_is_honoured_by_the_hold_probe() {
    let profiles = tempfile::tempdir().unwrap();
    let profile = profiles.path().join("alice");
    std::fs::create_dir_all(&profile).unwrap();
    let index = index_of(&[descriptor("alice", &profile, true)]);
    let declared = parse(&config(&json!({"alice": [{"repository": URL}]})));
    let converge = |_: &Binding, hold: &dyn Fn() -> anyhow::Result<()>| {
        // The operator's `stop` lands between the pass's check and the mutation.
        session_hold::write_hold(&profile, session_hold::now_unix_ms()).unwrap();
        hold()?;
        Ok(Converged::Registered)
    };
    let registered = |_: &str| Ok(None);
    let mut state = ReconcileState::default();
    let pass = converge_pass(
        &declared,
        &mut Inputs {
            index: &index,
            converge: &converge,
            ready: &mut |_, _| Unverified,
            registered: &registered,
        },
        &mut state,
        0,
    );
    assert_eq!(outcomes(&pass), [Outcome::Skipped("held (operator stop)")]);
    assert!(
        state.bindings.memory.values().all(|m| m.failures == 0),
        "a hold is not a failure"
    );
}
