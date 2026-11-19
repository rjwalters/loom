//! The writer-exhaustion refusal (`writer_rate_limited`, #13746): classify a
//! confirmed primary rate-limit refusal of the writer a `lease pr claim`
//! write would carry, hand the facts to the daemon as a bounded typed line,
//! and hold that writer installation's route until its reset.
//!
//! # Why
//!
//! When the writer credential is rate-limited, `lease pr claim` used to
//! answer `claim_unverified` — "could not read this PR's lease records" for a
//! probe or read refusal, "could not post the lease comment" for a POST
//! refusal — so the step telemetry, the fleet-check STARVED row and the
//! operator all named the wrong cause (2026-11-10: 702 wasted draws in 16 h).
//! The reads were healthy; the *writer* was exhausted.
//!
//! # The three pieces
//!
//! 1. **Classification** ([`WriterLimited::classify`]): a failing writer
//!    call — the write-scope permission probe or the lease comment POST — is
//!    a confirmed writer rate limit when its text carries a primary
//!    rate-limit signature (or the status is a `429` that is not a gateway
//!    reserve refusal) and is not a secondary throttle. A bare auth denial,
//!    an ordinary permission refusal and a secondary limit are not
//!    classified: they keep their own diagnostics.
//! 2. **Typed handoff** ([`WriterLimited::handoff_line`] /
//!    [`parse_handoff`]): the claim prints one bounded, strict-format line on
//!    *stdout* (vendor output goes to stderr only, so a substring there can
//!    never create the distinct outcome); the daemon's preclaim parses it —
//!    and nothing else — into a `writer_rate_limited` step refusal. The final
//!    stderr line stays the human diagnostic, naming the stage, HTTP status,
//!    resource, safe credential ids and the UTC reset when confirmed; bounded
//!    facts only, never a credential, header dump, raw response, argv or
//!    credential-file path.
//! 3. **The route hold** ([`WriterLimited::arm_hold`] / [`hold_note`]): the
//!    daemon arms a hold for the exact bucket key (account, owner,
//!    installation, resource — [`crate::forge_bucket_book::BucketKey`]) the
//!    next write would spend, from a confirmed future reset, so the work
//!    finder stops drawing that writer's candidates until the reset instead
//!    of drawing and refusing them. Reader depletion, a wrong owner, an
//!    unknown ambient identity and a reset that already passed arm nothing.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::forge_bucket_book::{self, BucketKey, Reading, Resource};

/// The stable refusal reason, end to end: the claim's handoff token, the
/// `loom.dispatch.step` span's `loom.dispatch.reason`, the fleet
/// worker-to-captain `refuse` token (`fleet_dispatch::proto::REFUSE_REASONS`),
/// and the tick pass counter's key.
pub const OUTCOME: &str = "writer_rate_limited";

/// The stdout line prefix of the typed handoff. Vendor output never reaches
/// the claim's stdout, so a line with this prefix is the claim's own.
const HANDOFF_PREFIX: &str = "loom-claim-writer-limited ";

/// Longest handoff line believed. The fields are short and fixed; anything
/// longer was not written by this binary.
const HANDOFF_MAX_BYTES: usize = 512;

/// Which writer call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriterStage {
    /// The write-scope permission probe (the credential's own write check).
    PermissionProbe,
    /// The lease comment POST.
    LeasePost,
}

impl WriterStage {
    /// The diagnostic token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PermissionProbe => "permission_probe",
            Self::LeasePost => "comment_post",
        }
    }
}

/// A confirmed writer rate-limit refusal, with the bounded facts the claim
/// reports and the daemon holds on. Every field is an allowlisted fact: ids
/// are validated shapes, never a token or a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct WriterLimited {
    pub stage: WriterStage,
    /// The HTTP status, when the refusal carried one.
    #[serde(default)]
    pub status: Option<u16>,
    /// The billed bucket the refused call spends (REST: `core`).
    pub resource: String,
    /// `app-<id>` of the writer credential, when attributable.
    #[serde(default)]
    pub account: Option<String>,
    /// The installation id, when a sidecar or the refusal named one.
    #[serde(default)]
    pub installation: Option<String>,
    /// The installation's owner, when attributable (the bucket key's owner).
    #[serde(default)]
    pub owner: Option<String>,
    /// The confirmed future reset (epoch seconds), when one was read.
    #[serde(default)]
    pub reset_epoch: Option<i64>,
}

