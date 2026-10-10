//! Tests for [`super`] (#10802): injected systemctl and process-table fakes.

use std::cell::RefCell;
use std::sync::Mutex;

use super::*;
use crate::auto_update::pause_roll::teardown::parse_ps;

const ME: u32 = 100;

fn scope(name: &str, state: &str, age: Option<u64>) -> ScopeInfo {
    ScopeInfo {
        name: name.to_string(),
        active_state: state.to_string(),
        age_secs: age,
        result: Some("exit-code".to_string()),
        exec_main_status: Some("1".to_string()),
    }
}

#[derive(Default)]
struct FakeCtl {
    scopes: Vec<ScopeInfo>,
    calls: RefCell<Vec<String>>,
}

impl SystemCtl for FakeCtl {
    fn list_scopes(&self) -> Vec<ScopeInfo> {
        self.scopes.clone()
    }
    fn stop(&self, unit: &str) -> Result<(), String> {
        self.calls.borrow_mut().push(format!("stop {unit}"));
        Ok(())
    }
    fn reset_failed(&self, unit: &str) -> Result<(), String> {
        self.calls.borrow_mut().push(format!("reset {unit}"));
        Ok(())
    }
}

fn proc_at(pid: u32, ppid: u32, cwd: Option<&str>, cmd: &str, age: u64) -> ProcEntry {
    ProcEntry {
        pid,
        ppid,
        cwd: cwd.map(PathBuf::from),
        cmdline: cmd.to_string(),
        age_secs: Some(age),
    }
}

