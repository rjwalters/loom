//! Durable, schema-versioned signature evidence (#10474, bullet 7 of #10470).
//!
//! Every fetch-and-verify produces ONE [`SignatureEvidence`] record, in BOTH
//! policy modes and for every verdict (verified, refused, failed), so a drift
//! monitor can see what each host adopted -- and notice a host silently
//! running in the `present-only` compatibility mode.
//!
//! # Channel: a host-local JSONL journal (primary)
//!
//! The record is appended to `<repo>/.loom/logs/signature-evidence.jsonl`
//! (override: [`EVIDENCE_JOURNAL_PATH_ENV`]). This is the PRIMARY channel, and
//! deliberately not an OTLP log record, because:
//!
//! * the fetch runs inside short-lived `loom-daemon update` /
//!   `loom-daemon release-fetch` processes, not the long-running daemon that
//!   owns the OTLP exporter -- a log record there could be dropped with the
//!   process, and the evidence would be lost exactly when a roll fails;
//! * a journal is durable across an unreachable collector and a restart, the
//!   same reasoning `sweep-outcomes.jsonl` uses (#4644);
//! * it needs no collector `transform/privacy` allowlist change, so the
//!   contract can land (and be ingested by the 2am inventory side, or a
//!   collector `filelog` receiver) before any OTLP export is designed.
//!
//! The stderr `LOOM_SIGNATURE_EVIDENCE {json}` line is unchanged in WHEN it is
//! printed (required mode, verified artifact only) and only ADDITIVE in what it
//! carries: it is now this same record, a strict superset of the original nine
//! keys with identical meaning.
//!
//! # Never claim what was not established
//!
//! Fields that depend on sibling issues are emitted as an explicit `null` plus
//! a `*_status: "not_available"` marker -- never invented, never omitted --
//! so the schema is stable when they land:
//!
//! | field | filled by |
//! |---|---|
//! | `source_revision` | #10473 (adopted source revision / tag ancestry); filled when the required-mode source gate ran |
//! | `policy_revision` | #10472 (recorded assurance policy) |
//! | `approval_provenance` | #10472 (provider authority map) |
//! | `root_domain_scope` | #10472 (provider authority map) |
//!
//! # Sanitized
//!
//! Only public release facts plus the host's telemetry identity and the build
//! provenance. No token, credential, local filesystem path, or environment
//! value other than the deliberately public signer identity / issuer / workflow
//! pin -- and those pass [`public_text`], which drops anything path- or
//! token-shaped.

use super::fetch::{self, FetchInputs, FetchOutcome, SignaturePolicy};
use super::signature::{SignatureState, VerifiedBy};
use super::source::SourceReport;
use crate::telemetry::provenance::Provenance;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Bumped on any breaking change to [`SignatureEvidence`]'s shape. Adding a
/// field is NOT breaking (consumers must ignore unknown keys).
pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;
/// Prefix of the stderr evidence line (required mode only).
pub const EVIDENCE_LINE_PREFIX: &str = "LOOM_SIGNATURE_EVIDENCE";
/// Env: override the journal path (test seam / relocating the journal).
pub const EVIDENCE_JOURNAL_PATH_ENV: &str = "LOOM_SIGNATURE_EVIDENCE_JOURNAL_PATH";
/// Default filename under `<repo>/.loom/logs/`.
pub const EVIDENCE_JOURNAL_FILENAME: &str = "signature-evidence.jsonl";
/// Rotate (to `<name>.1`) once the journal passes this size. One record is a
/// few hundred bytes and is written once per update attempt.
pub const MAX_JOURNAL_BYTES: u64 = 1024 * 1024;

/// Which signature policy the verdict was reached under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyMode {
    /// `LOOM_DAEMON_UPDATE_REQUIRE_SIGNATURE` on (#10470).
    Required,
    /// The default compatibility mode (#5054).
    PresentOnly,
}

impl From<&SignaturePolicy> for PolicyMode {
    fn from(p: &SignaturePolicy) -> Self {
        if p.require_signature {
            Self::Required
        } else {
            Self::PresentOnly
        }
    }
}

