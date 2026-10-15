//! Adopting no-op holds from the forge (#12024).
//!
//! `noop-holds.json` ([`super::persist`]) only remembers the holds *this*
//! host applied, so a hold whose host was replaced, or whose file was lost,
//! had nothing left to release it when its inputs changed. The forge record
//! is enough to rebuild one: `loom:blocked`, the body park record
//! (`reason="no-op hold"`, [`park_hold::NOOP_HOLD_REASON`]) and the trusted
//! hold notice carrying the fingerprint digest the park was decided on
//! ([`NOOP_HOLD_FP_MARKER_PREFIX`]). Once per process, before its first
//! reconciliation, a host lists the open `loom:blocked` issues, and adopts
//! every no-op park it does not already track; reconciliation then releases
//! it on a fingerprint change like any other. An older notice without the
//! digest line is read from its `Fingerprint compared:` text, so a hold
//! moved off `loom:operator` by `labels migrate --holds` is adopted too.
//!
//! The read is the restored-park rule of #11930
//! (`quarantine_reconciliation::restored`): the forge is the source of truth
//! for a park. Adopted holds are reconciled on a slower clock
//! ([`ADOPTED_RECONCILE_INTERVAL`]), because every host adopts them, and they
//! are not written to this host's holds file, because the next process adopts
//! them again. Fail-open: an unreadable listing is retried later, and an
//! unreadable notice adopts nothing.

use super::*;

/// Most holds adopted in one pass.
const ADOPT_PER_PASS: usize = 25;

/// How soon a failed listing is retried.
const ADOPT_RETRY: std::time::Duration = std::time::Duration::from_secs(900);

/// Minimum gap between reconciliation reads of one adopted hold. Six times
/// [`RECONCILE_INTERVAL`]: every host adopts every hold, so the fleet still
/// looks at one far more often than this.
pub(super) const ADOPTED_RECONCILE_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(1800);

/// `--jq` over the open `loom:blocked` issue listing: one compact
/// `{number, body}` per issue (pull requests dropped).
const LIST_JQ: &str = ".[] | select(.pull_request == null) | {number, body: (.body // \"\")}";

/// The pre-#12024 notice's fingerprint line.
const LEGACY_FP_PREFIX: &str = "Fingerprint compared: `";

/// The fingerprint digest of the no-op hold standing in `bodies` (trusted
/// comment bodies, oldest first): the newest hold notice with no release
/// notice after it. `None` when no hold stands or its digest cannot be read.
#[must_use]
pub fn standing_noop_hold_fingerprint<S: AsRef<str>>(bodies: &[S]) -> Option<String> {
    let mut standing = None;
    for body in bodies {
        let body = body.as_ref();
        if body.contains(NOOP_HOLD_RELEASED_COMMENT_MARKER) {
            standing = None;
        } else if body.contains(NOOP_HOLD_APPLIED_COMMENT_MARKER) {
            standing = notice_fingerprint(body);
        }
    }
    standing
}

/// The digest one hold notice records.
fn notice_fingerprint(body: &str) -> Option<String> {
    if let Some(rest) = body.split(NOOP_HOLD_FP_MARKER_PREFIX).nth(1) {
        let d = rest.split("-->").next()?.trim();
        return is_digest(d).then(|| d.to_string());
    }
    let full = body.split(LEGACY_FP_PREFIX).nth(1)?.split('`').next()?;
    Some(legacy_digest(full))
}

/// Whether `body` carries the no-op hold's daemon park record.
fn is_noop_park(body: &str) -> bool {
    crate::park_record::parse(body).iter().any(|r| {
        park_hold::is_daemon_hold(r) && r.reason.as_deref() == Some(park_hold::NOOP_HOLD_REASON)
    })
}

impl SweepRegistry {
    /// Adopt every no-op park on the forge this host does not track (once
    /// per process; a failed listing is retried after [`ADOPT_RETRY`]).
    /// Returns how many were adopted.
    pub(crate) fn adopt_forge_noop_holds(&mut self) -> usize {
        let now = std::time::Instant::now();
        if self.noop_cooldown.adopted
            || self.config.skip_label_flip
            || self.noop_cooldown_config.hold_threshold == 0
            || self.noop_cooldown.adopt_retry_at.is_some_and(|t| now < t)
        {
            return 0;
        }
        let Some(candidates) = self.list_noop_parks() else {
            self.noop_cooldown.adopt_retry_at = Some(now + ADOPT_RETRY);
            return 0;
        };
        self.noop_cooldown.adopted = true;
        let mut adopted = 0;
        for issue in candidates {
            if adopted >= ADOPT_PER_PASS {
                break;
            }
            if self.noop_cooldown.streaks.contains_key(&issue) {
                continue;
            }
            let Some(fingerprint) = self.read_standing_noop_hold(issue) else {
                continue;
            };
            let count = self.noop_cooldown_config.hold_threshold;
            let mut streak = NoopStreak::recovered(count, fingerprint, ParkKind::HumanGate);
            streak.adopted = true;
            self.noop_cooldown.streaks.insert(issue, streak);
            adopted += 1;
            log::info!(
                "sweep_registry: adopted the no-op hold on #{issue} from the forge; it is \
                 released here when its inputs change (#12024)"
            );
        }
        adopted
    }

