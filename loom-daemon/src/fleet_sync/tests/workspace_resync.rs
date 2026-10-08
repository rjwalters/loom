//! Tests for the workspace resync (#10718).
//!
//! Real git, in temp dirs only: a bare `origin`, a `seed` clone that stands
//! for everyone else pushing to it, and one clone per "host". The forge (the
//! claim ref) is the in-memory `FakeRefForge` behind the fleet store's
//! transport seams, and its batched head query is the in-memory `ForgeHeads`
//! (see `workspace_resync_heads.rs`). The payload is a small synthetic
//! `defaults/` tree. No network, no `gh`, no registered workspace.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use tempfile::TempDir;

use super::git::WORKTREE_PREFIX;
use super::*;
use crate::fleet_store::resync_claim::test_support::{FakeRefForge, Fault, Shared};
use crate::fleet_store::resync_claim::{claim_message, CLAIM_REF};
use crate::init::payload::{resync_workspace_with, ResyncOutcome, Stamp};
use crate::install_compat::INSTALL_METADATA_PATH;

const RUNNING: &str = "0.19.880";
const REPO: &str = "acme/app";
const INTERVAL: Duration = Duration::from_secs(60);

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
}

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// A `defaults/` tree touching every payload surface. `new` is the release
/// the daemon under test runs: one script changed, one doc added.
fn defaults(root: &Path, new: bool) -> PathBuf {
    let d = root.join(if new { "defaults-new" } else { "defaults-old" });
    write(&d.join(".loom-README.md"), "readme\n");
    write(&d.join("pricing.json"), "{}\n");
    write(&d.join("roles/builder.md"), "builder\n");
    write(
        &d.join("scripts/a.sh"),
        if new {
            "#!/bin/sh\necho new\n"
        } else {
            "#!/bin/sh\necho old\n"
        },
    );
    write(&d.join("hooks/h.sh"), "#!/bin/sh\n");
    write(&d.join("docs/d.md"), "doc\n");
    if new {
        write(&d.join("docs/new.md"), "new in this release\n");
    }
    write(&d.join("runtimes/r.json"), "{}\n");
    write(&d.join(".loom/bin/loom"), "#!/bin/sh\n");
    write(&d.join(".claude/commands/loom/builder.md"), "cmd\n");
    d
}

fn payload_from(defaults: &Path) -> Payload {
    payload_at(defaults, RUNNING)
}

fn payload_at(defaults: &Path, version: &str) -> Payload {
    Payload::from_defaults(
        defaults.to_path_buf(),
        Stamp {
            version: v(version),
            commit: Some("a".repeat(40)),
            requires_daemon: "0.19.772".to_string(),
            release_build: true,
        },
    )
}

/// How the repo looks before the daemon under test meets it.
#[derive(Clone, Copy)]
struct Seed {
    /// Recorded `loom_version`.
    version: &'static str,
    /// Recorded `requires_daemon`, if any.
    requires: Option<&'static str>,
    /// Installed from the release under test (so the payload diff is empty).
    current: bool,
    /// The repo ignores `.loom/bin/`, so that payload file is never tracked.
    ignore_bin: bool,
}

const STALE: Seed = Seed {
    version: "0.19.800",
    requires: Some("0.19.772"),
    current: false,
    ignore_bin: false,
};

struct Fixture {
    tmp: TempDir,
    origin: PathBuf,
    seed: PathBuf,
    new_defaults: PathBuf,
    forge: Rc<FakeRefForge>,
}

fn metadata(version: &str, requires: Option<&str>) -> String {
    let requires =
        requires.map_or_else(String::new, |r| format!("  \"requires_daemon\": \"{r}\",\n"));
    format!(
        "{{\n  \"loom_version\": \"{version}\",\n{requires}  \"install_date\": \"2026-01-01\",\n  \
         \"installed_files\": []\n}}\n"
    )
}

impl Fixture {
    fn new(seed: Seed) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let old = defaults(&root, false);
        let new = defaults(&root, true);
        let origin = root.join("origin.git");
        fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--quiet", "--bare", "-b", "main"]);
        // A developer's global hooksPath must not disable the hook the
        // branch-protection test installs.
        git(&origin, &["config", "core.hooksPath", "hooks"]);

