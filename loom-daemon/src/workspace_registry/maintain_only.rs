//! The registry's per-workspace dispatch mode: maintain-only (#11186).
//!
//! A maintain-only workspace stays registered, so every maintenance step that
//! walks the registry still covers it: the per-repo Loom resync, the checkout
//! fast-forward and the floor checks. Dispatch reads the same flag through
//! [`crate::workspace_hold::hold_for`], which reports it as the
//! `maintain-only` hold on every path that starts work.
//!
//! The flag is set two ways, and the entry records which:
//!
//! - the fleet store's roster, `fleet: maintain` ([`crate::fleet_store::roster`]),
//!   applied like any other roster change;
//! - `loom-daemon workspace hold <root>` / `release <root>`, by hand, for a
//!   host no fleet store drives.
//!
//! On a host a fleet store drives the roster has the last word, exactly as it
//! does for `priority`: a hand-set flag on a repo the store lists as
//! `fleet: true` is cleared by the next roster apply.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{normalize_path, WorkspaceRegistry};

/// Who made a workspace maintain-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaintainOnlySource {
    /// The fleet store's roster: `fleet: maintain`.
    FleetStore,
    /// `loom-daemon workspace hold <root>`.
    Operator,
}

impl MaintainOnlySource {
    /// `fleet store` / `operator`, for a person.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::FleetStore => "fleet store",
            Self::Operator => "operator",
        }
    }
}

/// A workspace's maintain-only mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintainOnly {
    /// Who set it.
    pub by: MaintainOnlySource,
    /// When it was set.
    pub since: DateTime<Utc>,
}

impl MaintainOnly {
    /// `maintain-only (fleet store)`: what `status` and the hold logs print.
    #[must_use]
    pub fn label(&self) -> String {
        format!("maintain-only ({})", self.by.label())
    }
}

impl WorkspaceRegistry {
    /// The maintain-only mark of the workspace at `root` (normalized here),
    /// or `None` when it is a normal workspace or not registered.
    #[must_use]
    pub fn maintain_only_of(&self, root: &Path) -> Option<MaintainOnly> {
        let canonical = normalize_path(root);
        self.workspaces
            .iter()
            .find(|w| w.root == root || w.root == canonical)
            .and_then(|w| w.maintain_only)
    }

    /// Make the workspace at `root` maintain-only (`Some(by)`) or a normal,
    /// dispatched workspace again (`None`), in place: it is never removed and
    /// re-added. `None` when `root` is not registered; otherwise whether the
    /// entry changed. Marking an already maintain-only workspace again keeps
    /// its original mark (who and since).
    pub fn set_maintain_only(
        &mut self,
        root: &Path,
        by: Option<MaintainOnlySource>,
        now: DateTime<Utc>,
    ) -> Option<bool> {
        let canonical = normalize_path(root);
        let ws = self.workspaces.iter_mut().find(|w| w.root == canonical)?;
        Some(match (by, ws.maintain_only) {
            (Some(by), None) => {
                ws.maintain_only = Some(MaintainOnly { by, since: now });
                true
            }
            (None, Some(_)) => {
                ws.maintain_only = None;
                true
            }
            _ => false,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn the_flag_flips_in_place_and_round_trips() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let path = dir.path().join("workspaces.json");
        let mut reg = WorkspaceRegistry::default();
        reg.add_with_priority(&repo, None, 7).unwrap();

        let by = Some(MaintainOnlySource::Operator);
        assert_eq!(reg.set_maintain_only(&repo, by, t0()), Some(true));
        // Again: no change, and the first mark stands.
        let later = t0() + chrono::Duration::hours(1);
        let fleet = Some(MaintainOnlySource::FleetStore);
        assert_eq!(reg.set_maintain_only(&repo, fleet, later), Some(false));
        let mark = reg.maintain_only_of(&repo).unwrap();
        assert_eq!((mark.by, mark.since), (MaintainOnlySource::Operator, t0()));
        assert_eq!(mark.label(), "maintain-only (operator)");

        reg.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"maintain_only\""), "{text}");
        assert!(text.contains("\"operator\""), "{text}");
        let loaded = WorkspaceRegistry::load(&path).unwrap();
        assert_eq!(loaded, reg);

        // Back to normal: same entry, same priority, never removed.
        assert_eq!(reg.set_maintain_only(&repo, None, later), Some(true));
        assert_eq!(reg.workspaces.len(), 1);
        assert_eq!(reg.workspaces[0].priority, 7);
        assert!(reg.maintain_only_of(&repo).is_none());
        reg.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("maintain_only"), "a normal entry writes no key: {text}");
    }

    #[test]
    fn an_unregistered_root_is_none() {
        let mut reg = WorkspaceRegistry::default();
        let by = Some(MaintainOnlySource::Operator);
        assert_eq!(reg.set_maintain_only(Path::new("/nope"), by, t0()), None);
        assert!(reg.maintain_only_of(Path::new("/nope")).is_none());
    }

    /// A registry written before #11186 has no key: every entry is normal.
    #[test]
    fn an_older_registry_reads_as_normal() {
        let reg: WorkspaceRegistry =
            serde_json::from_str(r#"{"version":1,"workspaces":[{"root":"/a","priority":3}]}"#)
                .unwrap();
        assert!(reg.workspaces[0].maintain_only.is_none());
    }
}