/// Runs `body` with hooks that record every signal.
fn with_hooks<R>(body: impl FnOnce(&ReapHooks<'_>, &RefCell<Vec<(u32, i32)>>) -> R) -> R {
    let sent: RefCell<Vec<(u32, i32)>> = RefCell::new(Vec::new());
    let signal = |pid: u32, sig: i32| {
        sent.borrow_mut().push((pid, sig));
        true
    };
    let snapshot = Vec::new;
    let hooks = ReapHooks {
        signal: &signal,
        snapshot: &snapshot,
        is_alive: &|_| false,
        sleep: &|_| {},
    };
    body(&hooks, &sent)
}

fn killed(sent: &RefCell<Vec<(u32, i32)>>) -> Vec<u32> {
    let mut pids: Vec<u32> = sent
        .borrow()
        .iter()
        .filter(|(_, s)| *s == libc::SIGTERM)
        .map(|(p, _)| *p)
        .collect();
    pids.sort_unstable();
    pids
}

fn request(started_ago: i64, scope_unit: Option<&str>) -> ExitRequest {
    ExitRequest {
        sweep_id: "sweep-1".to_string(),
        issue: Some(7),
        pid: 4242,
        pgid: Some(4242),
        started_at: Utc::now() - chrono::Duration::seconds(started_ago),
        workspace_root: PathBuf::from("/repo"),
        scope_unit: scope_unit.map(str::to_string),
    }
}

/// Every process carries the run's marker: the start-time cases.
fn run_exit(
    req: &ExitRequest,
    ctl: &dyn SystemCtl,
    procs: Vec<ProcEntry>,
    other_claim: bool,
    dry_run: bool,
) -> (Vec<ReapRecord>, Vec<u32>) {
    let all: Vec<u32> = procs.iter().map(|p| p.pid).collect();
    run_exit_marked(req, ctl, procs, &all, other_claim, dry_run)
}

/// Only `marked` carry `LOOM_SWEEP_ID=<req.sweep_id>`.
fn run_exit_marked(
    req: &ExitRequest,
    ctl: &dyn SystemCtl,
    procs: Vec<ProcEntry>,
    marked: &[u32],
    other_claim: bool,
    dry_run: bool,
) -> (Vec<ReapRecord>, Vec<u32>) {
    let recorder: Mutex<Vec<ReapRecord>> = Mutex::new(Vec::new());
    let sweep = req.sweep_id.clone();
    let carries = move |pid: u32, id: &str| id == sweep && marked.contains(&pid);
    let pids = with_hooks(|hooks, sent| {
        run_exit_teardown(
            req,
            &ExitPorts {
                ctl,
                procs: &|| procs.clone(),
                hooks,
                identity_leg: &|_| TeardownReport::default(),
                recorder: &recorder,
                other_claim: &move || other_claim,
                carries_marker: &carries,
                now: Utc::now(),
                me: ME,
                dry_run,
            },
        );
        killed(sent)
    });
    (recorder.into_inner().unwrap(), pids)
}

// ---- unit names -----------------------------------------------------------

#[test]
fn only_real_agent_scopes_are_agent_scopes() {
    assert!(is_agent_scope("loom-agent-123-456.scope"));
    assert!(!is_agent_scope("loom-agent-probe-123-456.scope"));
    assert!(!is_agent_scope("nginx.service"));
    assert!(!is_agent_scope("loom-daemon.service"));
    assert!(!is_agent_scope("loom-agent-123.service"));
    assert_eq!(embedded_pid("loom-agent-123-456.scope"), Some(123));
    assert_eq!(embedded_pid("loom-agent-probe-123-456.scope"), None);
    assert_eq!(embedded_pid("loom-agent-issue_5.scope"), None);
}

#[test]
fn list_units_output_is_parsed_and_filtered() {
    let out = "loom-agent-1-2.scope loaded active running A\n\
               ● loom-agent-3-4.scope loaded failed failed B\n\
               loom-agent-probe-5-6.scope loaded active running C\n\
               other.scope loaded active running D\n";
    assert_eq!(
        parse_list_units(out),
        vec![
            ("loom-agent-1-2.scope".to_string(), "active".to_string()),
            ("loom-agent-3-4.scope".to_string(), "failed".to_string())
        ]
    );
}

// ---- periodic scope pass --------------------------------------------------

#[test]
fn unclaimed_scope_with_a_dead_pid_is_stopped_but_claimed_and_probe_are_not() {
    let scopes = vec![
        scope("loom-agent-10-1.scope", "active", Some(7200)), // dead pid
        scope("loom-agent-11-1.scope", "active", Some(7200)), // live pid
        scope("loom-agent-item_x.scope", "active", Some(7200)), // recorded unit
        scope("loom-agent-probe-12-1.scope", "active", Some(7200)),
        scope("loom-agent-13-1.scope", "active", Some(60)), // too young
        scope("loom-agent-14-1.scope", "active", None),     // age unknown
    ];
    let claimed = HashSet::from(["loom-agent-item_x.scope".to_string()]);
    let actions = plan_scopes(&scopes, &claimed, &|pid, _| pid == 11, 1800);
    assert_eq!(
        actions,
        vec![ScopeAction::Stop {
            unit: "loom-agent-10-1.scope".to_string()
        }]
    );
}

#[test]
fn only_failed_agent_scopes_are_reset() {
    let scopes = vec![
        scope("loom-agent-1-1.scope", "failed", None),
        scope("nginx.service", "failed", None),
        scope("loom-agent-probe-2-2.scope", "failed", None),
    ];
    let actions = plan_scopes(&scopes, &HashSet::new(), &|_, _| true, 1800);
    assert_eq!(actions.len(), 1);
    assert!(
        matches!(&actions[0], ScopeAction::ResetFailed { unit, .. } if unit == "loom-agent-1-1.scope")
    );
}

#[test]
fn failed_scope_outcome_is_recorded_before_the_reset() {
    struct Order<'a>(&'a RefCell<Vec<String>>);
    impl Recorder for Order<'_> {
        fn record(&self, r: &ReapRecord) {
            self.0.borrow_mut().push(format!("record {:?}", r.detail));
        }
    }
    struct Ctl<'a>(&'a RefCell<Vec<String>>);
    impl SystemCtl for Ctl<'_> {
        fn list_scopes(&self) -> Vec<ScopeInfo> {
            Vec::new()
        }
        fn stop(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn reset_failed(&self, u: &str) -> Result<(), String> {
            self.0.borrow_mut().push(format!("reset {u}"));
            Ok(())
        }
    }
    let log = RefCell::new(Vec::new());
    let action = ScopeAction::ResetFailed {
        unit: "loom-agent-1-1.scope".to_string(),
        result: Some("signal".to_string()),
        exec_main_status: Some("9".to_string()),
    };
    run_scope_actions(&[action], &Ctl(&log), &Order(&log), false);
    let log = log.into_inner();
    assert_eq!(log.len(), 2);
    assert!(log[0].starts_with("record") && log[0].contains("Result=signal"));
    assert_eq!(log[1], "reset loom-agent-1-1.scope");
}

