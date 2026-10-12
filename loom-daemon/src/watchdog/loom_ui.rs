//! The watchdog's loom-ui output (#11295): `daemon.down` once recovery is
//! exhausted, and `watchdog.peer-coord-degraded`, as incidents on loom-ui's
//! Alerts page beside the forge issue each already files.
//!
//! These are the only alerts that still work when the daemon itself is dead,
//! so they come from this separate one-shot process, with the daemon's ingest
//! key and URL (resolved from the marker's `repo_root` config, like the rest
//! of the tick). Each POST is synchronous and bounded by [`POST_TIMEOUT`].
//!
//! A `<sentinel>.loom-ui` file records an `open` loom-ui has: it is written
//! only when loom-ui answered (2xx, or a 4xx that retrying cannot fix), so an
//! unanswered `open` is simply retried on the next tick, and removed once the
//! `resolved` is answered, so a healthy tick posts nothing at all.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::loom_ui_alerts::{self, Alert, Delivery, Severity, State, Target};

/// Bound on one POST: a tick must stay short.
pub const POST_TIMEOUT: Duration = Duration::from_secs(10);
/// Rule for an outage recovery could not end.
pub const RULE_DAEMON_DOWN: &str = "daemon.down";
/// Rule for degraded peer-claim coordination.
pub const RULE_PEER_COORD: &str = "watchdog.peer-coord-degraded";

/// The "loom-ui has this open" record kept beside `sentinel`.
#[must_use]
pub fn record_for(sentinel: &Path) -> PathBuf {
    let mut name = sentinel.as_os_str().to_os_string();
    name.push(".loom-ui");
    PathBuf::from(name)
}

/// Post `alert` if loom-ui does not have it yet (`open`) or still has it
/// (`resolved`), keeping `record` in step. `None` when nothing was owed.
pub fn sync(
    record: &Path,
    alert: &Alert,
    post: impl FnOnce(&Alert) -> Delivery,
) -> Option<Delivery> {
    let open = alert.state == State::Open;
    if record.exists() == open {
        return None;
    }
    let delivery = post(alert);
    let answered = !matches!(delivery, Delivery::Retry(_));
    if answered {
        if open {
            if let Some(dir) = record.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(record, format!("{}\n", chrono::Utc::now().to_rfc3339()));
        } else {
            let _ = std::fs::remove_file(record);
        }
    }
    Some(delivery)
}

/// The target for this tick, resolved at most once per process.
fn target() -> Option<&'static Target> {
    static TARGET: OnceLock<Option<Target>> = OnceLock::new();
    TARGET
        .get_or_init(|| {
            let paths = super::config::Paths::from_env();
            let root = super::marker::get_nonempty(&paths.marker, "repo_root")
                .map(PathBuf::from)
                .or_else(|| std::env::var("LOOM_WORKSPACE").ok().map(PathBuf::from))
                .or_else(|| std::env::current_dir().ok())?;
            loom_ui_alerts::resolve_target(&root).ok()
        })
        .as_ref()
}

fn send(record: &Path, alert: &Alert) {
    // Nothing owed: decided before resolving config, so a healthy tick reads
    // no file and opens no connection.
    if record.exists() == (alert.state == State::Open) {
        return;
    }
    let Some(target) = target() else { return };
    if let Some(Delivery::Dropped(why) | Delivery::Retry(why)) =
        sync(record, alert, |a| loom_ui_alerts::post_blocking(target, a, POST_TIMEOUT))
    {
        log::warn!("watchdog: loom-ui {} {}: {why}", alert.rule, alert.state.as_str());
    }
}

/// `daemon.down` is open: recovery is exhausted.
pub fn daemon_down(escalation_sentinel: &Path, hostname: &str, detail: &str) {
    let alert = Alert::new(
        RULE_DAEMON_DOWN,
        "loom-daemon",
        super::escalate::title(hostname),
        Severity::Critical,
        State::Open,
    )
    .with_detail(detail);
    send(&record_for(escalation_sentinel), &alert);
}

/// `daemon.down` is over (any tick that ends the outage episode).
pub fn daemon_up(escalation_sentinel: &Path) {
    let alert = Alert::new(
        RULE_DAEMON_DOWN,
        "loom-daemon",
        "loom-daemon is running again",
        Severity::Critical,
        State::Resolved,
    );
    send(&record_for(escalation_sentinel), &alert);
}

/// Peer-claim coordination degraded (`open`) or green again (`resolved`).
pub fn peer_coord(peer_coord_sentinel: &Path, degraded: bool, hostname: &str, summary: &str) {
    let (state, title) = if degraded {
        (State::Open, format!("Peer-claim coordination is DEGRADED on {hostname}"))
    } else {
        (State::Resolved, format!("Peer-claim coordination recovered on {hostname}"))
    };
    let alert = Alert::new(RULE_PEER_COORD, "peer-claims", title, Severity::Critical, state)
        .with_detail(summary);
    send(&record_for(peer_coord_sentinel), &alert);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(state: State) -> Alert {
        Alert::new(RULE_DAEMON_DOWN, "loom-daemon", "down", Severity::Critical, state)
    }

    #[test]
    fn open_posts_once_then_holds_until_resolved() {
        let d = tempfile::tempdir().unwrap();
        let record = record_for(&d.path().join("sentinel"));
        let mut posts = Vec::new();
        let mut post = |a: &Alert| {
            posts.push(a.state);
            Delivery::Delivered
        };
        assert_eq!(sync(&record, &alert(State::Open), &mut post), Some(Delivery::Delivered));
        assert!(record.exists());
        // Later ticks of the same outage: nothing owed.
        assert_eq!(sync(&record, &alert(State::Open), &mut post), None);
        assert_eq!(sync(&record, &alert(State::Resolved), &mut post), Some(Delivery::Delivered));
        assert!(!record.exists());
        // Healthy ticks after that: nothing owed.
        assert_eq!(sync(&record, &alert(State::Resolved), &mut post), None);
        assert_eq!(posts, vec![State::Open, State::Resolved]);
    }

    #[test]
    fn an_unanswered_post_is_retried_next_tick() {
        let d = tempfile::tempdir().unwrap();
        let record = record_for(&d.path().join("sentinel"));
        let retry = |_: &Alert| Delivery::Retry("HTTP 503".into());
        assert!(sync(&record, &alert(State::Open), retry).is_some());
        assert!(!record.exists(), "a 5xx leaves the open owed");
        assert!(sync(&record, &alert(State::Open), |_| Delivery::Delivered).is_some());
        assert!(record.exists());
        assert!(sync(&record, &alert(State::Resolved), retry).is_some());
        assert!(record.exists(), "a 5xx leaves the resolve owed");
    }

    #[test]
    fn a_refusal_is_not_retried_every_tick() {
        let d = tempfile::tempdir().unwrap();
        let record = record_for(&d.path().join("sentinel"));
        let refused = |_: &Alert| Delivery::Dropped("HTTP 403".into());
        assert!(sync(&record, &alert(State::Open), refused).is_some());
        assert!(record.exists());
        assert_eq!(sync(&record, &alert(State::Open), refused), None);
    }

    #[test]
    fn the_record_sits_beside_its_sentinel() {
        assert_eq!(
            record_for(Path::new("/x/.watchdog-outage-escalated")),
            PathBuf::from("/x/.watchdog-outage-escalated.loom-ui")
        );
    }
}