impl WriterLimited {
    /// A refusal of `stage` confirmed by `text` (gh's stderr) at `status`
    /// (`None`: parse it from the text). `None` when the text is not a
    /// confirmed primary rate limit. Pure.
    #[must_use]
    pub fn classify(stage: WriterStage, status: Option<u16>, text: &str) -> Option<Self> {
        // A secondary (abuse / concurrency) throttle is not a primary
        // exhaustion: it must not name the writer rate limit, let alone hold
        // a route.
        if crate::rate_limit_breaker::evidence::is_secondary_limit(text) {
            return None;
        }
        // #12310: the read gateway's quota-reserve refusal is a `429` that is
        // not a limit.
        if crate::gh_invocation::purpose::is_reserve_refusal(text) {
            return None;
        }
        let lowered = text.to_ascii_lowercase();
        let primary_text = lowered.contains("api rate limit exceeded")
            || lowered.contains("api rate limit already exceeded");
        if !primary_text && status != Some(429) {
            return None;
        }
        Some(Self {
            stage,
            status: status.or_else(|| parse_http_status(text)),
            resource: Resource::Core.as_str().to_string(),
            account: None,
            installation: installation_id(text),
            owner: None,
            reset_epoch: None,
        })
    }

    /// Attribute the refusal to the writer identity the write would carry
    /// (`dir`'s sidecar), refusing a cross-owner mismatch: a credential for
    /// another owner says nothing about this route. Pure.
    #[must_use]
    pub fn with_identity(mut self, dir: Option<&Path>, nwo: &str) -> Self {
        let Some(dir) = dir else {
            return self;
        };
        let Some(id) = forge_bucket_book::dir_identity(dir, &forge_bucket_book::classify_dir(dir))
        else {
            return self;
        };
        let owner = owner_of(nwo);
        // A sidecar naming a different owner than the write's is the wrong
        // route: attribute nothing (a wrong owner must not hold).
        if id
            .owner
            .as_deref()
            .is_some_and(|sidecar_owner| *sidecar_owner != owner)
        {
            return self;
        }
        self.account = Some(id.account);
        self.owner = Some(owner);
        self.installation = id.installation.or(self.installation);
        self
    }

    /// The bucket key this refusal holds, when its identity is complete and
    /// attributable: an unknown ambient identity (`app-unknown`) or a missing
    /// owner holds nothing. Pure.
    #[must_use]
    pub fn hold_key(&self) -> Option<BucketKey> {
        let account = self.account.as_deref().filter(|a| !a.is_empty())?;
        if account == "app-unknown" {
            return None;
        }
        let owner = self.owner.as_deref()?;
        Some(
            BucketKey::new(account, owner, Resource::Core)
                .with_installation(self.installation.as_deref()),
        )
    }

    /// The route-hold key of a write to `target` (`owner/repo`) from `cwd`:
    /// the credential directory the `gh` facade would select, identified by
    /// its sidecar — no forge call, no credential read. `None` when the
    /// effective writer is unknown or unattributable. Pure.
    #[must_use]
    pub fn route_key(cwd: Option<&Path>, target: &str) -> Option<BucketKey> {
        let dir = writer_dir_for(cwd, target)?;
        let id = forge_bucket_book::dir_identity(&dir, &forge_bucket_book::classify_dir(&dir))?;
        if id.account == "app-unknown" {
            return None;
        }
        let owner = owner_of(target);
        if id
            .owner
            .as_deref()
            .is_some_and(|sidecar_owner| *sidecar_owner != owner)
        {
            return None;
        }
        Some(
            BucketKey::new(&id.account, &owner, Resource::Core)
                .with_installation(id.installation.as_deref()),
        )
    }

    /// The refusal's final stderr line: the stage, HTTP status, resource,
    /// safe credential id when attributable, and UTC reset when confirmed —
    /// bounded facts only. Pure.
    #[must_use]
    pub fn stderr_line(&self) -> String {
        let mut line = format!(
            "writer rate-limited: stage={} status={} resource={}",
            self.stage.as_str(),
            self.status
                .map_or_else(|| "unknown".to_string(), |s| s.to_string()),
            self.resource
        );
        if let Some(account) = &self.account {
            line.push_str(&format!(" account={account}"));
        }
        if let Some(installation) = &self.installation {
            line.push_str(&format!(" installation={installation}"));
        }
        match self.reset_utc() {
            Some(at) => line.push_str(&format!(
                " resets={}",
                at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            )),
            None => line.push_str(" reset unconfirmed"),
        }
        line.push_str(" — no claim taken; stand down");
        line
    }

    /// The confirmed reset as a UTC instant, when it is still in the future
    /// at `now`. Pure.
    #[must_use]
    pub fn reset_utc_at(&self, now: i64) -> Option<DateTime<Utc>> {
        let reset = self.reset_epoch?;
        if reset <= now {
            return None;
        }
        DateTime::from_timestamp(reset, 0)
    }

    /// The confirmed reset at the current clock. Pure.
    #[must_use]
    pub fn reset_utc(&self) -> Option<DateTime<Utc>> {
        self.reset_utc_at(Utc::now().timestamp())
    }