#[test]
fn dry_run_records_but_stops_and_resets_nothing() {
    let ctl = FakeCtl::default();
    let recorder: Mutex<Vec<ReapRecord>> = Mutex::new(Vec::new());
    let actions = vec![
        ScopeAction::Stop {
            unit: "loom-agent-1-1.scope".to_string(),
        },
        ScopeAction::ResetFailed {
            unit: "loom-agent-2-2.scope".to_string(),
            result: None,
            exec_main_status: None,
        },
    ];
    run_scope_actions(&actions, &ctl, &recorder, true);
    assert!(ctl.calls.borrow().is_empty());
    let recs = recorder.into_inner().unwrap();
    assert_eq!(recs.len(), 2);
    assert!(recs.iter().all(|r| r.dry_run));
}

#[test]
fn scope_pid_must_not_have_started_after_the_scope() {
    let now = Utc::now();
    assert!(pid_matches_scope(Some(now - chrono::Duration::seconds(500)), 400, now));
    assert!(!pid_matches_scope(Some(now - chrono::Duration::seconds(10)), 400, now));
    assert!(pid_matches_scope(None, 400, now));
}

// ---- exit teardown --------------------------------------------------------

#[test]
fn exit_stops_the_scope_and_reaps_a_setsid_dev_server() {
    // Linux: the scope is stopped; a leftover `vite` that escaped the process
    // group is reaped by worktree attribution.
    let ctl = FakeCtl {
        scopes: vec![scope("loom-agent-4242-99.scope", "active", Some(590))],
        ..FakeCtl::default()
    };
    let procs = vec![
        proc_at(1, 0, Some("/"), "init", 99_999),
        proc_at(500, 1, Some("/repo/.loom/worktrees/issue-7/app"), "node vite --port 5173", 300),
        proc_at(501, 500, Some("/repo/.loom/worktrees/issue-7/app"), "workerd serve", 299),
    ];
    let (recs, pids) = run_exit(&request(600, None), &ctl, procs, false, false);
    assert_eq!(*ctl.calls.borrow(), vec!["stop loom-agent-4242-99.scope"]);
    assert_eq!(pids, vec![500, 501]);
    assert!(recs
        .iter()
        .any(|r| r.kind == ResidueKind::Scope && r.path == ResiduePath::Exit));
    let tree = recs.iter().find(|r| r.kind == ResidueKind::Tree).unwrap();
    assert_eq!(tree.issue, Some(7));
    assert_eq!(tree.sweep_id.as_deref(), Some("sweep-1"));
    assert!(!tree.dry_run);
}

#[test]
fn exit_prefers_the_recorded_scope_unit() {
    let ctl = FakeCtl {
        scopes: vec![
            scope("loom-agent-item_9.scope", "active", Some(5)),
            scope("loom-agent-4242-99.scope", "active", Some(5)),
        ],
        ..FakeCtl::default()
    };
    run_exit(&request(600, Some("loom-agent-item_9.scope")), &ctl, vec![], false, false);
    assert_eq!(*ctl.calls.borrow(), vec!["stop loom-agent-item_9.scope"]);
}

#[test]
fn exit_ignores_an_older_scope_wearing_the_same_pid() {
    let ctl = FakeCtl {
        scopes: vec![scope("loom-agent-4242-99.scope", "active", Some(86_400))],
        ..FakeCtl::default()
    };
    run_exit(&request(600, None), &ctl, vec![], false, false);
    assert!(ctl.calls.borrow().is_empty());
}