/// What one fetch-and-verify concluded. A closed set; refusals are kept apart
/// so tooling gaps are never read as tampering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceOutcome {
    /// Checksum, signature (or a present-only skip) and GLIBC all passed.
    Verified,
    /// Required mode: no signature published.
    RefusedUnsigned,
    /// Required mode: a signature exists but could not be checked on this
    /// host (no cosign/codesign, underivable identity, no key). NOT tampering.
    RefusedUnavailable,
    /// A signature was checked and did not verify. Tamper evidence.
    SignatureInvalid,
    /// A signature exists but the verifier never answered (timeout /
    /// uncollectable output). Fail-closed abort, but NOT tamper evidence.
    SignatureInconclusive,
    /// The artifact did not match its published `.sha256`. Tamper evidence.
    ChecksumMismatch,
    /// Published signature material would not download (#8197). NOT tampering.
    SignatureMaterialUnavailable,
    /// The binary cannot load on this host's GLIBC (#8837). NOT tampering.
    GlibcIncompatible,
    /// Required mode: the source-revision / tag-movement gate (#10473) refused
    /// the artifact, or its adoption pin could not be written. NOT recorded as
    /// tamper evidence here -- the refusal text distinguishes tag movement from
    /// an unavailable check.
    SourceAssuranceRefused,
    /// The required assets could not be downloaded at all.
    DownloadFailed,
}

impl EvidenceOutcome {
    /// Only a failed checksum or a failed signature check is tamper evidence.
    #[must_use]
    pub fn is_tamper_evidence(self) -> bool {
        matches!(self, Self::SignatureInvalid | Self::ChecksumMismatch)
    }
}

/// Marker for a field this build cannot establish yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldStatus {
    /// The value is `null` because nothing in this build establishes it.
    NotAvailable,
    /// The value was established by a check that ran (#10473).
    Available,
}

/// Facts the verifier records as it goes; the record is built from these, never
/// from policy or inputs, so it cannot claim a check that did not run.
#[derive(Debug, Clone)]
pub struct EvidenceFacts {
    pub outcome: EvidenceOutcome,
    pub asset_sha256: Option<String>,
    pub signature_state: Option<SignatureState>,
    pub verified_by: Option<VerifiedBy>,
    /// What the source-revision / tag-movement gate established (#10473);
    /// `None` when it did not run (present-only mode, earlier failure).
    pub source: Option<SourceReport>,
}

impl Default for EvidenceFacts {
    fn default() -> Self {
        Self {
            outcome: EvidenceOutcome::DownloadFailed,
            asset_sha256: None,
            signature_state: None,
            verified_by: None,
            source: None,
        }
    }
}

/// One evidence record (schema v1). See the module docs and
/// `defaults/docs/daemon-reference.md` for the contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignatureEvidence {
    pub schema_version: u32,
    pub recorded_at: DateTime<Utc>,
    /// The same host identity telemetry uses (`LOOM_HOST_ID` precedence).
    pub host_id: String,
    /// The running (deciding) build's provenance.
    pub loom: Provenance,
    pub policy_mode: PolicyMode,
    pub outcome: EvidenceOutcome,
    pub tamper_evidence: bool,
    // ---- the original nine keys of the v0 stderr line ----
    pub tag: String,
    pub asset_sha256: Option<String>,
    pub signature_state: Option<String>,
    pub verification_method: Option<String>,
    pub identity: Option<String>,
    pub identity_regexp: Option<String>,
    pub oidc_issuer: Option<String>,
    pub configured_workflow: Option<String>,
    pub configured_workflow_applied: bool,
    // ---- blocked on siblings: always null + not_available today ----
    pub source_revision: Option<String>,
    pub source_revision_status: FieldStatus,
    // ---- source gate detail (#10473); null when the gate did not run ----
    #[serde(default)]
    pub source_anchor: Option<String>,
    #[serde(default)]
    pub source_check: Option<String>,
    #[serde(default)]
    pub adoption_record: Option<String>,
    pub policy_revision: Option<String>,
    pub policy_revision_status: FieldStatus,
    pub approval_provenance: Option<String>,
    pub approval_provenance_status: FieldStatus,
    pub root_domain_scope: Option<String>,
    pub root_domain_scope_status: FieldStatus,
}