        let work = root.join("seed");
        fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "--quiet", "-b", "main"]);
        identify(&work);
        git(&work, &["remote", "add", "origin", &origin.to_string_lossy()]);
        write(&work.join("README.md"), "an app\n");
        let ignore = if seed.ignore_bin {
            ".loom/worktrees/\n.loom/bin/\n"
        } else {
            ".loom/worktrees/\n"
        };
        write(&work.join(".gitignore"), ignore);
        // Install through the resync itself (it is the installer's own
        // payload step), then put the stamp the case wants on top.
        write(&work.join(INSTALL_METADATA_PATH), &metadata("0.19.0", None));
        let from = if seed.current { &new } else { &old };
        let installed = resync_workspace_with(&payload_from(from), &work).unwrap();
        assert!(matches!(installed, ResyncOutcome::Applied { .. }), "{installed:?}");
        write(&work.join(INSTALL_METADATA_PATH), &metadata(seed.version, seed.requires));
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "--quiet", "-m", "install loom"]);
        git(&work, &["push", "--quiet", "origin", "HEAD:refs/heads/main"]);
        Self {
            tmp,
            origin,
            seed: work,
            new_defaults: new,
            forge: Rc::new(FakeRefForge::new()),
        }
    }

    /// A host's clone of the repo: a registered workspace.
    fn clone_as(&self, name: &str) -> PathBuf {
        let root = self.tmp.path().canonicalize().unwrap().join(name);
        git(
            self.tmp.path(),
            &[
                "clone",
                "--quiet",
                &self.origin.to_string_lossy(),
                &root.to_string_lossy(),
            ],
        );
        identify(&root);
        root
    }

    fn origin_head(&self) -> String {
        git(&self.origin, &["rev-parse", "refs/heads/main"])
    }

    fn origin_commits(&self) -> usize {
        git(&self.origin, &["rev-list", "--count", "refs/heads/main"])
            .parse()
            .unwrap()
    }

    fn origin_file(&self, path: &str) -> String {
        git(&self.origin, &["show", &format!("refs/heads/main:{path}")])
    }

    /// Someone else changes the default branch: `edit` runs in the seed
    /// clone at origin's head, and the result is pushed.
    fn push_from_seed(&self, message: &str, edit: impl FnOnce(&Path)) {
        git(&self.seed, &["pull", "--quiet", "--ff-only", "origin", "main"]);
        edit(&self.seed);
        git(&self.seed, &["add", "-A"]);
        git(&self.seed, &["commit", "--quiet", "-m", message]);
        git(&self.seed, &["push", "--quiet", "origin", "HEAD:refs/heads/main"]);
    }

    /// Someone else performs exactly the resync the daemon would.
    fn resync_from_seed(&self) {
        let payload = payload_from(&self.new_defaults);
        self.push_from_seed("resync by someone else", |seed| {
            let outcome = resync_workspace_with(&payload, seed).unwrap();
            assert!(matches!(outcome, ResyncOutcome::Applied { .. }), "{outcome:?}");
        });
    }
}

fn identify(repo: &Path) {
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
}

/// One host: a clone, what it remembers, and its gate.
struct Host<'f> {
    fx: &'f Fixture,
    name: &'static str,
    root: PathBuf,
    memory: RefCell<Memory>,
    /// The version this host runs.
    version: &'static str,
    payload: LazyPayload,
    unpacked: Rc<Cell<u32>>,
    now: Cell<DateTime<Utc>>,
    /// How many workspaces a pass may classify before its time budget counts
    /// as spent.
    budget: Cell<u32>,
    /// Runs once, after this host has classified and just before it asks for
    /// the claim: the moment another writer can slip in.
    before_claim: RefCell<Option<Box<dyn FnOnce() + 'f>>>,
    /// The forge's batched head query, and whether the pass is past its
    /// deadline.
    heads: head_check::ForgeHeads,
    overdue: Cell<bool>,
}

impl<'f> Host<'f> {
    fn new(fx: &'f Fixture, name: &'static str) -> Self {
        Self::running(fx, name, RUNNING, fx.new_defaults.clone())
    }

    /// A host running `version`, whose binary embeds `defaults`.
    fn running(
        fx: &'f Fixture,
        name: &'static str,
        version: &'static str,
        defaults: PathBuf,
    ) -> Self {
        let unpacked = Rc::new(Cell::new(0));
        let count = unpacked.clone();
        Self {
            fx,
            name,
            root: fx.clone_as(name),
            memory: RefCell::new(Memory::default()),
            version,
            payload: LazyPayload::new(move || {
                count.set(count.get() + 1);
                Ok(payload_at(&defaults, version))
            }),
            unpacked,
            now: Cell::new(t0()),
            budget: Cell::new(u32::MAX),
            before_claim: RefCell::new(None),
            heads: head_check::ForgeHeads::default(),
            overdue: Cell::new(false),
        }
    }

    /// Move this host's clock forward.
    fn advance(&self, by: Duration) {
        self.now
            .set(self.now.get() + ChronoDuration::from_std(by).unwrap());
    }

    /// A pass in `Mode::Write` on a host in H0.
    fn pass(&self) -> WorkspacePass {
        self.pass_with(Mode::Write, &|| Ok(()), None)
    }

    fn pass_with(
        &self,
        mode: Mode,
        gate: &dyn Fn() -> std::result::Result<(), NotCurrent>,
        floor: Option<&str>,
    ) -> WorkspacePass {
        self.pass_over(std::slice::from_ref(&self.root), mode, gate, floor)
    }

    fn pass_over(
        &self,
        roots: &[PathBuf],
        mode: Mode,
        gate: &dyn Fn() -> std::result::Result<(), NotCurrent>,
        floor: Option<&str>,
    ) -> WorkspacePass {
        let forge = self.fx.forge.clone();
        let classified = Cell::new(0);
        let env = Env {
            host: self.name,
            running: v(self.version),
            floor: floor.map(v),
            interval: INTERVAL,
            payload: &self.payload,
            nwo: &|root| (!root.join(".not-github").exists()).then(|| REPO.to_string()),
            may_write: &|root, _| {
                if root.join(".read-only-credential").exists() {
                    Err("the credential has READ on acme/app, not WRITE".to_string())
                } else {
                    Ok(())
                }
            },
            forge: &move |_, _| -> Box<dyn ClaimForge> {
                let hook = self.before_claim.borrow_mut().take();
                if let Some(hook) = hook {
                    hook();
                }
                Box::new(Shared(forge.clone()))
            },
            gate,
            clock: &|| self.now.get(),
            spent: &|| {
                classified.set(classified.get() + 1);
                classified.get() > self.budget.get()
            },
            overdue: &|| self.overdue.get(),
            heads: &|asks| self.heads.answer(asks),
        };
        run(&env, roots, mode, &mut self.memory.borrow_mut())
    }