#[test]
fn exit_without_systemd_reaps_the_tree_from_the_ps_snapshot() {
    // macOS / scope-less: a `ps` table (no cwd), attribution by argv.
    let out = "  200     1   200 Ss   ??     00:05:00 /bin/zsh\n\
               \x20 500     1   500 S    ??     04:00 node /repo/.loom/worktrees/issue-7/node_modules/.bin/vite\n\
               \x20 501   500   500 S    ??     03:59 workerd serve /repo/.loom/worktrees/issue-7/w.json\n";
    let procs = entries_from_table(&parse_ps(out, Utc::now()), Utc::now());
    assert!(procs.iter().all(|p| p.cwd.is_none()));
    let (recs, pids) = run_exit(&request(600, None), &NoSystemd, procs, false, false);
    assert_eq!(pids, vec![500, 501]);
    assert!(recs.iter().all(|r| r.kind == ResidueKind::Tree));
}

#[test]
fn exit_leaves_an_operator_process_that_predates_the_agent() {
    let procs = vec![
        // An operator shell opened in the worktree before the agent started.
        proc_at(300, 1, Some("/repo/.loom/worktrees/issue-7"), "vim notes.md", 5000),
        proc_at(500, 1, Some("/repo/.loom/worktrees/issue-7"), "node vite", 100),
    ];
    let (_, pids) = run_exit(&request(600, None), &FakeCtl::default(), procs, false, false);
    assert_eq!(pids, vec![500]);
}

/// Judge finding 1 (#11238): an operator shell or editor opened in the
/// worktree AFTER the agent started passes the start-time bound, but carries
/// no `LOOM_SWEEP_ID` of this run, so it is not the run's and is left alone,
/// children included. The run's own dev server beside it is still reaped.
#[test]
fn exit_leaves_an_operator_process_started_after_the_agent() {
    let wt = Some("/repo/.loom/worktrees/issue-7");
    let procs = vec![
        proc_at(500, 1, wt, "node vite", 300),
        proc_at(700, 1, wt, "bash", 120), // operator's shell, unmarked
        proc_at(701, 700, wt, "vim notes.md", 110),
        proc_at(702, 700, Some("/tmp"), "cargo build", 100),
    ];
    let (recs, pids) =
        run_exit_marked(&request(600, None), &FakeCtl::default(), procs, &[500], false, false);
    assert_eq!(pids, vec![500]);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].pids, vec![500]);
}

#[test]
fn exit_without_ownership_evidence_reaps_nothing_from_the_worktree() {
    // Unreadable environments (or another run's marker) are never ownership.
    let wt = Some("/repo/.loom/worktrees/issue-7");
    let procs = vec![proc_at(500, 1, wt, "node vite", 300)];
    let (recs, pids) =
        run_exit_marked(&request(600, None), &FakeCtl::default(), procs, &[], false, false);
    assert!(pids.is_empty() && recs.is_empty());
}

#[test]
fn the_marker_must_name_this_run_exactly() {
    let env = "PATH=/bin\0LOOM_SWEEP_ID=sweep-1\0HOME=/h";
    assert!(marker_in(env.split('\0'), "sweep-1"));
    assert!(!marker_in(env.split('\0'), "sweep-12"));
    assert!(!marker_in("LOOM_SWEEP_ID=sweep-12".split('\0'), "sweep-1"));
    assert!(!marker_in("LOOM_SWEEP_ID=".split('\0'), ""));
    // macOS `ps -E`: argv then environment, whitespace-separated.
    assert!(marker_in(
        "node vite LOOM_SWEEP_ID=sweep-1 HOME=/h".split_whitespace(),
        "sweep-1"
    ));
}

#[test]
fn a_role_exit_request_has_no_worktree_leg() {
    let at = Utc::now();
    let req = ExitRequest::for_role(
        Path::new("/repo"),
        "role-judge-x",
        77,
        at,
        Some("loom-agent-role-judge-x.scope".to_string()),
    );
    assert_eq!((req.issue, req.pgid, req.pid), (None, Some(77), 77));
    let ctl = FakeCtl {
        scopes: vec![scope("loom-agent-role-judge-x.scope", "active", Some(5))],
        ..FakeCtl::default()
    };
    let procs = vec![proc_at(
        500,
        1,
        Some("/repo/.loom/worktrees/issue-7"),
        "node",
        1,
    )];
    let (_, pids) = run_exit(&req, &ctl, procs, false, false);
    assert_eq!(*ctl.calls.borrow(), vec!["stop loom-agent-role-judge-x.scope"]);
    assert!(pids.is_empty());
}

