//! Durable record of *applied* no-op holds (Issue #10156 review follow-up).
//!
//! Reconciliation walks the in-memory streak table, which a daemon restart
//! empties — leaving a parked issue that nothing would ever release. The
//! applied holds (issue, park kind, announced fingerprint) are therefore
//! mirrored to `<logs_dir>/noop-holds.json` and reloaded once per process,
//! before the first count or reconciliation. Best-effort throughout: an
//! unwritable file degrades to the pre-persistence behaviour.

use super::*;
use std::collections::BTreeMap;

const HOLDS_FILE: &str = "noop-holds.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedHold {
    park: String,
    fingerprint: String,
}

impl SweepRegistry {
    fn noop_holds_path(&self) -> PathBuf {
        self.config.logs_dir().join(HOLDS_FILE)
    }

    /// Reload persisted holds into the streak table (once; never overwrites a
    /// streak the process already tracks).
    pub(super) fn restore_noop_holds(&mut self) {
        if self.noop_cooldown.restored || self.config.skip_label_flip {
            return;
        }
        self.noop_cooldown.restored = true;
        let Ok(raw) = std::fs::read_to_string(self.noop_holds_path()) else {
            return;
        };
        let Ok(map) = serde_json::from_str::<BTreeMap<u32, PersistedHold>>(&raw) else {
            log::warn!("sweep_registry: unreadable {HOLDS_FILE}; ignoring persisted no-op holds");
            return;
        };
        for (issue, h) in map {
            let Some(park) = ParkKind::from_tag(&h.park) else {
                continue;
            };
            self.noop_cooldown
                .streaks
                .entry(issue)
                .or_insert(NoopStreak {
                    count: self.noop_cooldown_config.hold_threshold,
                    fingerprint: h.fingerprint,
                    last_sweep: None,
                    park,
                    held: true,
                    failure_notice_posted: false,
                    checked_at: None,
                });
        }
        self.noop_cooldown.persisted = raw;
    }

    /// Mirror the currently applied holds to disk (skipped when unchanged).
    pub(super) fn persist_noop_holds(&mut self) {
        if self.config.skip_label_flip {
            return;
        }
        let map: BTreeMap<u32, PersistedHold> = self
            .noop_cooldown
            .streaks
            .iter()
            .filter(|(_, s)| s.held)
            .map(|(i, s)| {
                (
                    *i,
                    PersistedHold {
                        park: s.park.tag().to_string(),
                        fingerprint: s.fingerprint.clone(),
                    },
                )
            })
            .collect();
        let Ok(json) = serde_json::to_string(&map) else {
            return;
        };
        if json == self.noop_cooldown.persisted {
            return;
        }
        let path = self.noop_holds_path();
        let tmp = path.with_extension("json.tmp");
        let written = std::fs::create_dir_all(self.config.logs_dir())
            .and_then(|()| std::fs::write(&tmp, &json))
            .and_then(|()| std::fs::rename(&tmp, &path));
        match written {
            Ok(()) => self.noop_cooldown.persisted = json,
            Err(e) => log::debug!("sweep_registry: could not persist no-op holds: {e}"),
        }
    }
}