    fn resync_worktrees(&self) -> Vec<String> {
        fs::read_dir(self.root.join(".loom/worktrees"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with(WORKTREE_PREFIX))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The gate is read at the start of a pass, immediately before the claim and
/// immediately before each push: which call of a pass's gate each one is.
const AT_CLAIM: u32 = 2;
const AT_PUSH: u32 = 3;

fn only(pass: &WorkspacePass) -> &WorkspaceReport {
    assert_eq!(pass.workspaces.len(), 1, "{pass:?}");
    &pass.workspaces[0]
}

fn reason(report: &WorkspaceReport) -> &str {
    report.reason.as_deref().unwrap_or("")
}

/// What a pass asked the network: `(head queries, ls-remotes, fetches)`.
fn network(pass: &WorkspacePass) -> (u32, u32, u32) {
    (pass.head_queries, pass.probes, pass.fetches)
}

// ----------------------------------------------------------------------------
// The host gate
// ----------------------------------------------------------------------------

fn h0() -> HostGateInputs {
    HostGateInputs {
        release_build: true,
        release_verified: true,
        verified: true,
        ..HostGateInputs::default()
    }
}

#[test]
fn the_host_gate_names_each_reason() {
    assert_eq!(host_gate(&h0()), Ok(()));
    type Set = fn(&mut HostGateInputs);
    let table: [(Set, NotCurrent); 8] = [
        (|i| i.draining = true, NotCurrent::Draining),
        (|i| i.staged = true, NotCurrent::Staged),
        (|i| i.roll_pending = true, NotCurrent::RollPending),
        (|i| i.stalled = true, NotCurrent::Stalled),
        (|i| i.below_floor = true, NotCurrent::FloorBelow),
        (|i| i.verified = false, NotCurrent::Unverified),
        (|i| i.release_build = false, NotCurrent::NotAReleaseBuild),
        (|i| i.release_verified = false, NotCurrent::ReleaseUnverified),
    ];
    for (set, expected) in table {
        let mut inputs = h0();
        set(&mut inputs);
        assert_eq!(host_gate(&inputs), Err(expected));
    }
    // A build that is no release outranks everything: it is not a state the
    // host will leave. One whose tag is not confirmed yet comes next.
    let mut inputs = h0();
    inputs.release_build = false;
    inputs.release_verified = false;
    inputs.draining = true;
    assert_eq!(host_gate(&inputs), Err(NotCurrent::NotAReleaseBuild));
    inputs.release_build = true;
    assert_eq!(host_gate(&inputs), Err(NotCurrent::ReleaseUnverified));
    assert!(NotCurrent::ReleaseUnverified.is_about_the_build());
    assert!(!NotCurrent::Draining.is_about_the_build());
}

#[test]
fn a_host_that_is_not_h0_reports_and_never_claims() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let before = fx.origin_head();
    for why in [
        NotCurrent::Draining,
        NotCurrent::Staged,
        NotCurrent::RollPending,
        NotCurrent::Stalled,
        NotCurrent::FloorBelow,
        NotCurrent::Unverified,
        NotCurrent::ReleaseUnverified,
    ] {
        let pass = host.pass_with(Mode::Write, &|| Err(why), None);
        assert_eq!(pass.host, Some(format!("host not H0: {why}")));
        assert_eq!(only(&pass).state, WState::W1, "still classified: {why}");
        assert!(pass.alerts.is_empty());
        assert_eq!(network(&pass), (0, 0, 0), "and asks no remote: {why}");
    }
    assert!(fx.forge.calls.borrow().is_empty(), "no claim call for any reason");
    assert_eq!(fx.origin_head(), before);
}

#[test]
fn a_build_that_is_not_a_release_says_so_once_for_the_host() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let pass = host.pass_with(Mode::Write, &|| Err(NotCurrent::NotAReleaseBuild), None);
    assert_eq!(
        pass.host.as_deref(),
        Some("host never resyncs: this daemon is not an official release build")
    );
    // Not a per-repo failure: no reason, no backoff, no alert, no diff made
    // from a payload that is no release.
    let report = only(&pass);
    assert_eq!((report.state, report.reason.as_deref()), (WState::Unknown, None));
    assert!(pass.alerts.is_empty());
    assert_eq!(host.unpacked.get(), 0);
    assert!(fx.forge.calls.borrow().is_empty());
}

// ----------------------------------------------------------------------------
// W1 -> W2 -> W0
// ----------------------------------------------------------------------------

#[test]
fn a_stale_workspace_is_resynced_with_one_commit_and_the_claim_released() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let commits = fx.origin_commits();

