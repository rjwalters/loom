//! The incidents one level-triggered source has open on loom-ui, kept across
//! restarts (#11295).
//!
//! A source evaluates its condition, hands [`Ledger::reconcile`] the alerts
//! that hold now, and the ledger posts only the edges: `open` for an incident
//! it does not have yet, `resolved` for one it has that no longer holds. The
//! open set is written to `<loom_dir>/loom-ui-alerts/<scope>.json` on every
//! change, so a daemon restarted while an incident was open still resolves it
//! on its first evaluation that finds the condition gone, and does not post a
//! second `open` for one that still holds.
//!
//! A queued post the process dies before sending is lost; the ledger has
//! already recorded it. An `open` lost that way is posted again by nothing (the
//! next `resolved` is then a no-op on loom-ui); a lost `resolved` leaves the
//! incident to loom-ui. Both need a crash inside the outbox's delivery window.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Alert, Severity, State};

/// Directory under `<loom_dir>` the ledgers live in.
pub const DIR: &str = "loom-ui-alerts";

/// One open incident, as persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    rule: String,
    key: String,
    title: String,
    critical: bool,
}

/// What a source has open, and where its edges go.
pub struct Ledger {
    path: Option<PathBuf>,
    open: BTreeMap<(String, String), Entry>,
    sink: Box<dyn FnMut(Alert) + Send>,
}

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger")
            .field("path", &self.path)
            .field("open", &self.open.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// `<loom_dir>`: the parent of `LOOM_SOCKET_PATH` when set (a test daemon on a
/// tempdir socket never writes into the operator's `~/.loom`), else `~/.loom`.
fn loom_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        return PathBuf::from(path).parent().map(Path::to_path_buf);
    }
    dirs::home_dir().map(|h| h.join(".loom"))
}

/// Where the production ledger for `scope` lives. Never a real file in unit
/// tests: those drive [`Ledger::at`] with their own path.
#[must_use]
pub fn path_for(scope: &str) -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    loom_dir().map(|d| d.join(DIR).join(format!("{scope}.json")))
}

impl Ledger {
    /// The production ledger for `scope`, posting through [`super::post`].
    #[must_use]
    pub fn load(scope: &str) -> Self {
        Self::at(path_for(scope), super::post)
    }

    /// A ledger persisted at `path` (`None`: memory only), posting to `sink`.
    #[must_use]
    pub fn at(path: Option<PathBuf>, sink: impl FnMut(Alert) + Send + 'static) -> Self {
        let open = path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str::<Vec<Entry>>(&raw).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|e| ((e.rule.clone(), e.key.clone()), e))
            .collect();
        Self {
            path,
            open,
            sink: Box::new(sink),
        }
    }

    /// Whether `rule`/`key` is open.
    #[must_use]
    pub fn is_open(&self, rule: &str, key: &str) -> bool {
        self.open.contains_key(&(rule.to_string(), key.to_string()))
    }

    /// The open incidents, as `(rule, key)`.
    #[must_use]
    pub fn open_incidents(&self) -> Vec<(String, String)> {
        self.open.keys().cloned().collect()
    }

    /// Open `alert` (its `state` is ignored) unless it is open already.
    /// Returns whether it was posted.
    pub fn open(&mut self, mut alert: Alert) -> bool {
        let id = (alert.rule.clone(), alert.key.clone());
        if self.open.contains_key(&id) {
            return false;
        }
        self.open.insert(
            id,
            Entry {
                rule: alert.rule.clone(),
                key: alert.key.clone(),
                title: alert.title.clone(),
                critical: alert.severity == Severity::Critical,
            },
        );
        log::info!("loom_ui_alerts: open {} {}", alert.rule, alert.key);
        alert.state = State::Open;
        (self.sink)(alert);
        self.save();
        true
    }

    /// Resolve `rule`/`key` if it is open, with `detail` saying why. Returns
    /// whether it was posted.
    pub fn resolve(&mut self, rule: &str, key: &str, detail: &str) -> bool {
        let Some(entry) = self.open.remove(&(rule.to_string(), key.to_string())) else {
            return false;
        };
        log::info!("loom_ui_alerts: resolved {rule} {key}");
        (self.sink)(resolved(&entry, detail));
        self.save();
        true
    }

    /// Make the open set exactly `current`: open what is new, resolve what is
    /// gone. Returns how many edges were posted.
    pub fn reconcile(&mut self, current: Vec<Alert>) -> usize {
        self.reconcile_where(current, |_, _| true)
    }

    /// [`Self::reconcile`] over the incidents `owns` claims only: an open
    /// incident `owns` refuses is left alone even when absent from `current`
    /// (another evaluation decides it).
    pub fn reconcile_where(
        &mut self,
        current: Vec<Alert>,
        owns: impl Fn(&str, &str) -> bool,
    ) -> usize {
        let wanted: std::collections::BTreeSet<(String, String)> = current
            .iter()
            .map(|a| (a.rule.clone(), a.key.clone()))
            .collect();
        let gone: Vec<(String, String)> = self
            .open
            .keys()
            .filter(|(rule, key)| owns(rule, key) && !wanted.contains(&(rule.clone(), key.clone())))
            .cloned()
            .collect();
        let mut posted = 0;
        for (rule, key) in gone {
            posted += usize::from(self.resolve(&rule, &key, "The condition has cleared."));
        }
        for alert in current {
            posted += usize::from(self.open(alert));
        }
        posted
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if super::is_off() {
            return;
        }
        let entries: Vec<&Entry> = self.open.values().collect();
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut body = serde_json::to_vec_pretty(&entries).map_err(std::io::Error::other)?;
            body.push(b'\n');
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, body)?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            log::warn!("loom_ui_alerts: could not write {}: {e}", path.display());
        }
    }
}

