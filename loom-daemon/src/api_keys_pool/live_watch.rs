//! In-run exhaustion detection for a **live** launch (issue #11286 item 3).
//!
//! # The gap this closes
//!
//! [`super::ingest`] marks a seat only after its run exits, so for the whole
//! remaining lifetime of a run that hit its cap at minute 1, every concurrent
//! spawn could still be handed the same empty seat. #8424 rejected the
//! in-process fix (a supervisor between Loom and the `exec`ed harness) for
//! good reasons that still hold, and this module does not revisit them.
//!
//! Instead it applies the *same* post-hoc reading incrementally: the daemon's
//! reaper already visits every live sweep on every tick, and the sweep's
//! retained log is already where the harness's stderr and error events land.
//! [`LiveWatch`] tails that log by byte offset and, the moment the region's
//! provider-written lines classify as an exhaustion, hands back what
//! [`super::ingest::apply_mark`] needs. No process is added to the exec chain;
//! the only cost is reading what was appended since the previous tick.
//!
//! # Narrower than the exit-time path, on purpose
//!
//! The exit-time ingest has guard 3 ("an exit-0 run is never marked"); a live
//! run has no exit code yet. To keep that guard's intent, the live path acts
//! on **`Exhausted` only**:
//!
//! * A rate limit is transient and OpenCode/Kimi retry it in-process (up to 10
//!   attempts) — a live 429 banner is routinely followed by success, so marking
//!   on it mid-run would mark seats that were never empty. It still marks at
//!   exit exactly as before.
//! * An exhausted allowance is not retried into success: once the provider
//!   says the plan is used up, nothing the run does before its reset can
//!   succeed against that seat.
//!
//! Every other guard is shared with the exit path rather than re-implemented:
//! the region is anchored, the `# LOOM_LAUNCH` record must say
//! `credentialSource: "pool"` (guards 1/2), a credential failure anywhere in
//! the region suppresses the mark (guard 4), and only provider-written lines
//! are classified — never the agent's transcript (guard 5).
//!
//! A run is marked at most once per launch; the exit-time ingest then finds
//! the mark already covering it ([`super::bad_marks::escalate_bad_for_class`])
//! and writes nothing.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::classify::{classify, provider_lines, Classification};
use super::ingest::{parse_launch_record, LaunchRecord, LAUNCH_RECORD_PREFIX};

/// Most bytes read from the log per poll. A freshly-watched long log (a
/// daemon restart mid-sweep) catches up over a few ticks instead of one big
/// read under the registry lock.
pub const MAX_READ_PER_POLL: u64 = 1024 * 1024;

/// Longest unterminated trailing line kept between polls. Anything longer is
/// a transcript event (provider error lines are short), so it is dropped.
const MAX_PARTIAL_LINE: usize = 64 * 1024;

/// Provider-written lines kept for the reset-horizon parse.
const MAX_PROVIDER_LINES: usize = 64;

/// Longest single provider line kept for the reset-horizon parse.
const MAX_PROVIDER_LINE_BYTES: usize = 4096;

/// What a poll decided: mark this record's account for `classification`,
/// with `provider_text` as the reset-horizon evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveExhaustion {
    pub record: LaunchRecord,
    pub classification: Classification,
    pub provider_text: String,
}

/// Incremental reader of one launch's retained log. See the module docs.
#[derive(Debug, Default)]
pub struct LiveWatch {
    anchor: String,
    offset: u64,
    partial: String,
    in_region: bool,
    record: Option<LaunchRecord>,
    credential_failure: bool,
    exhausted: bool,
    provider_tail: VecDeque<String>,
    decided: bool,
}

impl LiveWatch {
    /// Watch the region that starts at the last line containing `anchor`
    /// (`sweep_id=<id>`). An empty anchor means the whole log.
    #[must_use]
    pub fn new(anchor: impl Into<String>) -> Self {
        let anchor = anchor.into();
        Self {
            in_region: anchor.is_empty(),
            anchor,
            ..Self::default()
        }
    }