    let pass = host.pass();
    let report = only(&pass);
    assert_eq!(report.state, WState::W0, "{report:?}");
    assert!(reason(report).starts_with("resynced to v0.19.880 in "), "{report:?}");
    assert!(pass.alerts.is_empty());

    assert_eq!(fx.origin_commits(), commits + 1, "exactly one commit");
    assert_eq!(fx.origin_file(".loom/scripts/a.sh"), "#!/bin/sh\necho new");
    assert_eq!(fx.origin_file(".loom/docs/new.md"), "new in this release");
    let meta = fx.origin_file(INSTALL_METADATA_PATH);
    assert!(meta.contains("\"loom_version\": \"0.19.880\""), "{meta}");
    assert!(!meta.contains("resync_pending"), "{meta}");
    let message = git(&fx.origin, &["log", "-1", "--format=%B", "refs/heads/main"]);
    assert_eq!(
        message,
        "chore(loom): resync installed Loom to v0.19.880\n\nLoom-Resync-Host: host-a\n\
         Loom-Resync-Version: 0.19.880"
    );
    // The commit's parent is the head that was fetched: a plain fast-forward.
    assert_eq!(git(&fx.origin, &["rev-list", "--count", "--merges", "refs/heads/main"]), "0");

    assert_eq!(fx.forge.ref_sha(CLAIM_REF), None, "claim released");
    assert!(host.resync_worktrees().is_empty(), "worktree removed");
    assert_eq!(git(&host.root, &["worktree", "list"]).lines().count(), 1);

    // Next tick: current, and it costs nothing at the forge.
    let calls = fx.forge.calls.borrow().len();
    let again = host.pass();
    assert_eq!(only(&again).state, WState::W0);
    assert_eq!(fx.forge.calls.borrow().len(), calls);
    assert_eq!(fx.origin_commits(), commits + 1);
}

#[test]
fn two_hosts_racing_resync_exactly_once_and_the_loser_is_not_an_error() {
    let fx = Fixture::new(STALE);
    let a = Host::new(&fx, "host-a");
    let b = Host::new(&fx, "host-b");
    let commits = fx.origin_commits();

    // host-b's whole pass runs while host-a holds the claim, just before
    // host-a pushes: origin is still stale, so host-b wants the claim too.
    let calls = Cell::new(0);
    let b_pass = RefCell::new(None);
    let a_pass = a.pass_with(
        Mode::Write,
        &|| {
            calls.set(calls.get() + 1);
            if calls.get() == AT_PUSH {
                *b_pass.borrow_mut() = Some(b.pass());
            }
            Ok(())
        },
        None,
    );
    assert_eq!(only(&a_pass).state, WState::W0, "{a_pass:?}");
    let b_pass = b_pass.into_inner().expect("host-b ran");
    let lost = only(&b_pass);
    assert_eq!(lost.state, WState::W1);
    assert!(reason(lost).starts_with("claim held by host-a since "), "{lost:?}");
    assert!(b_pass.alerts.is_empty(), "losing the claim is not an alert");
    assert!(b.memory.borrow().backoff.is_empty(), "nor a failure");

    assert_eq!(fx.origin_commits(), commits + 1, "exactly one commit fleet-wide");
    // host-b's next tick finds the work done.
    assert_eq!(only(&b.pass()).state, WState::W0);
    assert_eq!(fx.origin_commits(), commits + 1);
}

#[test]
fn a_leftover_claim_whose_work_landed_is_released_with_no_commit() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    // A holder that died after it was too late to matter: its claim is stale.
    let old = fx
        .forge
        .add_commit(&claim_message("host-x", RUNNING, t0() - ChronoDuration::hours(2)), &[]);
    fx.forge.set_ref(CLAIM_REF, &old);
    // Its push lands between this host's classification and its re-read.
    *host.before_claim.borrow_mut() = Some(Box::new(|| fx.resync_from_seed()));
    let pass = host.pass();
    let report = only(&pass);
    assert_eq!(report.state, WState::W0, "{report:?}");
    assert_eq!(reason(report), "already current once the claim was held; no commit");
    assert_eq!(
        git(&fx.origin, &["log", "-1", "--format=%s", "refs/heads/main"]),
        "resync by someone else",
        "this host added no commit"
    );
    assert_eq!(fx.forge.ref_sha(CLAIM_REF), None, "the leftover claim is released");
    assert!(host.resync_worktrees().is_empty());
}

#[test]
fn a_moved_branch_is_re_read_and_retried_once() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let commits = fx.origin_commits();
    // Someone merges unrelated work between this host's commit and its push.
    let calls = Cell::new(0);
    let pass = host.pass_with(
        Mode::Write,
        &|| {
            calls.set(calls.get() + 1);
            if calls.get() == AT_PUSH {
                fx.push_from_seed("unrelated work", |seed| write(&seed.join("src.txt"), "x\n"));
            }
            Ok(())
        },
        None,
    );
    assert_eq!(only(&pass).state, WState::W0, "{pass:?}");
    assert_eq!(calls.get(), AT_PUSH + 1, "the gate is asked again before the second push");
    assert_eq!(fx.origin_commits(), commits + 2, "their commit, then ours on top");
    assert_eq!(fx.origin_file("src.txt"), "x");
    assert_eq!(fx.origin_file(".loom/docs/new.md"), "new in this release");
    assert_eq!(fx.forge.ref_sha(CLAIM_REF), None);
}