/// A full 40-hex commit SHA.
fn is_commit_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `Some(s)` only for text safe to publish: non-empty, bounded, no control
/// characters, not a local path (`/…`, `~…`, `\`), not token-shaped.
#[must_use]
pub fn public_text(s: &str) -> Option<String> {
    let t = s.trim();
    let bad = t.is_empty()
        || t.len() > 512
        || t.starts_with('/')
        || t.starts_with('~')
        || t.contains('\\')
        || t.chars().any(char::is_control)
        || is_token_shaped(t)
        || has_dot_segment(t)
        || !url_is_public(t);
    (!bad).then(|| t.to_string())
}

/// [`public_text`] for a signer identity or OIDC issuer: additionally rejects a
/// bare relative filesystem path (a `/` with no URL scheme). Identities are
/// URLs (workflow identities, issuers) or slash-free names (e-mail addresses).
#[must_use]
pub fn public_identity(s: &str) -> Option<String> {
    let t = public_text(s)?;
    (t.contains("://") || !t.contains('/')).then_some(t)
}

/// True when any `/`-separated segment is `.` or `..`.
fn has_dot_segment(t: &str) -> bool {
    t.split('/').any(|seg| seg == ".." || seg == ".")
}

/// For a value with a URL scheme: only `http(s)`, no userinfo, no query or
/// fragment (either can carry a credential). Non-URL text passes.
fn url_is_public(t: &str) -> bool {
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("file:") {
        return false;
    }
    let Some((scheme, rest)) = t.split_once("://") else {
        return true;
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    !authority.contains('@') && !t.contains('?') && !t.contains('#')
}

fn is_token_shaped(t: &str) -> bool {
    const TOKEN_PREFIXES: &[&str] = &[
        "ghp_",
        "gho_",
        "ghs_",
        "ghu_",
        "ghr_",
        "github_pat_",
        "sk-",
        "glpat-",
    ];
    TOKEN_PREFIXES.iter().any(|p| t.contains(p))
}

/// `Some(regexp)` only when the derived keyless identity regexp is safe to
/// publish. It legitimately contains backslash escapes, so [`public_text`]
/// cannot be applied directly; instead the inputs it embeds (workflow pin,
/// tag) must each be public, and the unescaped text must not be token-shaped.
fn public_regexp(regexp: &str, tag: &str, approved_workflow: Option<&str>) -> Option<String> {
    let workflow_ok = approved_workflow
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .is_none_or(|w| public_text(w).is_some());
    let unescaped: String = regexp.chars().filter(|&c| c != '\\').collect();
    let bad = !workflow_ok
        || public_text(tag).is_none()
        || regexp.len() > 512
        || regexp.chars().any(char::is_control)
        || is_token_shaped(&unescaped);
    (!bad).then(|| regexp.to_string())
}

impl SignatureEvidence {
    /// Build the record for this host and build, now.
    #[must_use]
    pub fn from_facts(tag: &str, policy: &SignaturePolicy, facts: &EvidenceFacts) -> Self {
        Self::assemble(
            tag,
            policy,
            facts,
            crate::sweep_registry::host_identity(),
            Provenance::current(),
            Utc::now(),
        )
    }

    /// Pure constructor (identity, provenance and clock injected).
    #[must_use]
    pub fn assemble(
        tag: &str,
        policy: &SignaturePolicy,
        facts: &EvidenceFacts,
        host_id: String,
        loom: Provenance,
        recorded_at: DateTime<Utc>,
    ) -> Self {
        let (method, identity, identity_regexp, issuer, applied) = match &facts.verified_by {
            Some(VerifiedBy::Codesign) => (Some("codesign"), None, None, None, false),
            Some(VerifiedBy::CosignKey) => (Some("cosign-key"), None, None, None, false),
            Some(VerifiedBy::KeylessExactIdentity { identity, issuer }) => (
                Some("cosign-keyless-identity"),
                Some(identity.as_str()),
                None,
                Some(issuer.as_str()),
                false,
            ),
            Some(VerifiedBy::KeylessIdentityRegexp {
                regexp,
                issuer,
                workflow_pinned,
            }) => (
                Some("cosign-keyless-identity-regexp"),
                None,
                Some(regexp.as_str()),
                Some(issuer.as_str()),
                *workflow_pinned,
            ),
            None => (None, None, None, None, false),
        };
        let source_commit = facts
            .source
            .as_ref()
            .and_then(|r| r.source_commit.clone())
            .filter(|c| is_commit_sha(c));
        Self {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            recorded_at,
            host_id: public_text(&host_id).unwrap_or_else(|| "unknown".to_string()),
            loom,
            policy_mode: PolicyMode::from(policy),
            outcome: facts.outcome,
            tamper_evidence: facts.outcome.is_tamper_evidence(),
            tag: public_text(tag).unwrap_or_else(|| "redacted".to_string()),
            asset_sha256: facts
                .asset_sha256
                .clone()
                .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())),
            signature_state: facts.signature_state.map(|s| s.as_str().to_string()),
            verification_method: method.map(str::to_string),
            identity: identity.and_then(public_identity),
            identity_regexp: identity_regexp
                .and_then(|r| public_regexp(r, tag, policy.approved_workflow.as_deref())),
            oidc_issuer: issuer.and_then(public_identity),
            configured_workflow: policy.approved_workflow.as_deref().and_then(public_text),
            configured_workflow_applied: applied,
            source_revision: source_commit.clone(),
            source_revision_status: if source_commit.is_some() {
                FieldStatus::Available
            } else {
                FieldStatus::NotAvailable
            },
            source_anchor: facts
                .source
                .as_ref()
                .and_then(|r| r.source_anchor.clone())
                .filter(|a| is_commit_sha(a)),
            source_check: facts
                .source
                .as_ref()
                .and_then(|r| r.source_check)
                .map(str::to_string),
            adoption_record: facts
                .source
                .as_ref()
                .and_then(|r| r.adoption)
                .map(str::to_string),
            policy_revision: None,
            policy_revision_status: FieldStatus::NotAvailable,
            approval_provenance: None,
            approval_provenance_status: FieldStatus::NotAvailable,
            root_domain_scope: None,
            root_domain_scope_status: FieldStatus::NotAvailable,
        }
    }

    /// The `LOOM_SIGNATURE_EVIDENCE {json}` stderr line.
    #[must_use]
    pub fn stderr_line(&self) -> String {
        let json = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        format!("{EVIDENCE_LINE_PREFIX} {json}")
    }
}