fn resolved(entry: &Entry, detail: &str) -> Alert {
    let severity = if entry.critical {
        Severity::Critical
    } else {
        Severity::Warning
    };
    Alert::new(&entry.rule, &entry.key, &entry.title, severity, State::Resolved).with_detail(detail)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<(String, String, State)>>>;

    fn ledger(path: Option<PathBuf>) -> (Ledger, Seen) {
        let seen: Seen = Arc::default();
        let sink = seen.clone();
        let l = Ledger::at(path, move |a: Alert| {
            sink.lock().unwrap().push((a.rule, a.key, a.state));
        });
        (l, seen)
    }

    fn a(rule: &str, key: &str) -> Alert {
        Alert::new(rule, key, "t", Severity::Warning, State::Open)
    }

    fn take(seen: &Seen) -> Vec<(String, String, State)> {
        std::mem::take(&mut *seen.lock().unwrap())
    }

    fn e(rule: &str, key: &str, state: State) -> (String, String, State) {
        (rule.into(), key.into(), state)
    }

    #[test]
    fn only_edges_are_posted() {
        let (mut l, seen) = ledger(None);
        assert_eq!(l.reconcile(vec![a("r", "x"), a("r", "y")]), 2);
        assert_eq!(l.reconcile(vec![a("r", "x"), a("r", "y")]), 0);
        assert_eq!(l.reconcile(vec![a("r", "y")]), 1);
        assert_eq!(l.reconcile(vec![]), 1);
        assert_eq!(
            take(&seen),
            [
                e("r", "x", State::Open),
                e("r", "y", State::Open),
                e("r", "x", State::Resolved),
                e("r", "y", State::Resolved),
            ]
        );
    }

    #[test]
    fn a_restart_resolves_what_the_previous_process_left_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        {
            let (mut l, _) = ledger(Some(path.clone()));
            l.reconcile(vec![a("r", "gone"), a("r", "still")]);
        }
        let (mut l, seen) = ledger(Some(path.clone()));
        assert!(l.is_open("r", "gone"));
        // Still bad: no second open. Gone: resolved.
        l.reconcile(vec![a("r", "still")]);
        assert_eq!(take(&seen), [e("r", "gone", State::Resolved)]);
        let (l, _) = ledger(Some(path));
        assert_eq!(l.open_incidents(), [("r".to_string(), "still".to_string())]);
    }

    #[test]
    fn reconcile_where_leaves_what_it_does_not_own() {
        let (mut l, seen) = ledger(None);
        l.reconcile(vec![a("a", "1"), a("b", "1")]);
        take(&seen);
        l.reconcile_where(vec![], |rule, _| rule == "a");
        assert_eq!(take(&seen), [e("a", "1", State::Resolved)]);
        assert!(l.is_open("b", "1"));
    }

    #[test]
    fn resolved_keeps_the_severity_and_title() {
        let seen: Arc<Mutex<Vec<Alert>>> = Arc::default();
        let sink = seen.clone();
        let mut l = Ledger::at(None, move |a| sink.lock().unwrap().push(a));
        l.open(Alert::new("r", "k", "Bad thing", Severity::Critical, State::Open));
        l.resolve("r", "k", "fixed");
        let got = seen.lock().unwrap();
        assert_eq!(got[1].state, State::Resolved);
        assert_eq!(got[1].severity, Severity::Critical);
        assert_eq!(got[1].title, "Bad thing");
        assert_eq!(got[1].detail.as_deref(), Some("fixed"));
    }

    #[test]
    fn an_unreadable_file_is_an_empty_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(&path, "not json").unwrap();
        let (l, _) = ledger(Some(path));
        assert!(l.open_incidents().is_empty());
    }

    #[test]
    fn production_paths_are_never_real_files_in_tests() {
        assert_eq!(path_for("x"), None);
    }
}