#[test]
fn exit_leaves_a_process_outside_the_worktree_and_a_newer_agent() {
    let outside = vec![proc_at(500, 1, Some("/repo/src"), "node vite", 100)];
    let (recs, pids) = run_exit(&request(600, None), &FakeCtl::default(), outside, false, false);
    assert!(pids.is_empty() && recs.is_empty());

    // A re-dispatched agent already runs in the worktree: hard stop.
    let newer = vec![
        proc_at(600, 1, Some("/repo/.loom/worktrees/issue-7"), "claude -p /loom:sweep 7", 20),
        proc_at(601, 600, Some("/repo/.loom/worktrees/issue-7"), "node vite", 10),
    ];
    let (_, pids) = run_exit(&request(600, None), &FakeCtl::default(), newer, false, false);
    assert!(pids.is_empty());
    // Or a different run holds the claim.
    let any = vec![proc_at(
        500,
        1,
        Some("/repo/.loom/worktrees/issue-7"),
        "node vite",
        10,
    )];
    let (_, pids) = run_exit(&request(600, None), &FakeCtl::default(), any, true, false);
    assert!(pids.is_empty());
}

#[test]
fn exit_dry_run_records_without_signalling_or_stopping() {
    let ctl = FakeCtl {
        scopes: vec![scope("loom-agent-4242-99.scope", "active", Some(590))],
        ..FakeCtl::default()
    };
    let procs = vec![proc_at(
        500,
        1,
        Some("/repo/.loom/worktrees/issue-7"),
        "node vite",
        100,
    )];
    let (recs, pids) = run_exit(&request(600, None), &ctl, procs, false, true);
    assert!(ctl.calls.borrow().is_empty());
    assert!(pids.is_empty());
    assert_eq!(recs.len(), 2);
    assert!(recs.iter().all(|r| r.dry_run));
}

// ---- periodic worktree pass ----------------------------------------------

fn plan(procs: &[ProcEntry], exists: &[&str], managed: &[&str], claimed: bool) -> WorktreePlan {
    let exists: Vec<PathBuf> = exists.iter().map(PathBuf::from).collect();
    let managed: Vec<PathBuf> = managed.iter().map(PathBuf::from).collect();
    plan_worktree_residue(
        procs,
        &[PathBuf::from("/reg")],
        ME,
        1800,
        &WorktreeProbes {
            exists: &|p| exists.iter().any(|e| e == p),
            is_managed: &|p| managed.iter().any(|e| e == p),
            live_claim: &|_, _| claimed.then(|| "live".to_string()),
        },
    )
}

#[test]
fn processes_in_a_deleted_worktree_are_reaped() {
    let procs = vec![
        proc_at(500, 1, Some("/reg/.loom/worktrees/issue-5 (deleted)"), "node vite", 4000),
        proc_at(501, 500, Some("/reg/.loom/worktrees/issue-5 (deleted)/app"), "workerd", 4000),
    ];
    let plan = plan(&procs, &[], &[], false);
    assert_eq!(plan.trees.len(), 1);
    assert_eq!(plan.trees[0].issue, 5);
    with_hooks(|hooks, sent| {
        let rec: Mutex<Vec<ReapRecord>> = Mutex::new(Vec::new());
        run_worktree_plan(&plan, hooks, &rec, false);
        assert_eq!(killed(sent), vec![500, 501]);
        let recs = rec.into_inner().unwrap();
        assert_eq!(recs[0].kind, ResidueKind::Tree);
        assert_eq!(recs[0].path, ResiduePath::Periodic);
        assert_eq!(recs[0].issue, Some(5));
    });
}