    /// The typed handoff line for stdout. Pure.
    #[must_use]
    pub fn handoff_line(&self) -> String {
        // The fields are validated shapes; the cap cannot bind in practice,
        // and a line over it is refused on the parse side.
        format!("{HANDOFF_PREFIX}{}", serde_json::to_string(self).unwrap_or_default())
    }

    /// Confirm the reset from evidence that needs no per-candidate polling:
    /// the persisted bucket-book reading for this exact installation (a
    /// confirmed exhaustion: `remaining == 0`, still fresh), else one bounded
    /// free `rate_limit` probe of the writer directory (`gh api rate_limit`
    /// is unmetered). A reset that already passed, a healthy reading and a
    /// failed probe leave `reset_epoch` unset — the refusal still classifies,
    /// but holds nothing. Inject `gh` with `program` (tests).
    pub fn confirm_reset(&mut self, cwd: Option<&Path>, program: Option<&Path>) {
        let now = Utc::now().timestamp();
        if self.reset_epoch.is_some_and(|r| r > now) {
            return;
        }
        self.reset_epoch = None;
        let Some(key) = self.hold_key() else {
            return;
        };
        if let Some(reset) = self.reset_from_snapshot(now, &key) {
            self.reset_epoch = Some(reset);
        } else {
            self.reset_epoch = self.reset_from_probe(cwd, program, now);
        }
    }

    /// The persisted bucket book's confirmed-exhaustion reading for `key`.
    fn reset_from_snapshot(&self, now: i64, key: &BucketKey) -> Option<i64> {
        let dir = crate::forge_call_stats::host_sink_dir()?;
        reset_from_snapshot_at(&forge_bucket_book::load(&dir, now), key, now)
    }

    /// One bounded free probe of the writer directory for `key`'s owner,
    /// booked into the bucket book (and snapshotted) like every probe. A
    /// probe that answers healthy, fails, or is unparseable resets nothing.
    fn reset_from_probe(
        &self,
        cwd: Option<&Path>,
        program: Option<&Path>,
        now: i64,
    ) -> Option<i64> {
        let key = self.hold_key()?;
        let owner = key.owner.as_str();
        let root = cwd?;
        let targets = forge_bucket_book::probe_targets(root, std::time::SystemTime::now());
        let target = targets.into_iter().find(|t| {
            t.role == crate::forge_identity::IdentityRole::Writer
                && t.owner == owner
                && self.account.as_deref().is_none_or(|a| a == t.account)
        })?;
        let out = match forge_bucket_book::probe_invocation(&target, program).run() {
            crate::cmd_out::CmdOutcome::Ran(o) if o.status.success() => o,
            _ => return None,
        };
        let resp =
            crate::forge_listing::parse_http_response(&String::from_utf8_lossy(&out.stdout))?;
        let reading = forge_bucket_book::parse_probe(&resp.body, now)
            .into_iter()
            .find(|(r, _)| *r == Resource::Core)
            .map(|(_, reading)| reading)?;
        let reset = reading.reset_epoch;
        if reading.remaining == Some(0) && reset > now {
            forge_bucket_book::insert(key, reading);
            if let Some(dir) = crate::forge_call_stats::host_sink_dir() {
                let _ = forge_bucket_book::persist(&dir, now);
            }
            return Some(reset);
        }
        None
    }

    /// Arm the daemon's route hold for this refusal's key: only a confirmed
    /// exhaustion with a valid future reset and a complete, attributable
    /// identity holds a route. `false` when nothing was armed.
    pub fn arm_hold(&self, now: i64) -> bool {
        let Some(key) = self.hold_key() else {
            return false;
        };
        let Some(reset) = self.reset_epoch else {
            return false;
        };
        forge_bucket_book::arm_writer_hold(key, reset, now)
    }
}

/// `owner/repo`'s owner, lowercased.
#[must_use]
pub fn owner_of(nwo: &str) -> String {
    nwo.split_once('/')
        .map_or_else(|| nwo.to_string(), |(owner, _)| owner.to_string())
        .to_ascii_lowercase()
}

/// The writer credential directory a call from `cwd` against `target` would
/// carry: the facade's own selection (`gh_config_dir_for_call`), else the
/// ambient `GH_CONFIG_DIR` (a claim child inherits the daemon's). Pure.
fn writer_dir_for(cwd: Option<&Path>, target: &str) -> Option<PathBuf> {
    crate::credential_preflight::gh_config_dir_for_call(cwd, Some(target))
        .or_else(|| std::env::var_os("GH_CONFIG_DIR").map(PathBuf::from))
}

