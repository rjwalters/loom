//! The bounded FIFO the drain task works through (#11295). Pure, with an
//! injected clock.
//!
//! Strict FIFO with head-of-line retry: while the oldest alert waits out its
//! backoff nothing behind it is sent, so an incident's `open` can never be
//! overtaken by its own `resolved`. Every alert goes to the same loom-ui, so
//! when the head cannot get through nothing behind it could either.
//!
//! Bounded twice: at most `capacity` queued alerts (a new one past that drops
//! the oldest *queued* one, never the one in flight), and at most
//! [`MAX_ATTEMPTS`] tries per alert.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::{Alert, Delivery};

/// Default queue bound.
pub const DEFAULT_CAPACITY: usize = 256;
/// Tries per alert before it is dropped (~3.5 hours of backoff).
pub const MAX_ATTEMPTS: u32 = 20;
/// First retry delay.
pub const BACKOFF_BASE: Duration = Duration::from_secs(5);
/// Longest retry delay.
pub const BACKOFF_CAP: Duration = Duration::from_secs(15 * 60);

/// One queued alert.
#[derive(Debug, Clone)]
pub struct Entry {
    pub alert: Alert,
    /// Failed tries so far.
    pub attempts: u32,
    not_before: Instant,
}

/// What the drain task should do now.
#[derive(Debug)]
pub enum Next {
    /// Send this (now owned by the caller; hand it back to [`Outbox::settle`]).
    Ready(Entry),
    /// The head is backing off for this long.
    Wait(Duration),
    Empty,
}

/// Delay before try `attempts + 1`, after `attempts` failures.
#[must_use]
pub fn backoff(attempts: u32) -> Duration {
    let shift = attempts.saturating_sub(1).min(16);
    BACKOFF_BASE.saturating_mul(1 << shift).min(BACKOFF_CAP)
}

#[derive(Debug)]
pub struct Outbox {
    queue: VecDeque<Entry>,
    capacity: usize,
}

impl Outbox {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Append `alert`, due now. Returns the alert dropped to make room.
    pub fn push(&mut self, alert: Alert, now: Instant) -> Option<Alert> {
        let lost = (self.queue.len() >= self.capacity)
            .then(|| self.queue.pop_front())
            .flatten()
            .map(|e| e.alert);
        self.queue.push_back(Entry {
            alert,
            attempts: 0,
            not_before: now,
        });
        lost
    }

    /// Take the head if it is due.
    pub fn take_ready(&mut self, now: Instant) -> Next {
        match self.queue.front() {
            None => Next::Empty,
            Some(head) if head.not_before > now => Next::Wait(head.not_before - now),
            Some(_) => self.queue.pop_front().map_or(Next::Empty, Next::Ready),
        }
    }

    /// Record how sending `entry` went. A retry goes back to the head with
    /// its backoff; past [`MAX_ATTEMPTS`] it is returned as given up.
    pub fn settle(&mut self, mut entry: Entry, delivery: &Delivery, now: Instant) -> Option<Entry> {
        match delivery {
            Delivery::Delivered | Delivery::Dropped(_) => None,
            Delivery::Retry(_) => {
                entry.attempts += 1;
                if entry.attempts >= MAX_ATTEMPTS {
                    return Some(entry);
                }
                entry.not_before = now + backoff(entry.attempts);
                self.queue.push_front(entry);
                None
            }
        }
    }
}