    /// Whether this watch has already handed back a decision for the current
    /// launch (it never hands back a second one for the same launch).
    #[must_use]
    pub fn decided(&self) -> bool {
        self.decided
    }

    /// Read what was appended to `log` since the previous poll and return a
    /// decision the first time the current launch reads as exhausted. An
    /// unreadable log is "no opinion" and is retried next poll.
    pub fn poll(&mut self, log: &Path) -> Option<LiveExhaustion> {
        let mut file = std::fs::File::open(log).ok()?;
        let len = file.metadata().ok()?.len();
        if len < self.offset {
            // Truncated or replaced: start over, as a fresh watch would.
            *self = Self::new(std::mem::take(&mut self.anchor));
        }
        file.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut bytes = Vec::new();
        file.take(MAX_READ_PER_POLL).read_to_end(&mut bytes).ok()?;
        self.offset += bytes.len() as u64;
        self.feed(&String::from_utf8_lossy(&bytes))
    }

    /// Feed newly appended text (pure half of [`Self::poll`]).
    pub fn feed(&mut self, appended: &str) -> Option<LiveExhaustion> {
        let mut text = std::mem::take(&mut self.partial);
        text.push_str(appended);
        let complete = match text.rfind('\n') {
            Some(end) => {
                let rest = text.split_off(end + 1);
                if rest.len() <= MAX_PARTIAL_LINE {
                    self.partial = rest;
                }
                text
            }
            None => {
                if text.len() <= MAX_PARTIAL_LINE {
                    self.partial = text;
                }
                return self.decision();
            }
        };
        for line in complete.lines() {
            self.feed_line(line);
        }
        self.decision()
    }

    fn feed_line(&mut self, line: &str) {
        if !self.anchor.is_empty() && line.contains(&self.anchor) {
            // A later occurrence of the anchor starts a new region: nothing
            // before it may be attributed to this run.
            self.in_region = true;
            self.reset_launch();
            return;
        }
        if !self.in_region {
            return;
        }
        if line.trim_start().starts_with(LAUNCH_RECORD_PREFIX) {
            if let Some(record) = parse_launch_record(line) {
                // A new launch inside the region (a re-dispatch, a
                // containment re-exec): last record wins, flags start over.
                self.reset_launch();
                self.record = Some(record);
            }
            return;
        }
        let provider = provider_lines(line);
        if provider.trim().is_empty() {
            return;
        }
        match classify(&provider, 1) {
            Some(Classification::CredentialFailure) => self.credential_failure = true,
            Some(Classification::Exhausted) => self.exhausted = true,
            _ => {}
        }
        let mut kept = provider;
        if kept.len() > MAX_PROVIDER_LINE_BYTES {
            let mut cut = MAX_PROVIDER_LINE_BYTES;
            while !kept.is_char_boundary(cut) {
                cut -= 1;
            }
            kept.truncate(cut);
        }
        self.provider_tail.push_back(kept);
        while self.provider_tail.len() > MAX_PROVIDER_LINES {
            self.provider_tail.pop_front();
        }
    }

    fn reset_launch(&mut self) {
        self.record = None;
        self.credential_failure = false;
        self.exhausted = false;
        self.provider_tail.clear();
        self.decided = false;
    }

    fn decision(&mut self) -> Option<LiveExhaustion> {
        if self.decided || !self.exhausted || self.credential_failure {
            return None;
        }
        let record = self.record.as_ref()?;
        if !record.is_pool_selected() || record.provider.is_none() || record.account.is_none() {
            return None;
        }
        self.decided = true;
        Some(LiveExhaustion {
            record: record.clone(),
            classification: Classification::Exhausted,
            provider_text: self
                .provider_tail
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
        })
    }
}

#[cfg(test)]
#[path = "live_watch_tests.rs"]
mod tests;