    /// The open `loom:blocked` issues whose body carries the no-op park
    /// record, or `None` when the listing cannot be read.
    fn list_noop_parks(&self) -> Option<Vec<u32>> {
        let (owner, repo) = self.resolve_owner_repo()?;
        let path = format!(
            "repos/{owner}/{repo}/issues?state=open&labels={}&per_page=100",
            crate::work_finder::BLOCKED_HOLD_LABEL
        );
        let out = self
            .gh_read("noop_hold.adopt_list", ["api", path.as_str(), "--paginate", "--jq", LIST_JQ])
            .ok()??;
        if !out.status.success() {
            return None;
        }
        let mut issues = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines().map(str::trim) {
            if line.is_empty() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let number = u32::try_from(v.get("number")?.as_u64()?).ok()?;
            let body = v.get("body").and_then(|b| b.as_str()).unwrap_or_default();
            if is_noop_park(body) {
                issues.push(number);
            }
        }
        Some(issues)
    }

    /// The fingerprint digest of the hold standing on `issue`, from its
    /// trusted comments (#9548: a marker from an untrusted author is prose).
    fn read_standing_noop_hold(&self, issue: u32) -> Option<String> {
        let (owner, repo) = self.resolve_owner_repo()?;
        let path = format!("repos/{owner}/{repo}/issues/{issue}/comments?per_page=100");
        let out = self
            .gh_read("noop_hold.adopt_comments", ["api", "--paginate", path.as_str()])
            .ok()??;
        if !out.status.success() {
            return None;
        }
        let bodies = crate::comment_trust::TrustPolicy::for_root(&self.config.workspace_root)
            .trusted_bodies(&out.stdout)?;
        standing_noop_hold_fingerprint(&bodies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(fp_line: &str) -> String {
        format!("{NOOP_HOLD_COMMENT_MARKER}\n{NOOP_HOLD_APPLIED_COMMENT_MARKER}\n{fp_line}\nheld")
    }

    #[test]
    fn the_newest_unreleased_notice_stands() {
        let a = notice(&format!("{NOOP_HOLD_FP_MARKER_PREFIX}0123456789abcdef -->"));
        let released = format!("{NOOP_HOLD_COMMENT_MARKER}\n{NOOP_HOLD_RELEASED_COMMENT_MARKER}");
        assert_eq!(
            standing_noop_hold_fingerprint(&[a.as_str()]).as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(standing_noop_hold_fingerprint(&[a.as_str(), released.as_str()]), None);
        let b = notice(&format!("{NOOP_HOLD_FP_MARKER_PREFIX}fedcba9876543210 -->"));
        assert_eq!(
            standing_noop_hold_fingerprint(&[a.as_str(), released.as_str(), b.as_str()]).as_deref(),
            Some("fedcba9876543210")
        );
    }

    #[test]
    fn a_pre_12024_notice_is_read_from_its_full_fingerprint() {
        let full = "labels=[loom:curated];open=true;comments=[];pr=none;deps=[]";
        let old = notice(&format!("Fingerprint compared: `{full}`"));
        assert_eq!(standing_noop_hold_fingerprint(&[old]), Some(legacy_digest(full)));
    }

    #[test]
    fn a_failure_notice_or_a_bad_digest_stands_for_nothing() {
        let failed = format!("{NOOP_HOLD_COMMENT_MARKER}\n{NOOP_HOLD_FAILED_COMMENT_MARKER}");
        assert_eq!(standing_noop_hold_fingerprint(&[failed]), None);
        let bad = notice(&format!("{NOOP_HOLD_FP_MARKER_PREFIX}not-a-digest -->"));
        assert_eq!(standing_noop_hold_fingerprint(&[bad]), None);
    }

    #[test]
    fn only_the_noop_park_record_marks_a_noop_park() {
        let record =
            park_hold::render_hold_record(park_hold::NOOP_HOLD_REASON, "2026-10-14T00:00:00Z");
        assert!(is_noop_park(&format!("body\n\n{record}\n")));
        let prless =
            park_hold::render_hold_record(park_hold::PRLESS_HOLD_REASON, "2026-10-14T00:00:00Z");
        assert!(!is_noop_park(&prless));
        assert!(!is_noop_park("Blocked by #3"));
    }
}
