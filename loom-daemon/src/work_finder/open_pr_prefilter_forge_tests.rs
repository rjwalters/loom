//! The batched open-PR pre-filter through the **production** dispatcher
//! ([`RegistryDispatcher`] over a real [`SweepRegistry`]) and a fake `gh`
//! that logs every call (Issue #8865).
//!
//! The registry here has its guards ON (`skip_label_flip = false`), so a
//! candidate the pre-filter lets through runs the real `dispatch()`: the
//! `prepare` phase, then the 2.5 closed-issue read and the 2.6 open-PR probe.
//! That is what makes the before/after comparison in
//! [`measurement_attempts_and_forge_calls_per_tick`] a comparison of the two
//! real paths rather than of two fakes.
//!
//! Every test holds the default `serial_test` key: the write-scope gate that
//! `prepare` runs reads process-global environment
//! (`crate::write_scope_test_support`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serial_test::serial;

use super::super::*;
use crate::claim_reconciliation::open_pr_listing::test_support::{listing, row, Row};
use crate::sweep_registry::test_support::{
    fake_gh_graphql_arm, state_probe_json, touch_sweep_command,
};
use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
use crate::write_scope_test_support::WritableRoot;

/// Process environment a dispatch reads, cleared for the test and restored
/// after it.
struct Env(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Env {
    fn isolated(root: &Path) -> Self {
        let cleared = [
            "LOOM_REPO",
            "GH_REPO",
            "LOOM_RUNTIME",
            "LOOM_RUNTIME_BUILDER",
            "LOOM_MODEL",
            "LOOM_MODEL_PROFILE",
            "LOOM_MODEL_EXPERIMENT",
            "LOOM_BACKSTOP_LEASE_DIR",
            "LOOM_OPEN_PR_MEMO",
        ];
        let emptied = [
            "LOOM_CONFIG_DEFAULTS_FILE",
            "LOOM_SHARED_TOKENS_DIR",
            "LOOM_SHARED_API_KEYS_DIR",
        ];
        let prior = cleared
            .into_iter()
            .chain(emptied)
            .map(|key| (key, std::env::var_os(key)))
            .collect();
        for key in cleared {
            std::env::remove_var(key);
        }
        for key in emptied {
            std::env::set_var(key, "");
        }
        std::env::set_var("LOOM_BACKSTOP_LEASE_DIR", root.join("backstop"));
        Self(prior)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// How the fake `gh` answers the open-PR listing.
enum Pulls {
    /// `200` with these rows and an ETag; `304` when the ETag is presented.
    Rows(Vec<Row>),
    /// A forge outage (`502`).
    Failing,
}

/// A registered workspace, its registry, and the fake `gh`'s call log.
struct Fixture {
    registry: Arc<Mutex<SweepRegistry>>,
    log: PathBuf,
    /// The script the registry's `gh` runs; [`Self::reanswer`] rewrites it.
    inner: PathBuf,
    ws: WritableRoot,
    _env: Env,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// `pulls(repo)` builds the listing for the fixture's `owner/repo`.
    /// `graphql_prs` is what the closes-graph fallback answers (open PR
    /// numbers, whitespace separated).
    fn new(pulls: impl FnOnce(&str) -> Pulls, graphql_prs: &str) -> Self {
        Self::build(pulls, graphql_prs, false)
    }

    /// [`Self::new`] with the registry's guards on (`false`) or off (`true`).
    fn build(pulls: impl FnOnce(&str) -> Pulls, graphql_prs: &str, skip_label_flip: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let env = Env::isolated(&root);
        let inner = root.join("fake-gh.sh");
        let ws = WritableRoot::register_with_gh(&root, &inner);
        let log = root.join("gh.log");

        let script =
            fake_gh(&root, &ws.repo, pulls(&ws.repo), &fake_gh_graphql_arm(graphql_prs, 0));
        write_executable(&inner, &script);

        let spawn = root.join(".loom/scripts/spawn-claude.sh");
        std::fs::create_dir_all(spawn.parent().unwrap()).unwrap();
        write_executable(&spawn, "#!/usr/bin/env bash\necho spawned\nexit 0\n");
        touch_sweep_command(&root);

        let mut config = SweepRegistryConfig::new(root.clone());
        config.spawn_bin = Some(spawn);
        config.gh_bin = Some(ws.gh.clone());
        config.skip_label_flip = skip_label_flip;
        config.journal_path = Some(root.join("test-sweeps-journal.json"));
        Self {
            registry: Arc::new(Mutex::new(SweepRegistry::new(config))),
            log,
            inner,
            ws,
            _env: env,
            _dir: dir,
        }
    }

    /// Rewrite the fake `gh`'s answers mid-test, keeping its call log.
    fn reanswer(&self, pulls: impl FnOnce(&str) -> Pulls, legs: &str) {
        let root = self.inner.parent().unwrap();
        let script = fake_gh(root, &self.ws.repo, pulls(&self.ws.repo), legs);
        write_executable(&self.inner, &script);
    }

    fn dispatcher(&self) -> RegistryDispatcher {
        RegistryDispatcher::new(Arc::clone(&self.registry))
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Calls made after the first `skip`, as a [`Calls`] tally.
    fn tally_from(&self, skip: usize) -> Calls {
        let calls = self.calls();
        let calls = &calls[skip.min(calls.len())..];
        let count = |pred: &dyn Fn(&str) -> bool| calls.iter().filter(|c| pred(c)).count();
        Calls {
            total: calls.len(),
            listing: count(&|c| c.contains("pulls?state=open")),
            issue: count(&|c| c.contains("/issues/")),
            graphql: count(&|c| is_guard_graphql(c)),
        }
    }
}

/// A GraphQL call of the open-PR guard, not the landed pre-filter's
/// closes-graph query (#11853), the one that asks for the last reopen.
fn is_guard_graphql(call: &str) -> bool {
    call.contains("graphql") && !call.contains("REOPENED_EVENT")
}

/// `gh` calls of one kind made during a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Calls {
    total: usize,
    /// Open-PR listing page reads.
    listing: usize,
    /// Per-issue reads (`repos/…/issues/<n>…`).
    issue: usize,
    graphql: usize,
}

/// The fake `gh`: logs each call, answers the open-PR listing per `pulls`,
/// then `legs` (arms for the guard's other reads, tried before the catch-all),
/// then `repo view` and an open-issue body for any other `api repos/…` read.
fn fake_gh(root: &Path, repo: &str, pulls: Pulls, legs: &str) -> String {
    let pulls_arm = match pulls {
        Pulls::Rows(rows) => pages_arm(root, &rows, true),
        Pulls::Failing => "case \"$*\" in api*'pulls?state=open'*)\n  \
             echo 'gh: Something went wrong (HTTP 502)' >&2; exit 1 ;;\nesac\n"
            .to_string(),
    };
    format!(
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >> \"{log}\"\n\
         {pulls_arm}\
         {legs}\
         if [[ \"$1\" == repo && \"$2\" == view ]]; then printf '{repo}\\n'; exit 0; fi\n\
         if [[ \"$1\" == api && \"$2\" == repos/* ]]; then printf '%s\\n' '{state}'; exit 0; fi\n\
         exit 0\n",
        log = root.join("gh.log").display(),
        state = state_probe_json("open", false),
    )
}

fn write_executable(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.sync_all();
    }
}

/// A `bash` arm serving `rows` as 100-row listing pages from files under
/// `root`, each `200` with a per-page ETag, or `304` (exit 1, like real
/// `gh`) when the caller presents one and `conditional` is set.
fn pages_arm(root: &Path, rows: &[Row], conditional: bool) -> String {
    let not_modified = "case \"$*\" in api*'pulls?state=open'*'If-None-Match'*)\n  \
         printf 'HTTP/2.0 304 Not Modified\\r\\n\\r\\n'; exit 1 ;;\nesac\n";
    let mut arm = String::from(if conditional { not_modified } else { "" });
    let tag = if conditional { "p" } else { "q" };
    let pages: Vec<&[Row]> = if rows.is_empty() {
        vec![rows]
    } else {
        rows.chunks(100).collect()
    };
    // A full last page is followed by an empty one, as on the forge.
    let trailing_empty = rows.len().is_multiple_of(100) && !rows.is_empty();
    for (i, page) in pages.iter().enumerate() {
        let file = root.join(format!("pulls-page-{}.json", i + 1));
        std::fs::write(&file, listing(page)).unwrap();
        arm.push_str(&format!(
            "case \"$*\" in api*'pulls?state=open'*'&page={n}'|api*'pulls?state=open'*'&page={n} '*)\n  \
             printf 'HTTP/2.0 200 OK\\r\\nEtag: W/\"{tag}{n}\"\\r\\n\\r\\n'; cat '{file}'; exit 0 ;;\nesac\n",
            n = i + 1,
            file = file.display(),
        ));
    }
    if trailing_empty {
        arm.push_str(
            "case \"$*\" in api*'pulls?state=open'*)\n  \
             printf 'HTTP/2.0 200 OK\\r\\n\\r\\n'; echo '[]'; exit 0 ;;\nesac\n",
        );
    }
    arm
}

/// An open same-repo PR `number` on a topic branch, linking `issue` by body.
fn closing(repo: &str, number: u32, issue: u32) -> Row {
    row(number, &[])
        .head(&format!("topic-{number}"))
        .repo(repo)
        .body(&format!("Closes #{issue}"))
}

const N: u32 = 12;

fn blocked_numbers() -> Vec<u32> {
    (8601..8601 + N).collect()
}

fn blocked_rows(repo: &str) -> Vec<Row> {
    blocked_numbers()
        .into_iter()
        .map(|issue| closing(repo, issue + 1000, issue))
        .collect()
}

/// What links and what does not is the guard's own classifier: a fork PR from
/// an untrusted author does not block (H14), a passing mention does not block
/// (#8940), a `feature/issue-N` head with no closing phrase does.
#[test]
#[serial]
fn the_prefilter_uses_the_guards_classifier() {
    let fx = Fixture::new(
        |repo| {
            Pulls::Rows(vec![
                closing(repo, 9801, 8801),
                row(8802, &[]).repo(repo).body("no phrase"), // head feature/issue-8802
                row(9803, &[])
                    .head("topic")
                    .repo(repo)
                    .body("filed #8803 to track it"),
                row(9804, &[])
                    .head("topic")
                    .repo("outsider/loom")
                    .author("rando", "NONE")
                    .body("Closes #8804"),
                row(9805, &[])
                    .head("topic")
                    .repo("friend/loom")
                    .author("pal", "COLLABORATOR")
                    .body("Closes #8805"),
            ])
        },
        "",
    );
    let blocked = fx
        .dispatcher()
        .open_pr_blocked(&[8801, 8802, 8803, 8804, 8805, 8806]);
    let mut got: Vec<(u32, u32)> = blocked.into_iter().collect();
    got.sort_unstable();
    assert_eq!(got, vec![(8801, 9801), (8802, 8802), (8805, 9805)]);
    assert_eq!(fx.tally_from(0).listing, 1);
}

/// An unreadable listing is no verdict: it neither records nor removes a memo
/// entry.
#[test]
#[serial]
fn an_unreadable_listing_leaves_the_memo_alone() {
    let fx = Fixture::new(|_| Pulls::Failing, "");
    let at = chrono::Utc::now() - chrono::Duration::hours(3);
    fx.registry
        .lock()
        .unwrap()
        .seed_open_pr_memo(8971, 9971, at);
    assert!(fx.dispatcher().open_pr_blocked(&[8971, 8972]).is_empty());
    let reg = fx.registry.lock().unwrap();
    let later = at + chrono::Duration::seconds(1);
    assert_eq!(
        reg.fresh_open_pr_memo(8971, later)
            .map(|m| (m.pr, m.verified_at)),
        Some((9971, at))
    );
    assert!(reg.fresh_open_pr_memo(8972, chrono::Utc::now()).is_none());
}

/// Guards off (`skip_label_flip`, as in a credential-less fixture): the
/// pre-filter reads nothing, mirroring steps 2.5/2.6.
#[test]
#[serial]
fn guards_off_means_no_prefilter_read() {
    let fx = Fixture::build(|repo| Pulls::Rows(blocked_rows(repo)), "", true);
    let before = fx.calls().len();
    assert!(fx
        .dispatcher()
        .open_pr_blocked(&blocked_numbers())
        .is_empty());
    assert_eq!(fx.tally_from(before).total, 0, "{:?}", fx.calls());
}

/// #12025: one failed read does not blank the pre-filter. For
/// `LISTING_REUSE_FOR` the last good listing answers; past it, nothing is
/// verified. (Ported from the retired multi-workspace tick's test to the
/// `open_pr_blocked` read that survives #11911.)
#[test]
#[serial]
fn a_failed_read_reuses_the_last_good_listing_for_a_few_minutes() {
    use crate::worktree_ops::linked_pr_listing::{age_good_listing, LISTING_REUSE_FOR};
    let fx = Fixture::new(|repo| Pulls::Rows(vec![closing(repo, 9981, 8981)]), "");
    let warm = fx.dispatcher().open_pr_blocked(&[8981, 8982]);
    assert_eq!(warm.get(&8981), Some(&9981));

    fx.reanswer(|_| Pulls::Failing, &fake_gh_graphql_arm("", 0));
    let reused = fx.dispatcher().open_pr_blocked(&[8981, 8982]);
    assert_eq!(reused.get(&8981), Some(&9981), "the last good listing answers");
    assert!(!reused.contains_key(&8982), "{reused:?}");

    let root = fx.inner.parent().unwrap().to_path_buf();
    age_good_listing(&root, &fx.ws.repo, LISTING_REUSE_FOR + std::time::Duration::from_secs(1));
    let expired = fx.dispatcher().open_pr_blocked(&[8981, 8983]);
    assert!(expired.is_empty(), "past the window nothing is verified: {expired:?}");
}