#[test]
fn a_branch_that_moved_because_it_was_resynced_ends_current_with_no_commit() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let commits = fx.origin_commits();
    let calls = Cell::new(0);
    let pass = host.pass_with(
        Mode::Write,
        &|| {
            calls.set(calls.get() + 1);
            if calls.get() == AT_PUSH {
                fx.resync_from_seed();
            }
            Ok(())
        },
        None,
    );
    let report = only(&pass);
    assert_eq!(report.state, WState::W0, "{report:?}");
    assert_eq!(fx.origin_commits(), commits + 1, "only the other writer's commit");
    assert_eq!(
        git(&fx.origin, &["log", "-1", "--format=%s", "refs/heads/main"]),
        "resync by someone else"
    );
    assert_eq!(fx.forge.ref_sha(CLAIM_REF), None);
}

#[test]
fn a_branch_that_keeps_moving_goes_back_to_w1_without_a_failure() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let calls = Cell::new(0);
    let pass = host.pass_with(
        Mode::Write,
        &|| {
            calls.set(calls.get() + 1);
            if calls.get() >= AT_PUSH {
                let n = calls.get();
                fx.push_from_seed("more work", |seed| {
                    write(&seed.join("src.txt"), &format!("{n}\n"))
                });
            }
            Ok(())
        },
        None,
    );
    let report = only(&pass);
    assert_eq!(report.state, WState::W1, "{report:?}");
    assert_eq!(reason(report), "the default branch moved twice; retrying next tick");
    assert!(pass.alerts.is_empty());
    assert!(host.memory.borrow().backoff.is_empty());
    assert_eq!(fx.forge.ref_sha(CLAIM_REF), None, "released for the next tick");
}

#[test]
fn the_gate_failing_between_the_claim_and_the_push_abandons_the_resync() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let before = fx.origin_head();
    // The host starts to roll while the resync is in its worktree.
    let calls = Cell::new(0);
    let pass = host.pass_with(
        Mode::Write,
        &|| {
            calls.set(calls.get() + 1);
            if calls.get() >= AT_PUSH {
                Err(NotCurrent::Draining)
            } else {
                Ok(())
            }
        },
        None,
    );
    let report = only(&pass);
    assert_eq!(report.state, WState::W1, "{report:?}");
    assert_eq!(reason(report), "host not H0: draining before the push");
    assert_eq!(fx.origin_head(), before, "no push");
    assert_eq!(fx.forge.ref_sha(CLAIM_REF), None, "claim released");
    assert!(host.resync_worktrees().is_empty(), "worktree discarded");
    assert!(pass.alerts.is_empty());
    assert!(host.memory.borrow().backoff.is_empty(), "a roll is not a failure");
}

#[test]
fn a_claim_taken_over_before_the_push_is_fenced_off() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let before = fx.origin_head();
    let calls = Cell::new(0);
    let pass = host.pass_with(
        Mode::Write,
        &|| {
            calls.set(calls.get() + 1);
            if calls.get() == AT_PUSH {
                let theirs = fx
                    .forge
                    .add_commit(&claim_message("host-b", RUNNING, t0()), &[]);
                fx.forge.set_ref(CLAIM_REF, &theirs);
            }
            Ok(())
        },
        None,
    );
    let report = only(&pass);
    assert_eq!(report.state, WState::W1, "{report:?}");
    assert_eq!(reason(report), "the claim is no longer ours; not pushing");
    assert_eq!(fx.origin_head(), before);
    assert!(fx.forge.ref_sha(CLAIM_REF).is_some(), "the other host's claim is left alone");
}

// ----------------------------------------------------------------------------
// Never downgrade, never claim for nothing
// ----------------------------------------------------------------------------

#[test]
fn a_release_that_changes_no_installed_file_makes_no_claim_and_no_commit() {
    let fx = Fixture::new(Seed {
        version: RUNNING,
        current: true,
        ..STALE
    });
    let host = Host::new(&fx, "host-a");
    let before = fx.origin_head();
    let pass = host.pass();
    assert_eq!((only(&pass).state, only(&pass).reason.as_deref()), (WState::W0, None));
    assert!(fx.forge.calls.borrow().is_empty(), "zero forge calls");
    assert_eq!(fx.origin_head(), before);

    // The verdict is remembered per default-branch commit: the next tick
    // does not even unpack the payload.
    assert_eq!(host.unpacked.get(), 1);
    host.pass();
    assert_eq!(host.unpacked.get(), 1);
}

#[test]
fn an_empty_diff_is_w0_whatever_the_stamp_says() {
    // Files equal to the payload under a stamp no resync will ever rewrite:
    // one with no `requires_daemon` (ResyncOwed), one below the floor (W3 by
    // version alone). Neither is claimed, and neither loops.
    for (seed, floor) in [
        (
            Seed {
                version: "0.19.800",
                requires: None,
                current: true,
                ignore_bin: false,
            },
            None,
        ),
        (
            Seed {
                version: "0.19.800",
                current: true,
                ..STALE
            },
            Some("0.19.850"),
        ),
    ] {
        let fx = Fixture::new(seed);
        let host = Host::new(&fx, "host-a");
        let before = fx.origin_head();
        for _ in 0..2 {
            let pass = host.pass_with(Mode::Write, &|| Ok(()), floor);
            let report = only(&pass);
            assert_eq!(report.state, WState::W0, "{report:?}");
            assert_eq!(reason(report), "files match the payload; the stamp is left as is");
        }
        assert!(fx.forge.calls.borrow().is_empty());
        assert_eq!(fx.origin_head(), before);
        assert!(fx.origin_file(INSTALL_METADATA_PATH).contains("0.19.800"), "never re-stamped");
    }
}

