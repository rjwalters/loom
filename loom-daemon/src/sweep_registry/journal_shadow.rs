//! E2 claim-lock `owner.json` shadow appends to J1 (Epic #9908, Phase 2b,
//! #11346). Each helper runs only after the corresponding legacy write (or
//! lock-directory removal) succeeded, and never changes its result — see
//! [`crate::journal::execution`].
//!
//! `claim.acquired` journals the whole provisional owner; every later
//! read-modify-write (`claim.child_recorded`) journals an RFC 7386 patch, so
//! the projection is rebuilt from events the way Phase 3 will read it, and
//! the post-append comparison checks that rebuild against the full owner
//! just written.

use serde_json::{Map, Value};

use crate::journal::execution::{self as j1, Change, Kind, ShadowRecord, Store, Subject};

use super::locks::LockOwner;
use super::SweepRegistry;

/// Which claim-lock namespace (`.loom/locks/<ns>-<N>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockNs {
    Issue,
    Pr,
}

impl LockNs {
    fn as_str(self) -> &'static str {
        match self {
            LockNs::Issue => "issue",
            LockNs::Pr => "pr",
        }
    }
}

impl SweepRegistry {
    fn owner_record(
        &self,
        kind: Kind,
        ns: LockNs,
        number: u32,
        sweep_id: Option<&str>,
        change: Change,
        mut fields: Map<String, Value>,
    ) -> ShadowRecord {
        let lock_dir = self
            .config
            .locks_dir()
            .join(format!("{}-{number}", ns.as_str()));
        fields.insert("lock".into(), Value::from(ns.as_str()));
        ShadowRecord {
            kind,
            store: Store::Owner,
            path: lock_dir.join("owner.json"),
            item: "owner".to_owned(),
            change,
            subject: Subject {
                workspace: self.config.workspace_root.display().to_string(),
                issue: (ns == LockNs::Issue).then_some(number),
                pr: (ns == LockNs::Pr).then_some(number),
                sweep_id: sweep_id.map(str::to_owned),
            },
            fields,
            key: None,
        }
    }

    /// `owner.json` was created (or taken over in place) with `owner`.
    pub(crate) fn shadow_owner_acquired(
        &self,
        ns: LockNs,
        number: u32,
        owner: &LockOwner,
        provisional: bool,
    ) {
        if j1::current_settings().is_none() {
            return;
        }
        let Ok(value) = serde_json::to_value(owner) else {
            return;
        };
        let mut fields = Map::new();
        fields.insert("owner_pid".into(), Value::from(owner.owner_pid));
        fields.insert("provisional".into(), Value::from(provisional));
        j1::shadow(self.owner_record(
            Kind::ClaimAcquired,
            ns,
            number,
            Some(owner.sweep_id.as_str()),
            Change::Set(value),
            fields,
        ));
    }

    /// `owner.json` was rewritten from `before` to `after` (child pid, pgid,
    /// model/effort, overflow, resume handle).
    pub(crate) fn shadow_owner_updated(
        &self,
        ns: LockNs,
        number: u32,
        before: &Value,
        after: &LockOwner,
    ) {
        if j1::current_settings().is_none() {
            return;
        }
        let Ok(after_value) = serde_json::to_value(after) else {
            return;
        };
        let patch = j1::diff_patch(before, &after_value);
        let mut fields = Map::new();
        fields.insert("child_pid".into(), Value::from(after.owner_pid));
        fields.insert(
            "pid_start_time".into(),
            super::reaper::pid_identity::pid_start_wallclock(after.owner_pid)
                .map_or(Value::Null, |t| Value::from(t.to_rfc3339())),
        );
        for name in ["pgid", "model", "effort", "scope_unit", "overflow"] {
            if let Some(v) = after_value.get(name) {
                fields.insert(name.into(), v.clone());
            }
        }
        j1::shadow(self.owner_record(
            Kind::ClaimChildRecorded,
            ns,
            number,
            Some(after.sweep_id.as_str()),
            Change::Patch {
                patch,
                expected: after_value,
            },
            fields,
        ));
    }

    /// The lock directory (and with it `owner.json`) was removed.
    pub(crate) fn shadow_owner_released(&self, ns: LockNs, number: u32, sweep_id: Option<&str>) {
        let mut fields = Map::new();
        fields.insert("outcome".into(), Value::from("released"));
        j1::shadow(self.owner_record(
            Kind::ClaimReleased,
            ns,
            number,
            sweep_id,
            Change::Remove,
            fields,
        ));
    }
}
