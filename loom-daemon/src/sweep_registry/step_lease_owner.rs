//! Who holds a step's claim, read from its role-keyed lease (Issue #11898).
//!
//! When a step dies, the reaper restores its claim unless a newer claimant
//! holds the item. For a non-Builder step the label is no evidence of who
//! that is: the daemon pre-takes only the role-keyed lease
//! (`loom-daemon lease pr claim --role <role>`), and the role prompt applies
//! the label itself, at a time of its own. So the question is asked of the
//! lease: is the role's earliest live lease on the item still this step's
//! (its sweep id), or someone else's?
//!
//! This is a lib-side reader of the `loom:pr-lease` family the bin crate's
//! `cli/pr_lease.rs` writes. It reads the first three marker tokens only,
//! like every reader of that family, and applies the same liveness rule: a
//! lease is live while its `updated_at` is inside the lease TTL and no later
//! yield record names the same identity. The bin crate's tests pin this
//! parser to `pr_lease::parse_record`.
//!
//! Fail-open, like every release-path probe: an unreadable list, or no live
//! lease of the role, is "not held elsewhere", so the claim is restored.

use super::*;
use crate::gh_invocation::AccessIntent;
use crate::step_role::StepRole;

/// The role-keyed lease family's first-line prefixes (`cli/pr_lease.rs`).
pub const ROLE_LEASE_PREFIX: &str = "<!-- loom:pr-lease role=";
const ROLE_YIELD_PREFIX: &str = "<!-- loom:pr-lease-yield role=";
const ROLE_RELEASED_PREFIX: &str = "<!-- loom:pr-lease-released role=";

/// What one role-lease comment's first line records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleLeaseKind {
    Lease,
    Yield,
    Released,
}

/// One parsed role-lease record: kind, role token, host, sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleLeaseRecord {
    pub kind: RoleLeaseKind,
    pub role: String,
    pub host: String,
    pub sweep: String,
}

/// Parse a role-lease comment body's first line. Pure.
#[must_use]
pub fn parse_role_lease(body: &str) -> Option<RoleLeaseRecord> {
    let first = body.lines().next()?;
    // The yield/released prefixes extend the lease prefix's `loom:pr-lease`,
    // so they are tried first.
    let (kind, rest) = if let Some(rest) = first.strip_prefix(ROLE_YIELD_PREFIX) {
        (RoleLeaseKind::Yield, rest)
    } else if let Some(rest) = first.strip_prefix(ROLE_RELEASED_PREFIX) {
        (RoleLeaseKind::Released, rest)
    } else {
        (RoleLeaseKind::Lease, first.strip_prefix(ROLE_LEASE_PREFIX)?)
    };
    let rest = rest.trim_end().strip_suffix("-->")?;
    let mut tokens = rest.split_whitespace();
    let role = tokens.next()?.to_string();
    let host = tokens.next()?.strip_prefix("host=")?.to_string();
    let sweep = tokens.next()?.strip_prefix("sweep=")?.to_string();
    if role.is_empty() || host.is_empty() || sweep.is_empty() {
        return None;
    }
    Some(RoleLeaseRecord {
        kind,
        role,
        host,
        sweep,
    })
}

/// One role-lease comment as read off the forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleLeaseRow {
    pub id: u64,
    pub updated_at: DateTime<Utc>,
    pub body: String,
}

/// Parse the read's NDJSON (`{id, updated_at, body}` per line). Malformed
/// lines drop.
#[must_use]
pub fn parse_role_lease_rows(ndjson: &[u8]) -> Vec<RoleLeaseRow> {
    String::from_utf8_lossy(ndjson)
        .lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            Some(RoleLeaseRow {
                id: v.get("id")?.as_u64()?,
                updated_at: DateTime::parse_from_rfc3339(v.get("updated_at")?.as_str()?)
                    .ok()?
                    .with_timezone(&Utc),
                body: v.get("body")?.as_str()?.to_string(),
            })
        })
        .collect()
}

/// The sweep id holding `role`'s earliest live lease among `rows` at `now`,
/// `None` when no lease of the role is live. Pure; the same rule as
/// `pr_lease::live_leases`.
#[must_use]
pub fn earliest_live_holder(
    rows: &[RoleLeaseRow],
    role: &str,
    now: DateTime<Utc>,
    ttl: chrono::Duration,
) -> Option<String> {
    let parsed: Vec<(&RoleLeaseRow, RoleLeaseRecord)> = rows
        .iter()
        .filter_map(|r| parse_role_lease(&r.body).map(|p| (r, p)))
        .filter(|(_, p)| p.role == role)
        .collect();
    parsed
        .iter()
        .filter(|(r, p)| p.kind == RoleLeaseKind::Lease && now - r.updated_at < ttl)
        .filter(|(r, p)| {
            !parsed.iter().any(|(y, q)| {
                q.kind == RoleLeaseKind::Yield
                    && y.id > r.id
                    && q.host == p.host
                    && q.sweep == p.sweep
            })
        })
        .min_by_key(|(r, _)| r.id)
        .map(|(_, p)| p.sweep.clone())
}