#[test]
fn a_payload_file_the_repo_ignores_is_not_a_difference() {
    let fx = Fixture::new(Seed {
        version: RUNNING,
        current: true,
        ignore_bin: true,
        ..STALE
    });
    assert!(
        git(&fx.origin, &["ls-tree", "-r", "--name-only", "refs/heads/main"])
            .lines()
            .all(|p| p != ".loom/bin/loom"),
        "fixture: the ignored file is not tracked"
    );
    let host = Host::new(&fx, "host-a");
    assert_eq!(only(&host.pass()).state, WState::W0);
    assert!(fx.forge.calls.borrow().is_empty());
}

#[test]
fn a_too_old_workspace_with_a_real_diff_is_w3_and_is_resynced() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let check = host.pass_with(Mode::Check, &|| Ok(()), Some("0.19.850"));
    assert_eq!(only(&check).state, WState::W3);
    let pass = host.pass_with(Mode::Write, &|| Ok(()), Some("0.19.850"));
    assert_eq!(only(&pass).state, WState::W0);
}

#[test]
fn a_repo_ahead_of_the_daemon_is_never_claimed_or_written() {
    for (version, requires, state, why) in [
        // Compatible per classify(), yet ahead.
        (
            "0.19.900",
            Some("0.19.772"),
            WState::RepoAhead,
            "installed 0.19.900 > running 0.19.880",
        ),
        // ResyncOwed per classify(), yet ahead.
        ("0.19.900", None, WState::RepoAhead, "installed 0.19.900 > running 0.19.880"),
        // W4.
        (
            "0.19.900",
            Some("0.19.890"),
            WState::W4,
            "requires daemon 0.19.890 > running 0.19.880",
        ),
        // Cannot be ordered, so it may be newer.
        (
            "0.20.0-rc1",
            Some("0.19.772"),
            WState::RepoAhead,
            "installed loom_version \"0.20.0-rc1\" is not MAJOR.MINOR.PATCH; it may be newer \
             than this daemon",
        ),
    ] {
        let fx = Fixture::new(Seed {
            version,
            requires,
            ..STALE
        });
        let host = Host::new(&fx, "host-a");
        let before = fx.origin_head();
        let pass = host.pass();
        let report = only(&pass);
        assert_eq!((report.state, reason(report)), (state, why));
        assert!(report.state.repo_ahead(), "exposed for the host roll (#10719)");
        assert_eq!(report.installed.as_deref(), Some(version));
        assert!(fx.forge.calls.borrow().is_empty(), "no claim call for {version}");
        assert_eq!(fx.origin_head(), before);
        assert!(pass.alerts.is_empty());
        assert_eq!(host.unpacked.get(), 0, "no diff either");
    }
}

#[test]
fn a_repo_that_moves_ahead_after_the_claim_is_released_unwritten() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    // A newer host resyncs it to 0.19.900 right after this host's classification.
    *host.before_claim.borrow_mut() = Some(Box::new(|| {
        fx.push_from_seed("resync by a newer host", |seed| {
            write(&seed.join(INSTALL_METADATA_PATH), &metadata("0.19.900", Some("0.19.772")));
            write(&seed.join(".loom/scripts/a.sh"), "#!/bin/sh\necho newest\n");
        });
    }));
    let pass = host.pass();
    let report = only(&pass);
    assert_eq!(report.state, WState::RepoAhead, "{report:?}");
    assert_eq!(reason(report), "installed 0.19.900 > running 0.19.880");
    assert_eq!(
        fx.origin_file(".loom/scripts/a.sh"),
        "#!/bin/sh\necho newest",
        "not rolled back"
    );
    assert_eq!(
        git(&fx.origin, &["log", "-1", "--format=%s", "refs/heads/main"]),
        "resync by a newer host"
    );
    assert_eq!(fx.forge.ref_sha(CLAIM_REF), None, "released");
    assert!(pass.alerts.is_empty());
}

// ----------------------------------------------------------------------------
// Failure, backoff, alert
// ----------------------------------------------------------------------------

