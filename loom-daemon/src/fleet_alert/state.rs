//! De-duplication state machine (#10164). Pure given an injected clock.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::Condition;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Started,
    Reminder,
    Cleared,
}

/// One alert to deliver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub kind: Kind,
    pub key: String,
    pub headline: String,
    pub fix: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Persisted {
    last_alert: Option<DateTime<Utc>>,
    headline: String,
}

#[derive(Debug, Default)]
struct Counters {
    bad: u32,
    good: u32,
}

/// Per-condition alert state. `active` entries are the persisted ones.
#[derive(Debug, Default)]
pub struct AlertState {
    active: BTreeMap<String, Persisted>,
    counters: BTreeMap<String, Counters>,
    debounce: u32,
    reminder: Duration,
}

impl AlertState {
    #[must_use]
    pub fn new(debounce_ticks: u32, reminder: Duration) -> Self {
        Self {
            debounce: debounce_ticks.max(1),
            reminder,
            ..Self::default()
        }
    }

    /// Load persisted active alerts so a restart does not re-announce them.
    /// A missing or corrupt file is an empty state.
    #[must_use]
    pub fn load(path: &Path, debounce_ticks: u32, reminder: Duration) -> Self {
        let mut s = Self::new(debounce_ticks, reminder);
        if let Some(map) = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<BTreeMap<String, Persisted>>(&t).ok())
        {
            s.active = map;
        }
        s
    }

    /// Best-effort persist.
    pub fn save(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(&self.active) {
            if let Err(e) = std::fs::write(path, text) {
                log::debug!("fleet_alert: could not persist state: {e}");
            }
        }
    }

    #[must_use]
    pub fn is_active(&self, key: &str) -> bool {
        self.active.contains_key(key)
    }

    /// Advance one tick over the currently observed conditions.
    pub fn step(&mut self, now: DateTime<Utc>, observed: &[Condition]) -> Vec<Transition> {
        let mut out = Vec::new();
        for c in observed {
            let ctr = self.counters.entry(c.key.clone()).or_default();
            ctr.good = 0;
            ctr.bad = ctr.bad.saturating_add(1);
            let bad = ctr.bad;
            match self.active.get_mut(&c.key) {
                None if bad >= self.debounce => {
                    self.active.insert(
                        c.key.clone(),
                        Persisted {
                            last_alert: Some(now),
                            headline: c.headline.clone(),
                        },
                    );
                    out.push(transition(Kind::Started, c));
                }
                Some(p) => {
                    p.headline.clone_from(&c.headline);
                    let due = p.last_alert.is_none_or(|t| {
                        now.signed_duration_since(t)
                            .to_std()
                            .is_ok_and(|d| d >= self.reminder)
                    });
                    if due {
                        p.last_alert = Some(now);
                        out.push(transition(Kind::Reminder, c));
                    }
                }
                None => {}
            }
        }
        let seen: Vec<&str> = observed.iter().map(|c| c.key.as_str()).collect();
        // A persisted-active key not yet in `counters` (fresh after restart)
        // that is no longer observed still needs to clear.
        let orphans: Vec<String> = self
            .active
            .keys()
            .filter(|k| !seen.contains(&k.as_str()) && !self.counters.contains_key(*k))
            .cloned()
            .collect();
        for key in orphans {
            self.counters.insert(key, Counters::default());
        }
        let keys: Vec<String> = self.counters.keys().cloned().collect();
        for key in keys {
            if seen.contains(&key.as_str()) {
                continue;
            }
            let ctr = self.counters.entry(key.clone()).or_default();
            ctr.bad = 0;
            ctr.good = ctr.good.saturating_add(1);
            if ctr.good >= self.debounce {
                if let Some(p) = self.active.remove(&key) {
                    out.push(Transition {
                        kind: Kind::Cleared,
                        key: key.clone(),
                        headline: p.headline,
                        fix: String::new(),
                    });
                }
                self.counters.remove(&key);
            }
        }
        out
    }
}

fn transition(kind: Kind, c: &Condition) -> Transition {
    Transition {
        kind,
        key: c.key.clone(),
        headline: c.headline.clone(),
        fix: c.fix.clone(),
    }
}