impl SweepRegistry {
    /// Every trusted role-lease comment on `item`; `None` when unreadable.
    pub(crate) fn read_role_lease_rows(&self, item: u32) -> Option<Vec<RoleLeaseRow>> {
        if self.config.skip_label_flip {
            return None;
        }
        let (owner, repo) = self.resolve_owner_repo()?;
        let gh = self.resolved_gh();
        let mut cmd = self.gh_inv("guard.role_lease_comments", AccessIntent::Read, &gh);
        cmd = cmd
            .arg("api")
            .arg(format!("repos/{owner}/{repo}/issues/{item}/comments"))
            .arg("--paginate")
            .arg("--jq")
            .arg(format!(
                r#".[] | select(.body | startswith("<!-- loom:pr-lease")) | {{id: .id, updated_at: .updated_at, body: .body, {author}}}"#,
                author = crate::comment_trust::records::AUTHOR_JQ,
            ));
        let output = self.run_counted(cmd).ok().flatten()?;
        if !output.status.success() {
            return None;
        }
        // #9548: an untrusted author's lease is prose; it holds nothing.
        let policy = crate::comment_trust::TrustPolicy::for_root(&self.config.workspace_root);
        Some(parse_role_lease_rows(&policy.trusted_ndjson(&output.stdout)))
    }

    /// Whether `role`'s earliest live lease on `item` belongs to another
    /// sweep than `sweep_id` (#11898). Fail-open: unreadable, or no live
    /// lease of the role, is `false`.
    pub(crate) fn step_lease_held_elsewhere(
        &self,
        item: u32,
        role: StepRole,
        sweep_id: &str,
    ) -> bool {
        let Some(rows) = self.read_role_lease_rows(item) else {
            return false;
        };
        let ttl_minutes = crate::claim_reconciliation::resolve_lease_ttl_minutes();
        #[allow(clippy::cast_possible_truncation)]
        let ttl = chrono::Duration::seconds((ttl_minutes * 60.0) as i64);
        earliest_live_holder(&rows, role.role_name(), Utc::now(), ttl)
            .is_some_and(|holder| holder != sweep_id)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn row(id: u64, age_min: i64, body: &str) -> RoleLeaseRow {
        RoleLeaseRow {
            id,
            updated_at: Utc::now() - chrono::Duration::minutes(age_min),
            body: body.to_string(),
        }
    }

    fn ttl() -> chrono::Duration {
        chrono::Duration::minutes(15)
    }

    #[test]
    fn parses_each_record_kind_and_ignores_extra_tokens() {
        let lease =
            parse_role_lease("<!-- loom:pr-lease role=curator host=h sweep=s step=curator -->\nx")
                .unwrap();
        assert_eq!(
            (lease.kind, lease.role.as_str(), lease.sweep.as_str()),
            (RoleLeaseKind::Lease, "curator", "s")
        );
        let y = parse_role_lease(
            "<!-- loom:pr-lease-yield role=judge host=h sweep=s earliest_host=a earliest_sweep=b -->",
        )
        .unwrap();
        assert_eq!(y.kind, RoleLeaseKind::Yield);
        let r =
            parse_role_lease("<!-- loom:pr-lease-released role=judge host=h sweep=s -->").unwrap();
        assert_eq!(r.kind, RoleLeaseKind::Released);
        assert!(parse_role_lease("<!-- loom:lease host=h sweep=s -->").is_none());
        assert!(parse_role_lease("<!-- loom:pr-lease role=judge host= sweep=s -->").is_none());
    }

    #[test]
    fn the_earliest_live_lease_of_the_role_holds() {
        let rows = [
            row(1, 30, "<!-- loom:pr-lease role=curator host=a sweep=old -->"),
            row(2, 1, "<!-- loom:pr-lease role=judge host=a sweep=judge -->"),
            row(3, 1, "<!-- loom:pr-lease role=curator host=b sweep=mine -->"),
            row(4, 1, "<!-- loom:pr-lease role=curator host=c sweep=later -->"),
        ];
        // `old` aged out; `judge` is another role.
        assert_eq!(
            earliest_live_holder(&rows, "curator", Utc::now(), ttl()).as_deref(),
            Some("mine")
        );
    }

    #[test]
    fn a_yielded_or_released_lease_holds_nothing() {
        let rows = [
            row(1, 1, "<!-- loom:pr-lease role=doctor host=a sweep=first -->"),
            row(2, 1, "<!-- loom:pr-lease-yield role=doctor host=a sweep=first earliest_host=b earliest_sweep=x -->"),
            row(3, 1, "<!-- loom:pr-lease-released role=doctor host=b sweep=second -->"),
        ];
        assert_eq!(earliest_live_holder(&rows, "doctor", Utc::now(), ttl()), None);
    }

    #[test]
    fn rows_parse_from_ndjson_and_drop_malformed_lines() {
        let nd = b"{\"id\":7,\"updated_at\":\"2026-10-14T10:00:00Z\",\"body\":\"<!-- loom:pr-lease role=judge host=h sweep=s -->\"}\nnot json\n";
        let rows = parse_role_lease_rows(nd);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 7);
    }
}