#[test]
fn branch_protection_backs_off_at_the_cap_and_alerts_at_once() {
    let fx = Fixture::new(STALE);
    let hook = fx.origin.join("hooks/pre-receive");
    write(
        &hook,
        "#!/bin/sh\necho 'GH006: Protected branch update failed for refs/heads/main.' >&2\nexit 1\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let host = Host::new(&fx, "host-a");
    let before = fx.origin_head();

    let pass = host.pass();
    let report = only(&pass);
    assert_eq!(report.state, WState::W1, "{report:?}");
    let until = t0() + ChronoDuration::hours(6);
    assert_eq!(
        reason(report),
        format!(
            "backoff until {}: push refused by a branch rule: GH006: Protected branch update \
             failed for refs/heads/main.",
            stamp(until)
        )
    );
    assert_eq!(pass.alerts.len(), 1, "alert on the first protection failure");
    let alert = &pass.alerts[0];
    assert_eq!(
        (alert.kind, alert.failures, alert.next_attempt),
        ("branch-protection", 1, until)
    );
    assert_eq!(alert.repo.as_deref(), Some(REPO));
    assert!(alert.detail.contains("GH006"));
    assert_eq!(fx.origin_head(), before);
    assert_eq!(fx.forge.ref_sha(CLAIM_REF), None, "claim released");
    assert!(host.resync_worktrees().is_empty());

    // Inside the backoff nothing is tried: no fetch, no claim, no new alert.
    let calls = fx.forge.calls.borrow().len();
    host.now.set(t0() + ChronoDuration::hours(5));
    let waiting = host.pass();
    assert_eq!(only(&waiting).state, WState::W1);
    assert!(reason(only(&waiting)).starts_with("backoff until "));
    assert!(waiting.alerts.is_empty());
    assert_eq!(fx.forge.calls.borrow().len(), calls);

    // After the backoff the rule still refuses. That is logged, and the repo
    // backs off again, but a person is told once per repo, not every 6h.
    host.now.set(until + ChronoDuration::seconds(1));
    let again = host.pass();
    assert!(reason(only(&again)).contains("GH006"), "{again:?}");
    assert!(again.alerts.is_empty(), "no second alert for the same repo");
    assert_eq!(host.memory.borrow().backoff[&host.root].failures, 2);

    // The rule is lifted: after the backoff the resync lands and clears it.
    fs::remove_file(&hook).unwrap();
    host.now
        .set(until + ChronoDuration::hours(6) + ChronoDuration::seconds(2));
    assert_eq!(only(&host.pass()).state, WState::W0);
    assert!(host.memory.borrow().backoff.is_empty(), "cleared on success");
}

#[test]
fn other_failures_double_the_backoff_and_alert_on_the_third() {
    assert_eq!(Memory::delay(INTERVAL, 1), Duration::from_secs(60));
    assert_eq!(Memory::delay(INTERVAL, 2), Duration::from_secs(120));
    assert_eq!(Memory::delay(INTERVAL, 3), Duration::from_secs(240));
    assert_eq!(Memory::delay(INTERVAL, 9), Duration::from_secs(15_360));
    assert_eq!(Memory::delay(INTERVAL, 10), BACKOFF_CAP, "capped at 6h");
    assert_eq!(Memory::delay(INTERVAL, 400), BACKOFF_CAP);

    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let mut alerts = Vec::new();
    for (n, wait) in [(1u32, 60), (2, 120), (3, 240)] {
        // The forge is down for the claim.
        fx.forge
            .fault("POST repos/acme/app/git/commits", Fault::Status(503));
        let pass = host.pass();
        let report = only(&pass);
        assert_eq!(report.state, WState::W1);
        let until = host.now.get() + ChronoDuration::seconds(wait);
        assert!(
            reason(report).starts_with(&format!("backoff until {}: claim: ", stamp(until))),
            "failure {n}: {report:?}"
        );
        assert!(reason(report).contains("503"));
        alerts.push(pass.alerts.len());
        host.now.set(until + ChronoDuration::seconds(1));
    }
    assert_eq!(alerts, vec![0, 0, 1], "alert on the third consecutive failure");

    // The forge is back: the resync lands and the count starts over.
    assert_eq!(only(&host.pass()).state, WState::W0);
    assert!(host.memory.borrow().backoff.is_empty());
}

// ----------------------------------------------------------------------------
// Skips, modes, bounds
// ----------------------------------------------------------------------------

#[test]
fn the_source_repo_a_non_github_remote_and_an_uninstalled_repo_are_skipped() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");

    let source = fx.clone_as("loom-source");
    write(&source.join(".loom-source"), "");
    let elsewhere = fx.clone_as("gitea-hosted");
    write(&elsewhere.join(".not-github"), "");
    fx.push_from_seed("uninstall loom", |seed| {
        fs::remove_file(seed.join(INSTALL_METADATA_PATH)).unwrap();
    });

    let roots = [source, elsewhere, host.root.clone()];
    let pass = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
    let found: Vec<(WState, &str)> = pass
        .workspaces
        .iter()
        .map(|w| (w.state, reason(w)))
        .collect();
    assert_eq!(
        found,
        vec![
            (WState::Skipped, "the Loom source repo installs from its own tree"),
            (WState::Skipped, "forge-unsupported: origin is not a GitHub remote"),
            (WState::Skipped, "Loom is not installed on the default branch"),
        ]
    );
    assert!(fx.forge.calls.borrow().is_empty());
    assert!(pass.alerts.is_empty());
}

#[test]
fn a_repo_this_host_may_not_write_is_reported_and_not_claimed() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    write(&host.root.join(".read-only-credential"), "");
    let before = fx.origin_head();
    for _ in 0..2 {
        let pass = host.pass();
        let report = only(&pass);
        assert_eq!(report.state, WState::W1);
        assert_eq!(
            reason(report),
            "not written from this host: the credential has READ on acme/app, not WRITE"
        );
        // This host's standing, not a failure of the repo: another host may
        // be able to write it.
        assert!(pass.alerts.is_empty());
    }
    assert!(host.memory.borrow().backoff.is_empty());
    assert!(fx.forge.calls.borrow().is_empty(), "no claim without write scope");
    assert_eq!(fx.origin_head(), before);
}