/// Judge finding 2 (#11238): off Linux the snapshot is a `ps` table with no
/// cwd, so the deleted worktree must be found from argv. Same fixture shape
/// as the exit test, run through the periodic plan and reap.
#[test]
fn periodic_pass_finds_a_deleted_worktree_from_the_ps_snapshot() {
    let now = Utc::now();
    let out = "  200     1   200 Ss   ??     05:00:00 /bin/zsh\n\
               \x20 500     1   500 S    ??     02:00:00 node /gone/.loom/worktrees/issue-7/node_modules/.bin/vite\n\
               \x20 501   500   500 S    ??     01:59:59 workerd serve --config=/gone/.loom/worktrees/issue-7/w.json\n\
               \x20 600     1   600 S    ??     02:00:00 vim /reg/.loom/worktrees/issue-8/notes.md\n";
    let procs = entries_from_table(&parse_ps(out, now), now);
    assert!(procs.iter().all(|p| p.cwd.is_none()));
    // issue-7's checkout was deleted; issue-8 lives on in a registered root.
    let plan = plan(&procs, &["/reg/.loom/worktrees/issue-8"], &[], false);
    assert_eq!(plan.trees.len(), 1, "{plan:?}");
    assert_eq!(plan.trees[0].issue, 7);
    with_hooks(|hooks, sent| {
        let rec: Mutex<Vec<ReapRecord>> = Mutex::new(Vec::new());
        run_worktree_plan(&plan, hooks, &rec, false);
        assert_eq!(killed(sent), vec![500, 501]);
        assert_eq!(rec.into_inner().unwrap()[0].path, ResiduePath::Periodic);
    });
}

#[test]
fn argv_paths_are_the_absolute_ones() {
    assert_eq!(
        argv_paths("node --root=/a/b x:/c \"/d e\" rel/f"),
        vec![
            PathBuf::from("/a/b"),
            PathBuf::from("/c"),
            PathBuf::from("/d")
        ]
    );
}

#[test]
fn processes_of_a_deregistered_workspace_are_reaped_but_registered_ones_are_not() {
    let wt = "/gone/.loom/worktrees/issue-9";
    let reg = "/reg/.loom/worktrees/issue-9";
    let procs = vec![
        proc_at(500, 1, Some(wt), "node vite", 4000),
        proc_at(600, 1, Some(reg), "node vite", 4000),
    ];
    let plan = plan(&procs, &[wt, reg], &[wt, reg], false);
    assert_eq!(plan.trees.len(), 1);
    assert_eq!(plan.trees[0].seeds, vec![500]);
}

#[test]
fn deregistered_worktrees_that_are_unmanaged_or_claimed_are_left_alone() {
    let wt = "/gone/.loom/worktrees/issue-9";
    let procs = vec![proc_at(500, 1, Some(wt), "node vite", 4000)];
    assert!(plan(&procs, &[wt], &[], false).trees.is_empty());
    assert!(plan(&procs, &[wt], &[wt], true).trees.is_empty());
}

#[test]
fn an_operator_process_in_the_same_checkout_outside_the_worktrees_is_left_alone() {
    let procs = vec![
        proc_at(500, 1, Some("/gone"), "vim", 99_999),
        proc_at(501, 1, Some("/gone/src"), "node vite", 99_999),
        proc_at(502, 1, Some("/gone/.loom/state"), "tail -f", 99_999),
    ];
    let plan = plan(
        &procs,
        &["/gone/.loom/worktrees/issue-9"],
        &["/gone/.loom/worktrees/issue-9"],
        false,
    );
    assert!(plan.trees.is_empty() && plan.skipped.is_empty());
}

#[test]
fn young_processes_and_live_agents_in_a_deleted_worktree_are_left_alone() {
    let young = vec![proc_at(
        500,
        1,
        Some("/reg/.loom/worktrees/issue-5 (deleted)"),
        "node",
        60,
    )];
    assert!(plan(&young, &[], &[], false).trees.is_empty());
    let agent = vec![
        proc_at(500, 1, Some("/reg/.loom/worktrees/issue-5 (deleted)"), "claude -p x", 4000),
        proc_at(501, 500, Some("/reg/.loom/worktrees/issue-5 (deleted)"), "node", 4000),
    ];
    assert!(plan(&agent, &[], &[], false).trees.is_empty());
}