/// Download + verify under `policy`, returning the outcome AND its evidence
/// record. In required mode a verified outcome's `signature_line` also carries
/// the record as the `LOOM_SIGNATURE_EVIDENCE` line.
#[must_use]
pub fn fetch_and_verify_with_evidence(
    inputs: &FetchInputs<'_>,
    policy: &SignaturePolicy,
) -> (FetchOutcome, SignatureEvidence) {
    let mut facts = EvidenceFacts::default();
    let mut outcome = fetch::verify_core(inputs, policy, &mut facts);
    let record = SignatureEvidence::from_facts(inputs.tag, policy, &facts);
    if policy.require_signature {
        if let FetchOutcome::Verified { signature_line, .. } = &mut outcome {
            if !signature_line.is_empty() {
                signature_line.push('\n');
            }
            signature_line.push_str(&record.stderr_line());
        }
    }
    (outcome, record)
}

/// Where the journal lives: the env override, else `<repo>/.loom/logs/…` --
/// but only for a checkout that already has a `.loom/` (never create one in an
/// arbitrary directory).
#[must_use]
pub fn journal_path(repo_root: &Path) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(EVIDENCE_JOURNAL_PATH_ENV) {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let loom = repo_root.join(".loom");
    loom.is_dir()
        .then(|| loom.join("logs").join(EVIDENCE_JOURNAL_FILENAME))
}

/// Append one record (rotating past [`MAX_JOURNAL_BYTES`]).
///
/// # Errors
/// Any I/O error creating, rotating or writing the journal.
pub fn append(path: &Path, record: &SignatureEvidence) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_JOURNAL_BYTES) {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        std::fs::rename(path, PathBuf::from(rotated))?;
    }
    let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")
}

/// Best-effort journal write for the update paths: a journal failure is
/// logged, never allowed to change the update's verdict.
pub fn record(repo_root: &Path, evidence: &SignatureEvidence) -> Option<PathBuf> {
    let path = journal_path(repo_root)?;
    match append(&path, evidence) {
        Ok(()) => Some(path),
        Err(e) => {
            log::warn!("release_fetch: could not append signature evidence: {e}");
            None
        }
    }
}

/// Every parseable record in the journal, oldest first. Unparseable lines are
/// skipped.
#[must_use]
pub fn read_all(path: &Path) -> Vec<SignatureEvidence> {
    std::fs::read_to_string(path)
        .map(|s| {
            s.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}
