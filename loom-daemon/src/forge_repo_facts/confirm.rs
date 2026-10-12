//! Owner confirmation for verified negatives.
//!
//! A `head=<owner>:<branch>` filter built from a wrong owner returns `[]`,
//! which reads as "no PR" — and a reaper removes or switches on that. So a
//! negative answer computed from a remembered owner is only trusted after a
//! forced re-read of the record says the owner is still current. To keep that
//! to one read per root per pass (not one per worktree), answers are memoised
//! inside a [`PassScope`].

use std::cell::Cell;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::base::base_repo;
use super::record::{self, record_key};
use super::{enabled, state, Fact, GhRepoEnv};

static NEXT_PASS: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CURRENT_PASS: Cell<u64> = const { Cell::new(0) };
}

/// One reaper pass: owner confirmations inside it are memoised per root, and
/// a record confirmed inside it needs no second confirm. Scopes nest; the
/// innermost wins. Dropping the guard forgets the pass's answers.
#[derive(Debug)]
pub(crate) struct PassScope {
    id: u64,
    prev: u64,
}

impl PassScope {
    #[must_use]
    pub(crate) fn enter() -> Self {
        let id = NEXT_PASS.fetch_add(1, Ordering::Relaxed);
        let prev = CURRENT_PASS.with(|c| c.replace(id));
        Self { id, prev }
    }
}

impl Drop for PassScope {
    fn drop(&mut self) {
        CURRENT_PASS.with(|c| c.set(self.prev));
        let id = self.id;
        state::with(|s| {
            s.confirms.retain(|k, _| k.2 != id);
            s.confirmed.retain(|(p, _)| *p != id);
        });
    }
}

fn current_pass() -> Option<u64> {
    Some(CURRENT_PASS.with(Cell::get)).filter(|p| *p != 0)
}

/// Was `fact`'s record re-read from the forge inside the current pass?
pub(crate) fn confirmed_in_pass(fact: &Fact) -> bool {
    let Some(pass) = current_pass() else {
        return false;
    };
    let key = record_key(&fact.host, &fact.configured_nwo);
    state::with(|s| s.confirmed.contains(&(pass, key)))
}

/// Is `owner_used` still `root`'s canonical owner, per a forced conditional
/// re-read of the record made now? `false` on a different owner (the record
/// is then marked suspect) and on any failure — breaker, network, a record in
/// its failure backoff. With facts off this is `true`: no fact was used.
pub(crate) fn confirm_owner(root: &Path, env: GhRepoEnv, owner_used: &str) -> bool {
    confirm_owner_with(Path::new("gh"), root, env, owner_used)
}

/// [`confirm_owner`] with an explicit `gh` program.
pub(crate) fn confirm_owner_with(gh: &Path, root: &Path, env: GhRepoEnv, owner_used: &str) -> bool {
    if !enabled() {
        return true;
    }
    let memo_key = current_pass().map(|p| (root.to_path_buf(), env, p));
    let memo = memo_key
        .as_ref()
        .and_then(|k| state::with(|s| s.confirms.get(k).cloned()));
    let canonical = match memo {
        Some(answer) => answer,
        None => {
            let answer = revalidate(gh, root, env);
            if let Some(k) = memo_key {
                state::with(|s| s.confirms.insert(k, answer.clone()));
            }
            answer
        }
    };
    match canonical {
        Some(owner) if owner.eq_ignore_ascii_case(owner_used) => true,
        Some(owner) => {
            log::warn!(
                "forge_repo_facts: {} answered for owner {owner_used}, but the forge now says \
                 {owner}; treating the negative as unknown",
                root.display()
            );
            if let Some(base) = base_repo(root, env) {
                record::mark_suspect(&record_key(&base.host, &base.nwo), "owner changed");
            }
            false
        }
        None => false,
    }
}

/// The forced re-read: `None` on any failure.
fn revalidate(gh: &Path, root: &Path, env: GhRepoEnv) -> Option<String> {
    let base = base_repo(root, env)?;
    let key = record_key(&base.host, &base.nwo);
    let prior = record::load(&key);
    if prior.as_ref().is_some_and(|r| r.in_backoff(state::now())) {
        return None;
    }
    let rec = record::verify(gh, root, &base, prior.as_ref(), "repo_facts.confirm").ok()?;
    if let Some(pass) = current_pass() {
        state::with(|s| s.confirmed.insert((pass, key)));
    }
    Some(rec.canonical_owner)
}