/// `HTTP <status>` in gh's failure line.
fn parse_http_status(text: &str) -> Option<u16> {
    let lowered = text.to_ascii_lowercase();
    let at = lowered.find("http ")?;
    let digits: String = lowered[at + "http ".len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// `for installation ID <digits>` in GitHub's primary-limit message: a safe
/// installation id, never a token.
fn installation_id(text: &str) -> Option<String> {
    let lowered = text.to_ascii_lowercase();
    let at = lowered.find("for installation id ")?;
    let digits: String = lowered[at + "for installation id ".len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (!digits.is_empty() && digits.len() <= 20).then_some(digits)
}

/// The confirmed-exhaustion reset for `key` among `snapshot`'s fresh
/// readings: a bucket that says `remaining == 0`. Pure.
#[must_use]
pub fn reset_from_snapshot_at(
    snapshot: &[(BucketKey, Reading)],
    key: &BucketKey,
    now: i64,
) -> Option<i64> {
    snapshot
        .iter()
        .filter(|(k, r)| k == key && r.remaining == Some(0) && r.reset_epoch > now)
        .map(|(_, r)| r.reset_epoch)
        .max()
}

/// Parse the typed handoff out of a claim child's stdout: the last line with
/// [`HANDOFF_PREFIX`], within the bound, decoding to exactly the known
/// fields with validated shapes. Anything else — vendor stderr shaped like a
/// marker, a truncated line, unknown fields — parses as `None`. Pure.
#[must_use]
pub fn parse_handoff(stdout: &str) -> Option<WriterLimited> {
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.starts_with(HANDOFF_PREFIX))?;
    if line.len() > HANDOFF_MAX_BYTES {
        return None;
    }
    let limited: WriterLimited = serde_json::from_str(line.strip_prefix(HANDOFF_PREFIX)?).ok()?;
    /// An `app-<digits>` account label, 20 digits at most.
    fn plausible_app(a: &str) -> bool {
        let Some(id) = a.strip_prefix("app-") else {
            return false;
        };
        !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())
    }

    /// Numeric ids of at most 20 digits.
    fn plausible_digits(v: &str) -> bool {
        !v.is_empty() && v.len() <= 20 && v.bytes().all(|b| b.is_ascii_digit())
    }

    let shaped = limited.account.as_deref().is_none_or(plausible_app)
        && limited.installation.as_deref().is_none_or(plausible_digits)
        && limited
            .owner
            .as_deref()
            .is_none_or(forge_bucket_book::valid_owner);
    shaped.then_some(limited)
}

/// The active route hold for a write to `target` from `cwd`, at `now`:
/// `Some(reset_epoch)` while the bucket key's confirmed hold stands. `None`
/// when the writer route is unknown, unattributable, not held, or past its
/// reset — the caller draws as usual. Pure.
#[must_use]
pub fn hold_note(cwd: Option<&Path>, target: &str, now: i64) -> Option<i64> {
    let key = WriterLimited::route_key(cwd, target)?;
    forge_bucket_book::writer_hold(&key, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMARY_403: &str =
        "gh: API rate limit exceeded for installation ID 151241341 (HTTP 403)";
    const SECONDARY: &str =
        "gh: You have exceeded a secondary rate limit. Please wait a few minutes before you try again. (HTTP 403)";
    const AUTH: &str = "gh: Bad credentials (HTTP 401)";
    const PERMISSION: &str = "gh: Resource not accessible by integration (HTTP 403)";

    #[test]
    fn a_primary_limit_is_confirmed_at_both_stages() {
        for stage in [WriterStage::PermissionProbe, WriterStage::LeasePost] {
            let l = WriterLimited::classify(stage, None, PRIMARY_403).unwrap();
            assert_eq!(l.stage, stage);
            assert_eq!(l.status, Some(403));
            assert_eq!(l.resource, "core");
            assert_eq!(l.installation.as_deref(), Some("151241341"));
            assert_eq!(l.account, None);
            assert_eq!(l.reset_epoch, None);
        }
    }

    #[test]
    fn a_primary_429_with_or_without_a_signature_confirms() {
        let l = WriterLimited::classify(
            WriterStage::LeasePost,
            Some(429),
            "gh: API rate limit exceeded (HTTP 429)",
        )
        .unwrap();
        assert_eq!(l.status, Some(429));
        let bare = WriterLimited::classify(WriterStage::LeasePost, Some(429), "too many requests")
            .unwrap();
        assert_eq!(bare.status, Some(429));
        assert_eq!(bare.installation, None);
    }

    #[test]
    fn a_secondary_an_auth_a_permission_and_a_reserve_refusal_do_not_classify() {
        for text in [SECONDARY, PERMISSION, AUTH] {
            assert!(
                WriterLimited::classify(WriterStage::LeasePost, None, text).is_none(),
                "{text}"
            );
        }
        // An ordinary 403 with no primary signature stays unclassified.
        assert!(
            WriterLimited::classify(WriterStage::PermissionProbe, None, "gh: boom (HTTP 403)")
                .is_none()
        );
    }

    #[test]
    fn a_status_without_a_usable_reset_classifies_but_holds_nothing() {
        let l = WriterLimited::classify(WriterStage::LeasePost, None, PRIMARY_403).unwrap();
        assert_eq!(l.reset_epoch, None);
        assert!(l.hold_key().is_none(), "no identity, no hold key");
        assert!(!l.arm_hold(1_000), "no reset, no hold");
    }

    #[test]
    fn the_handoff_round_trips_and_refuses_everything_else() {
        let mut l =
            WriterLimited::classify(WriterStage::PermissionProbe, None, PRIMARY_403).unwrap();
        l.account = Some("app-4486636".to_string());
        l.owner = Some("acme".to_string());
        l.installation = Some("151241341".to_string());
        l.reset_epoch = Some(1_760_000_000);
        let parsed = parse_handoff(&format!("noise\n{}\n", l.handoff_line())).unwrap();
        assert_eq!(parsed, l);

        // An unknown field is refused (deny_unknown_fields).
        let bad = format!(
            "{HANDOFF_PREFIX}{{\"stage\":\"comment_post\",\"resource\":\"core\",\"bogus\":1}}"
        );
        assert!(parse_handoff(&bad).is_none());
        // A malformed identity shape is refused.
        let bad_account = format!(
            "{HANDOFF_PREFIX}{{\"stage\":\"comment_post\",\"resource\":\"core\",\"account\":\"app-token\"}}"
        );
        assert!(parse_handoff(&bad_account).is_none());
        let bad_owner = format!(
            "{HANDOFF_PREFIX}{{\"stage\":\"comment_post\",\"resource\":\"core\",\"owner\":\"not_a_owner!\"}}"
        );
        assert!(parse_handoff(&bad_owner).is_none());
        // Over the bound: not this binary's line.
        let long = format!("{HANDOFF_PREFIX}{}", "x".repeat(HANDOFF_MAX_BYTES + 1));
        assert!(parse_handoff(&long).is_none());
        assert!(parse_handoff("no handoff here\n").is_none());
    }

    #[test]
    fn the_stderr_line_names_the_bounded_facts_only() {
        let l = WriterLimited::classify(WriterStage::LeasePost, None, PRIMARY_403).unwrap();
        assert_eq!(
            l.stderr_line(),
            "writer rate-limited: stage=comment_post status=403 resource=core \
             installation=151241341 reset unconfirmed — no claim taken; stand down"
        );
        let mut full = WriterLimited::classify(WriterStage::LeasePost, None, PRIMARY_403).unwrap();
        full.account = Some("app-4486636".to_string());
        full.installation = Some("151241341".to_string());
        full.owner = Some("acme".to_string());
        full.reset_epoch = Some(chrono::Utc::now().timestamp() + 600);
        let line = full.stderr_line();
        assert!(line.contains("stage=comment_post"), "{line}");
        assert!(line.contains("status=403"), "{line}");
        assert!(line.contains("resource=core"), "{line}");
        assert!(line.contains("account=app-4486636"), "{line}");
        assert!(line.contains("installation=151241341"), "{line}");
        assert!(line.contains("resets=20"), "{line}");
        assert!(line.ends_with("— no claim taken; stand down"), "{line}");
    }

    #[test]
    fn an_identity_from_another_owner_never_attributes() {
        // A path that is not a credential directory classifies as `Other`,
        // so nothing is attributed.
        let dir = std::env::temp_dir().join("loom-wl-tests-does-not-exist");
        let l = WriterLimited::classify(WriterStage::LeasePost, None, PRIMARY_403)
            .unwrap()
            .with_identity(Some(&dir), "acme/widgets");
        assert_eq!(l.account, None, "an unidentifiable dir attributes nothing");
        assert!(l.hold_key().is_none());
    }

    #[test]
    fn a_snapshot_reading_of_an_exhausted_bucket_confirms_the_reset() {
        let key =
            BucketKey::new("app-4486636", "acme", Resource::Core).with_installation(Some("1"));
        let now = 1_760_000_000;
        let fresh = Reading {
            limit: Some(5000),
            remaining: Some(0),
            used: Some(5000),
            reset_epoch: now + 600,
            observed_at: now - 10,
            source: forge_bucket_book::Source::Refusal,
        };
        let healthy = Reading {
            remaining: Some(4000),
            ..fresh
        };
        assert_eq!(reset_from_snapshot_at(&[(key.clone(), fresh)], &key, now), Some(now + 600));
        assert_eq!(
            reset_from_snapshot_at(&[(key.clone(), healthy)], &key, now),
            None,
            "a healthy reading contradicts the failure: no reset"
        );
        assert_eq!(
            reset_from_snapshot_at(&[(key.clone(), fresh)], &key, now + 700),
            None,
            "a reset that passed holds nothing"
        );
    }

    #[test]
    fn arming_validates_reset_identity_and_past_resets() {
        let now = 1_760_000_000;
        let mut l = WriterLimited::classify(WriterStage::LeasePost, None, PRIMARY_403).unwrap();
        assert!(!l.arm_hold(now), "no identity");
        l.account = Some("app-4486636".to_string());
        l.owner = Some("acme".to_string());
        assert!(!l.arm_hold(now), "no reset");
        l.reset_epoch = Some(now - 1);
        assert!(!l.arm_hold(now), "a past reset");
        l.reset_epoch = Some(now + 600);
        assert!(l.arm_hold(now));
        let key = l.hold_key().unwrap();
        assert_eq!(forge_bucket_book::writer_hold(&key, now), Some(now + 600));
        assert_eq!(forge_bucket_book::writer_hold(&key, now + 601), None, "reconsidered at reset");
        // An unknown ambient identity never arms.
        let mut unknown = l.clone();
        unknown.account = Some("app-unknown".to_string());
        assert!(unknown.hold_key().is_none());
        assert!(!unknown.arm_hold(now));
        forge_bucket_book::clear_writer_holds_for_test();
    }

    #[test]
    fn the_hold_store_keys_by_installation_witness() {
        let now = 1_760_000_000;
        let base = BucketKey::new("app-4486636", "acme", Resource::Core);
        let witnessed = base.clone().with_installation(Some("151241341"));
        assert!(
            forge_bucket_book::arm_writer_hold(witnessed, now + 600, now),
            "arm with a witnessed installation"
        );
        // The same bucket without a witness still finds the hold (the
        // installation is a witness label, not part of the key).
        assert_eq!(forge_bucket_book::writer_hold(&base, now), Some(now + 600));
        // Another owner's bucket is not held.
        let other = BucketKey::new("app-4486636", "other", Resource::Core);
        assert_eq!(forge_bucket_book::writer_hold(&other, now), None);
        // An unknown account cannot arm.
        let unknown = BucketKey::new("app-unknown", "acme", Resource::Core);
        assert!(!forge_bucket_book::arm_writer_hold(unknown, now + 600, now));
        forge_bucket_book::clear_writer_holds_for_test();
    }

    #[test]
    fn the_owners_of_slugs_lowercase() {
        assert_eq!(owner_of("Acme/Widgets"), "acme");
        assert_eq!(owner_of("solo"), "solo");
    }

    /// The full producer chain end to end (#13746): the child classifies the
    /// refusal under a synthetic credential sidecar, hands the bounded facts
    /// over, the daemon parses them, arms the route hold, and the tick's
    /// admission finds it for that route until exactly the reset — while
    /// another writer's route stays drawable.
    #[test]
    #[serial_test::serial]
    fn the_handoff_arms_the_hold_the_admission_finds() {
        use crate::env_test_support::EnvGuard;
        let _env = EnvGuard::save(&["GH_CONFIG_DIR"]);
        let dir = tempfile::TempDir::new().unwrap();
        let cred = dir
            .path()
            .join(".loom")
            .join("gh-config-by-owner")
            .join("acme");
        std::fs::create_dir_all(&cred).unwrap();
        let side = crate::forge_identity::Sidecar {
            app_id: "4486636".to_string(),
            slug: None,
            installation_id: "151241341".to_string(),
            owner: Some("acme".to_string()),
            role: crate::forge_identity::SidecarRole::Writer,
            expires_at: "2027-01-01T00:00:00Z".to_string(),
        };
        std::fs::write(cred.join("identity.json"), serde_json::to_vec(&side).unwrap()).unwrap();
        std::env::set_var("GH_CONFIG_DIR", &cred);

        // The child: classify under the effective writer, attribute it, and
        // emit the handoff (its reset confirmation finds no evidence here).
        let l = WriterLimited::classify(WriterStage::LeasePost, None, PRIMARY_403)
            .unwrap()
            .with_identity(writer_dir_for(None, "acme/widgets").as_deref(), "acme/widgets");
        assert_eq!(l.account.as_deref(), Some("app-4486636"), "{l:?}");
        assert_eq!(l.owner.as_deref(), Some("acme"));
        assert_eq!(l.installation.as_deref(), Some("151241341"));
        let handoff = l.handoff_line();

        // The daemon: parse, confirm the reset, arm, and admit.
        let mut parsed = parse_handoff(&handoff).unwrap();
        assert_eq!(parsed, l);
        let now = Utc::now().timestamp();
        parsed.reset_epoch = Some(now + 600);
        assert!(parsed.arm_hold(now));
        assert_eq!(hold_note(None, "acme/widgets", now), Some(now + 600));
        assert_eq!(
            hold_note(None, "acme/widgets", now + 600),
            None,
            "the route is reconsidered at exactly the reset"
        );
        assert!(
            hold_note(None, "other/repo", now).is_none(),
            "another writer's route is not held"
        );
        forge_bucket_book::clear_writer_holds_for_test();
    }

    /// A writer credential directory with a published sidecar naming `owner`
    /// (the shape every publication writes, `forge_identity::sidecar`).
    fn writer_dir_with_sidecar(dir: &Path, owner: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let side = crate::forge_identity::Sidecar {
            app_id: "4486636".to_string(),
            slug: None,
            installation_id: "151241341".to_string(),
            owner: Some(owner.to_string()),
            role: crate::forge_identity::SidecarRole::Writer,
            expires_at: "2027-01-01T00:00:00Z".to_string(),
        };
        std::fs::write(dir.join("identity.json"), serde_json::to_vec(&side).unwrap()).unwrap();
    }

    /// The `GH_CONFIG_DIR` the spawn (`sweep_registry/step_spawn/lease.rs`)
    /// puts on the claim child, read back off the built `Command`.
    fn spawned_child_dir(root: &Path) -> Option<PathBuf> {
        let mut cmd = std::process::Command::new("true");
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        cmd.get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("GH_CONFIG_DIR"))
            .and_then(|(_, v)| v)
            .map(PathBuf::from)
    }

    /// #14559 item 2, measured: for a registered (cross-owner) root, the
    /// directory the claim child inherited and the directory the tick's
    /// route resolution picks are the same, so the key the child arms is
    /// the key the tick finds — the armed hold cannot go missing.
    #[test]
    #[serial_test::serial]
    fn a_registered_roots_spawn_env_and_route_resolution_agree() {
        use crate::credential_preflight::{
            clear_owner_root_registry, gh_config_dir_for_call, register_owner_gh_config_dir,
            register_root_gh_config_dir,
        };
        let _env = crate::env_test_support::EnvGuard::save(&["GH_CONFIG_DIR"]);
        clear_owner_root_registry();
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("widgets-checkout");
        std::fs::create_dir_all(&root).unwrap();
        let owner_dir = dir
            .path()
            .join(".loom")
            .join("gh-config-by-owner")
            .join("acme");
        writer_dir_with_sidecar(&owner_dir, "acme");
        // What `register_plan` does for an established cross-owner owner.
        register_root_gh_config_dir(&root, &owner_dir);
        register_owner_gh_config_dir("acme", &owner_dir);

        let child_dir = spawned_child_dir(&root);
        let route_dir = gh_config_dir_for_call(Some(&root), Some("acme/widgets"));
        assert_eq!(child_dir, route_dir, "the two resolvers must agree");
        assert_eq!(child_dir.as_deref(), Some(owner_dir.as_path()));

        // End to end: arm the way the child's handoff arms, find the way the
        // tick resolves the route.
        let l = WriterLimited::classify(WriterStage::LeasePost, None, PRIMARY_403)
            .unwrap()
            .with_identity(child_dir.as_deref(), "acme/widgets");
        let mut parsed = parse_handoff(&l.handoff_line()).unwrap();
        let now = Utc::now().timestamp();
        parsed.reset_epoch = Some(now + 600);
        assert!(parsed.arm_hold(now));
        assert_eq!(hold_note(Some(&root), "acme/widgets", now), Some(now + 600));
        forge_bucket_book::clear_writer_holds_for_test();
        clear_owner_root_registry();
    }

    /// #14559 item 2, measured: the workspace-root owner's own repos are
    /// deliberately absent from the registry, so the child stays on the
    /// process-global `GH_CONFIG_DIR` (startup's `set_var`) and the tick's
    /// resolution falls back to that same env — the same directory again.
    #[test]
    #[serial_test::serial]
    fn an_unregistered_root_resolves_the_route_through_the_same_process_env() {
        use crate::credential_preflight::{clear_owner_root_registry, gh_config_dir_for_call};
        let _env = crate::env_test_support::EnvGuard::save(&["GH_CONFIG_DIR"]);
        clear_owner_root_registry();
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("widgets-checkout");
        std::fs::create_dir_all(&root).unwrap();
        let owner_dir = dir
            .path()
            .join(".loom")
            .join("gh-config-by-owner")
            .join("acme");
        writer_dir_with_sidecar(&owner_dir, "acme");
        std::env::set_var("GH_CONFIG_DIR", &owner_dir);

        // The spawn leaves the child's env untouched (no registration).
        assert_eq!(spawned_child_dir(&root), None);
        // Both registry arms miss; the tick falls back to the env too.
        assert_eq!(gh_config_dir_for_call(Some(&root), Some("acme/widgets")), None);

        let child_dir = writer_dir_for(Some(&root), "acme/widgets");
        assert_eq!(child_dir.as_deref(), Some(owner_dir.as_path()));
        let l = WriterLimited::classify(WriterStage::LeasePost, None, PRIMARY_403)
            .unwrap()
            .with_identity(child_dir.as_deref(), "acme/widgets");
        let mut parsed = parse_handoff(&l.handoff_line()).unwrap();
        let now = Utc::now().timestamp();
        parsed.reset_epoch = Some(now + 600);
        assert!(parsed.arm_hold(now));
        assert_eq!(hold_note(Some(&root), "acme/widgets", now), Some(now + 600));
        forge_bucket_book::clear_writer_holds_for_test();
        clear_owner_root_registry();
    }

    /// #14559 item 2, measured: the one transient window — a root not yet
    /// registered at spawn (startup's cross-owner mint failed, #11957), so
    /// the child inherits the root owner's process credential while the
    /// target is another owner's repo — arms nothing: the cross-owner guard
    /// attributes nothing, so there is no hold to miss.
    #[test]
    #[serial_test::serial]
    fn a_mismatched_sidecar_arms_nothing_so_no_hold_can_go_missing() {
        use crate::credential_preflight::clear_owner_root_registry;
        let _env = crate::env_test_support::EnvGuard::save(&["GH_CONFIG_DIR"]);
        clear_owner_root_registry();
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("widgets-checkout");
        std::fs::create_dir_all(&root).unwrap();
        let acme_dir = dir
            .path()
            .join(".loom")
            .join("gh-config-by-owner")
            .join("acme");
        writer_dir_with_sidecar(&acme_dir, "acme");
        std::env::set_var("GH_CONFIG_DIR", &acme_dir);

        let l = WriterLimited::classify(WriterStage::LeasePost, None, PRIMARY_403)
            .unwrap()
            .with_identity(Some(acme_dir.as_path()), "other/widgets");
        assert_eq!(l.account, None, "the cross-owner guard attributes nothing");
        let mut parsed = parse_handoff(&l.handoff_line()).unwrap();
        let now = Utc::now().timestamp();
        parsed.reset_epoch = Some(now + 600);
        assert!(!parsed.arm_hold(now));
        assert_eq!(hold_note(Some(&root), "other/widgets", now), None);
        forge_bucket_book::clear_writer_holds_for_test();
        clear_owner_root_registry();
    }

    /// #14559 item 2, measured: even the one remaining divergence window —
    /// the tick resolving a directory other than the one the child inherited
    /// (a registration that changed between spawn and tick) — cannot split
    /// the key: the hold key is the credential identity (app, owner,
    /// installation), and both directories carry the same one.
    #[test]
    #[serial_test::serial]
    fn a_registry_repoint_with_the_same_identity_still_finds_the_armed_key() {
        use crate::credential_preflight::{clear_owner_root_registry, register_root_gh_config_dir};
        let _env = crate::env_test_support::EnvGuard::save(&["GH_CONFIG_DIR"]);
        clear_owner_root_registry();
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("widgets-checkout");
        std::fs::create_dir_all(&root).unwrap();
        let dir_a = dir
            .path()
            .join(".loom")
            .join("gh-config-by-owner")
            .join("acme");
        let dir_b = dir
            .path()
            .join("other-checkout")
            .join(".loom")
            .join("gh-config-by-owner")
            .join("acme");
        writer_dir_with_sidecar(&dir_a, "acme");
        writer_dir_with_sidecar(&dir_b, "acme");
        // The child inherited dir_b at spawn; the tick now resolves dir_a.
        std::env::set_var("GH_CONFIG_DIR", &dir_b);
        register_root_gh_config_dir(&root, &dir_a);

        let l = WriterLimited::classify(WriterStage::LeasePost, None, PRIMARY_403)
            .unwrap()
            .with_identity(Some(dir_b.as_path()), "acme/widgets");
        let mut parsed = parse_handoff(&l.handoff_line()).unwrap();
        let now = Utc::now().timestamp();
        parsed.reset_epoch = Some(now + 600);
        assert!(parsed.arm_hold(now));
        assert_eq!(
            hold_note(Some(&root), "acme/widgets", now),
            Some(now + 600),
            "same app + owner + installation = same bucket key, whatever the path"
        );
        forge_bucket_book::clear_writer_holds_for_test();
        clear_owner_root_registry();
    }

    #[test]
    fn http_statuses_parse_from_the_gh_line() {
        assert_eq!(parse_http_status(PRIMARY_403), Some(403));
        assert_eq!(parse_http_status("boom"), None);
        assert_eq!(parse_http_status("HTTP 502 from the proxy"), Some(502));
        assert_eq!(parse_http_status("HTTP/2 502"), None);
    }
}
