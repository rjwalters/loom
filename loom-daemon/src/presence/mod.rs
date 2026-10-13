//! Operator-session facts (#11563): the `operator.presence` heartbeat and
//! in-session `sweep.started` / `sweep.outcome` (`dispatch_origin=operator`),
//! written to a durable local journal until the transport of #11525 exists.
//!
//! - [`beat`]: producing the facts and the per-session throttle state.
//! - [`journal`]: the append-only journal and its export.
//! - [`flush`]: the daemon-less one-shot OTLP drain of that journal (#11713).
//!
//! The `loom-daemon presence` subcommand (`cli/presence.rs`) is the thin
//! entry point a Claude Code hook calls.

pub mod beat;
pub mod flush;
pub mod journal;

#[cfg(test)]
mod tests;
