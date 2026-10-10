//! The alert thread and its delivery sinks (#10164).
//!
//! No forge access: status comes from the daemon's own IPC socket, delivery is
//! the event bus. Loom sends no mail (#11087): human-facing notification is
//! derived outside Loom.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::state::{AlertState, Kind, Transition};
use super::{causes, classify, Settings};
use crate::types::{DaemonStatusReport, Request, Response};

const IPC_TIMEOUT: Duration = Duration::from_secs(30);

/// One delivery channel. Each fails independently.
pub trait AlertSink: Send {
    fn name(&self) -> &'static str;
    fn deliver(&self, t: &Transition, host: &str) -> Result<(), String>;
}

/// Human body shared by the sinks.
#[must_use]
pub fn render_body(t: &Transition) -> String {
    match t.kind {
        Kind::Started => format!("{}\nFix: {}", t.headline, t.fix),
        Kind::Reminder => format!("STILL DEGRADED: {}\nFix: {}", t.headline, t.fix),
        Kind::Cleared => format!("Cleared: {}", t.headline),
    }
}

/// Event-bus sink: relayed into Matrix by the Safehouse sink as an
/// `operator_priority.escalation` handoff line (issue number 0 = fleet-wide).
pub struct BusSink(pub Arc<crate::event_bus::EventBus>);

impl AlertSink for BusSink {
    fn name(&self) -> &'static str {
        "event-bus"
    }
    fn deliver(&self, t: &Transition, host: &str) -> Result<(), String> {
        let event = crate::types::Event::OperatorPriorityEscalation {
            slug: format!("fleet/{host}"),
            issue: 0,
            key: t.key.clone(),
            kind: "fleet-degraded".to_string(),
            stage: t.key.clone(),
            text: render_body(t),
            url: String::new(),
            host: host.to_string(),
            inherited_from: None,
            resolved: t.kind == Kind::Cleared,
        };
        // No subscriber (Safehouse off) is not a failure.
        let _ = self.0.publish(event);
        Ok(())
    }
}

/// Per-tick inputs other than the alert state and sinks.
pub struct TickContext<'a> {
    pub status: Option<&'a DaemonStatusReport>,
    pub window: Duration,
    pub host: &'a str,
    pub pool_dir: Option<&'a Path>,
    /// #10916: output-based watchdog data; `None` skips the watchdog.
    pub outputs: Option<&'a super::outputs::OutputWatch<'a>>,
}

/// One evaluation pass. Delivers every transition to every sink (a failing
/// sink never suppresses the others) and returns the transitions.
pub fn run_tick(
    state: &mut AlertState,
    sinks: &[Box<dyn AlertSink>],
    ctx: &TickContext<'_>,
    now: DateTime<Utc>,
) -> Vec<Transition> {
    let TickContext {
        status,
        window,
        host,
        pool_dir,
        outputs,
    } = *ctx;
    // An unreachable status is "unknown", not "healthy": leave state alone.
    let Some(status) = status else {
        return Vec::new();
    };
    let cause = causes::token_cause(status.capacity.total_accounts, pool_dir);
    let mut observed = classify(status, now, window, cause);
    // #10916: output-based watchdog for fleet singletons; absent data fires.
    if let Some(watch) = outputs {
        observed.extend(super::outputs::conditions(watch, now));
    }
    let transitions = state.step(now, &observed);
    for t in &transitions {
        for sink in sinks {
            if let Err(e) = sink.deliver(t, host) {
                log::warn!("fleet_alert: {} sink failed for {}: {e}", sink.name(), t.key);
            }
        }
    }
    transitions
}

fn fetch_status(socket: &Path) -> Option<DaemonStatusReport> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(IPC_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IPC_TIMEOUT)).ok()?;
    let req = serde_json::to_string(&Request::DaemonStatus).ok()?;
    stream.write_all(req.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    stream.flush().ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    match serde_json::from_str::<Response>(&line).ok()? {
        Response::DaemonStatus(report) => Some(*report),
        _ => None,
    }
}

/// Start the alert thread. Returns `None` when disabled.
pub fn spawn(
    workspace_root: PathBuf,
    socket_path: PathBuf,
    bus: Option<Arc<crate::event_bus::EventBus>>,
) -> Option<std::thread::JoinHandle<()>> {
    let settings = Settings::resolve(&workspace_root);
    if !settings.enabled {
        log::debug!(
            "fleet_alert: disabled (set autonomous.fleetAlert.enabled or {})",
            super::ENABLED_ENV
        );
        return None;
    }
    let Some(bus) = bus else {
        log::info!("fleet_alert: no delivery sink available; alerting is a no-op");
        return None;
    };
    let state_path = workspace_root
        .join(".loom")
        .join("logs")
        .join("fleet-alert-state.json");
    let spawned = std::thread::Builder::new()
        .name("fleet-alert".to_string())
        .spawn(move || {
            let sinks: Vec<Box<dyn AlertSink>> = vec![Box::new(BusSink(bus))];
            let mut state =
                AlertState::load(&state_path, settings.debounce_ticks, settings.reminder);
            let host = crate::sweep_registry::host_identity();
            let window = Duration::from_secs(crate::health::DEFAULT_WINDOW_SECS);
            // Let the daemon finish starting before the first evaluation.
            std::thread::sleep(Duration::from_secs(30));
            loop {
                let status = fetch_status(&socket_path);
                let pool_dir = status.as_ref().and_then(|s| s.token_pool_dir.clone());
                let ctx = TickContext {
                    status: status.as_ref(),
                    window,
                    host: &host,
                    pool_dir: pool_dir.as_deref(),
                    // The real fleet-store/SigNoz OutputSource is a follow-up.
                    outputs: None,
                };
                let transitions = run_tick(&mut state, &sinks, &ctx, Utc::now());
                if !transitions.is_empty() {
                    state.save(&state_path);
                }
                std::thread::sleep(settings.interval);
            }
        });
    match spawned {
        Ok(h) => {
            log::info!("fleet_alert: fleet-degraded alerting running (#10164)");
            Some(h)
        }
        Err(e) => {
            log::warn!("fleet_alert: could not start thread: {e}");
            None
        }
    }
}