#[test]
fn worktree_plan_dry_run_signals_nothing() {
    let procs = vec![proc_at(
        500,
        1,
        Some("/reg/.loom/worktrees/issue-5 (deleted)"),
        "node",
        4000,
    )];
    let plan = plan(&procs, &[], &[], false);
    with_hooks(|hooks, sent| {
        let rec: Mutex<Vec<ReapRecord>> = Mutex::new(Vec::new());
        run_worktree_plan(&plan, hooks, &rec, true);
        assert!(sent.borrow().is_empty());
        assert!(rec.into_inner().unwrap()[0].dry_run);
    });
}

// ---- records --------------------------------------------------------------

#[test]
fn every_real_reap_increments_its_counter_and_the_payload_has_the_documented_fields() {
    let mut status = AgentResidueStatus::default();
    let at = Utc::now();
    for (kind, dry) in [
        (ResidueKind::Scope, false),
        (ResidueKind::Tree, false),
        (ResidueKind::Tree, false),
        (ResidueKind::FailedScope, false),
        (ResidueKind::Scope, true),
    ] {
        count(&mut status, &ReapRecord::new(ResiduePath::Exit, kind, dry), at);
    }
    assert_eq!((status.scope, status.tree, status.failed_scope), (1, 2, 1));
    assert_eq!(status.dry_run_planned, 1);
    assert_eq!(status.last_reap_at, Some(at));

    let mut rec = ReapRecord::new(ResiduePath::Periodic, ResidueKind::FailedScope, false);
    rec.unit = Some("loom-agent-1-1.scope".to_string());
    let payload = rec.payload();
    for key in [
        "source", "path", "kind", "unit", "pids", "issue", "sweep_id", "dry_run", "detail",
    ] {
        assert!(payload.get(key).is_some(), "missing {key}");
    }
    assert_eq!(payload["path"], "periodic");
    assert_eq!(payload["kind"], "failed-scope");
}

#[tokio::test]
async fn production_recorder_publishes_on_the_bus() {
    let bus = Arc::new(crate::event_bus::EventBus::new());
    install_event_bus(bus.clone());
    let mut sub = bus.subscribe([EVENT_TOPIC]);
    let before = status_snapshot().scope;
    let mut rec = ReapRecord::new(ResiduePath::Exit, ResidueKind::Scope, false);
    rec.unit = Some("loom-agent-1-1.scope".to_string());
    ProductionRecorder.record(&rec);
    assert!(status_snapshot().scope > before);
    // The bus is process-global; another test may have installed its own.
    if Arc::ptr_eq(BUS.get().unwrap(), &bus) {
        let event = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await;
        assert!(event.is_ok());
    }
}

// ---- real systemd (opt-in) -----------------------------------------------

#[test]
fn real_systemd_scope_is_stopped_with_its_setsid_child() {
    if std::env::var("LOOM_REAL_SYSTEMD_TEST").as_deref() != Ok("1") {
        eprintln!("skipped: set LOOM_REAL_SYSTEMD_TEST=1 to run against systemctl --user");
        return;
    }
    let Some(ctl) = SystemdUser::system() else {
        eprintln!("skipped: systemctl --user is unavailable");
        return;
    };
    let unit = format!("loom-agent-{}-realtest.scope", std::process::id());
    let mut child = Command::new("systemd-run")
        .args([
            "--user", "--scope", "--quiet", "--unit", &unit, "--", "setsid", "sleep", "300",
        ])
        .spawn()
        .unwrap();
    let listed = || {
        ctl.list_scopes()
            .iter()
            .any(|s| s.name == unit && s.active_state == "active")
    };
    let mut tries = 0;
    while !listed() && tries < 50 {
        std::thread::sleep(Duration::from_millis(100));
        tries += 1;
    }
    assert!(listed(), "scope never appeared");
    ctl.stop(&unit).unwrap();
    assert!(!listed());
    let _ = child.wait();
}
