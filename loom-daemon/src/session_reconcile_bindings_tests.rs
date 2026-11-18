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
use std::cell::RefCell;
use std::path::PathBuf;

const URL: &str = "https://github.com/acme/widgets";
const OTHER: &str = "https://github.com/acme/gadgets";

fn config(clones: &Value) -> Value {
    json!({"autonomous": {"sessionReconcile": {"privateClones": clones}}})
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
        let converge = |binding: &Binding, _hold: &dyn Fn() -> anyhow::Result<()>| {
            self.calls
                .borrow_mut()
                .push((binding.account.clone(), binding.repository.clone()));
            (self.answer.borrow_mut())(binding)
        };
        let registered = |account: &str| Ok(self.registered.borrow().get(account).cloned());
        converge_pass(
            declared,
            &Inputs {
                index,
                converge: &converge,
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
        &Inputs {
            index: &index,
            converge: &converge,
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
