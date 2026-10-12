//! Issue #10661 item 1: `stop` on an already-missing container races a
//! reconcile recreate. `RacingRunner` is the lifecycle's `FakeRunner` with
//! one twist: the first `inspect` of the container reports it missing, and a
//! reconcile pass's `docker run` lands right after (so `stop` has already
//! decided there is nothing to stop).

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct RacingRunner {
    inner: FakeRunner,
    inspects: AtomicUsize,
    /// The recreated container has an in-flight exec.
    recreated_busy: bool,
}

impl ContainerRunner for RacingRunner {
    fn inspect(&self, container: &str) -> Result<Option<ContainerState>> {
        let seen = self.inner.inspect(container)?;
        if self.inspects.fetch_add(1, Ordering::SeqCst) == 0 {
            assert!(seen.is_none(), "the fixture starts with no container");
            // The pass's `docker run`, after this read and before the next.
            self.inner.seed_running(container);
            self.inner.set_busy(container, self.recreated_busy);
        }
        Ok(seen)
    }
    fn create(&self, c: &str, i: &str, h: &Path, w: &Path, d: &Path) -> Result<()> {
        self.inner.create(c, i, h, w, d)
    }
    fn start_existing(&self, container: &str) -> Result<()> {
        self.inner.start_existing(container)
    }
    fn has_active_exec(&self, container: &str) -> Result<bool> {
        self.inner.has_active_exec(container)
    }
    fn stop_and_remove(&self, container: &str, grace: Duration) -> Result<()> {
        self.inner.stop_and_remove(container, grace)
    }
    fn attach_interactive(&self, container: &str, tmux: &str) -> Result<i32> {
        self.inner.attach_interactive(container, tmux)
    }
    fn exec_capture(&self, c: &str, argv: &[&str], t: Duration) -> Result<ExecOutput> {
        self.inner.exec_capture(c, argv, t)
    }
    fn window_exists(&self, c: &str, s: &str, w: &str) -> Result<bool> {
        self.inner.window_exists(c, s, w)
    }
    fn new_window(&self, c: &str, s: &str, w: &str, cwd: &Path, cmd: &[&str]) -> Result<()> {
        self.inner.new_window(c, s, w, cwd, cmd)
    }
    fn select_window(&self, c: &str, s: &str, w: &str) -> Result<()> {
        self.inner.select_window(c, s, w)
    }
}

#[test]
#[serial]
fn stop_with_no_container_racing_a_recreate_ends_with_no_container_and_the_hold() {
    let (workspace, root, _env) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), RacingRunner::default(), None);
    let status = lifecycle.stop("alice", false).unwrap();
    let container = container_name("alice");
    assert!(!status.running, "{status:?}");
    assert!(status.container_id.is_none(), "{status:?}");
    assert!(status.held, "{status:?}");
    assert!(lifecycle
        .runner
        .inner
        .inspect(&container)
        .unwrap()
        .is_none());
    assert_eq!(*lifecycle.runner.inner.stops.lock().unwrap(), vec![container]);
    assert!(root
        .path()
        .join("alice")
        .join(session_hold::HOLD_FILE)
        .exists());
}

#[test]
#[serial]
fn a_busy_recreated_container_is_refused_without_force_and_the_hold_stays() {
    let (workspace, root, _env) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let runner = RacingRunner {
        recreated_busy: true,
        ..RacingRunner::default()
    };
    let lifecycle = SessionLifecycle::new(workspace.path(), runner, None);
    let error = lifecycle.stop("alice", false).unwrap_err().to_string();
    assert!(error.contains("in-flight"), "{error}");
    // Never a hard stop of active work; the hold keeps the pass away and a
    // retry (or `--force`) finishes the stop.
    assert!(lifecycle.runner.inner.stops.lock().unwrap().is_empty());
    let status = lifecycle.status("alice").unwrap();
    assert!(status.running && status.held, "{status:?}");
    let status = lifecycle.stop("alice", true).unwrap();
    assert!(!status.running && status.held, "{status:?}");
}

#[test]
#[serial]
fn force_stops_a_busy_recreated_container() {
    let (workspace, root, _env) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let runner = RacingRunner {
        recreated_busy: true,
        ..RacingRunner::default()
    };
    let lifecycle = SessionLifecycle::new(workspace.path(), runner, None);
    let status = lifecycle.stop("alice", true).unwrap();
    assert!(!status.running && status.held, "{status:?}");
    assert_eq!(lifecycle.runner.inner.stops.lock().unwrap().len(), 1);
}