#[test]
fn check_mode_classifies_and_never_claims_or_writes() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let before = fx.origin_head();
    let pass = host.pass_with(Mode::Check, &|| Ok(()), None);
    assert_eq!(only(&pass).state, WState::W1);
    assert_eq!(pass.host, None, "the host is in H0; it just may not write");
    assert!(fx.forge.calls.borrow().is_empty());
    assert_eq!(fx.origin_head(), before);
    assert!(host.resync_worktrees().is_empty());
}

#[test]
fn at_most_one_resync_per_pass() {
    let fx = Fixture::new(STALE);
    let other = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let second = other.clone_as("second-repo");
    let roots = [host.root.clone(), second];

    let first = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
    let states: Vec<WState> = first.workspaces.iter().map(|w| w.state).collect();
    assert_eq!(states, vec![WState::W0, WState::W1], "one W2, the other waits a tick");
    let next = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
    let states: Vec<WState> = next.workspaces.iter().map(|w| w.state).collect();
    assert_eq!(states, vec![WState::W0, WState::W0]);
}

#[test]
fn the_operators_checkout_is_never_touched_and_a_stale_worktree_is_cleaned() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    // Uncommitted work in the main checkout, on a file the resync changes,
    // plus an untracked file.
    let dirty = host.root.join(".loom/scripts/a.sh");
    fs::write(&dirty, "#!/bin/sh\necho MY LOCAL EDIT\n").unwrap();
    write(&host.root.join("notes.txt"), "scratch\n");
    // What a killed process leaves behind.
    let stale = host
        .root
        .join(".loom/worktrees")
        .join(format!("{WORKTREE_PREFIX}999999"));
    write(&stale.join("leftover"), "x\n");

    // The operator's last fetch, which a fetch of ours must not overwrite.
    // The branch moves first, so this pass does have to fetch.
    fx.push_from_seed("unrelated work", |seed| write(&seed.join("src.txt"), "x\n"));
    let fetch_head = host.root.join(".git/FETCH_HEAD");
    fs::write(&fetch_head, "the operator's own\n").unwrap();

    let head = git(&host.root, &["rev-parse", "HEAD"]);
    let branch = git(&host.root, &["rev-parse", "refs/heads/main"]);
    let status = git(&host.root, &["status", "--porcelain"]);

    assert_eq!(only(&host.pass()).state, WState::W0);

    assert_eq!(fs::read(&dirty).unwrap(), b"#!/bin/sh\necho MY LOCAL EDIT\n");
    assert_eq!(fs::read(host.root.join("notes.txt")).unwrap(), b"scratch\n");
    assert_eq!(git(&host.root, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&host.root, &["rev-parse", "refs/heads/main"]), branch, "not fast-forwarded");
    assert_eq!(git(&host.root, &["status", "--porcelain"]), status);
    assert_eq!(fs::read(&fetch_head).unwrap(), b"the operator's own\n", "FETCH_HEAD is theirs");
    assert!(!stale.exists(), "the leftover worktree is removed");
    assert!(host.resync_worktrees().is_empty());
    assert_eq!(
        fx.origin_file(".loom/scripts/a.sh"),
        "#!/bin/sh\necho new",
        "origin got the payload"
    );
}

// ----------------------------------------------------------------------------
// Status
// ----------------------------------------------------------------------------

#[test]
fn status_shows_each_workspace_and_an_old_snapshot_still_reads() {
    let pass = WorkspacePass {
        running: RUNNING.to_string(),
        host: Some(NotCurrent::Draining.host_note()),
        workspaces: vec![
            WorkspaceReport {
                root: PathBuf::from("/src/app"),
                repo: Some("acme/app".to_string()),
                state: WState::W1,
                installed: Some("0.19.800".to_string()),
                requires_daemon: None,
                reason: Some("claim held by host-b since 2026-10-08T12:00:00Z".to_string()),
            },
            WorkspaceReport {
                root: PathBuf::from("/src/lib"),
                repo: None,
                state: WState::RepoAhead,
                installed: None,
                requires_daemon: None,
                reason: None,
            },
        ],
        ..WorkspacePass::default()
    };
    assert_eq!(
        pass.lines(),
        vec![
            "  workspaces: host not H0: draining; reporting only",
            "  workspace acme/app: W1 (claim held by host-b since 2026-10-08T12:00:00Z), \
             installed 0.19.800",
            "  workspace /src/lib: repo-ahead",
        ]
    );
    let json = serde_json::to_value(&pass).unwrap();
    assert_eq!(json["workspaces"][1]["state"], "repo-ahead");
    assert_eq!(json["workspaces"][0]["state"], "W1");
    assert!(json.get("alerts").is_none());
    assert_eq!(serde_json::from_value::<WorkspacePass>(json).unwrap(), pass);
    assert!(WorkspacePass::default().is_unset());
    assert!(WorkspacePass::default().lines().is_empty());
}

#[path = "workspace_resync_bounds.rs"]
mod bounds;
#[path = "workspace_resync_heads.rs"]
mod head_check;
