//! Source-revision, tag-ancestry, and tag-movement checks (#10473, parent
//! #10470 bullets 4 and 6).
//!
//! A checksum and a signature both answer "are these the bits the release
//! published". Neither answers "was the release cut from a revision this host
//! is willing to run", nor "is this still the release this host adopted last
//! time it saw this tag". This module adds those two questions to the
//! required-assurance mode ([`super::fetch::SignaturePolicy::require_signature`]);
//! the default "present-only" mode never reaches it.
//!
//! # Approved-source anchor
//!
//! `LOOM_DAEMON_UPDATE_APPROVED_SOURCE_ANCHOR` is a full 40-hex commit SHA.
//! The release tag must resolve to that commit or a descendant of it. The tag
//! is resolved with `gh api repos/{slug}/git/ref/tags/{tag}` (an annotated tag
//! object is peeled through `git/tags/{sha}`), and ancestry is asked of
//! `gh api repos/{slug}/compare/{anchor}...{commit}`: only `identical` and
//! `ahead` are accepted. A short or non-hex anchor is a configuration error
//! ([`AnchorSetting::Invalid`]), refused rather than read as "not configured".
//!
//! # Unavailable is not mismatch
//!
//! A `gh` failure, a timeout, or an unparsable response is "source assurance
//! unavailable": refused in required mode, but never worded as tampering. A
//! definite `behind`/`diverged`, a tag that moved, or an asset that was
//! replaced is a mismatch. [`Refusal::class`] keeps the two apart.
//!
//! # Adoption record (tag movement / asset replacement)
//!
//! After an artifact passes every check in required mode, `{tag -> commit,
//! asset sha256}` is written to a local JSON record (default
//! `$HOME/.loom/daemon-update/release-adoption.json`; public release facts
//! only, no secrets). A later fetch of the same tag that resolves to a
//! different commit, or serves a different asset digest, is refused.
//!
//! **Honest limit**: the record lives on the recipient host. It detects drift
//! and ordinary/accidental replacement of a release asset or tag; it does not
//! defend against an attacker with write access to the recipient (who can
//! edit or delete the record) -- that is the deferred approval/promotion
//! boundary of #10470. The commit half of the record is populated only when
//! an anchor is configured, because that is the only time the tag is resolved
//! to a commit; the asset-digest half works in any required-mode fetch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Env: the approved source anchor (a full 40-hex commit SHA).
pub const APPROVED_SOURCE_ANCHOR_ENV: &str = "LOOM_DAEMON_UPDATE_APPROVED_SOURCE_ANCHOR";

/// Depth bound for peeling nested annotated tags (a tag of a tag ...). Real
/// releases have one level; the bound only stops a malicious/looping chain.
const MAX_TAG_PEEL: usize = 4;

/// The configured anchor, parsed. `Invalid` is a configuration error, never a
/// silent skip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AnchorSetting {
    #[default]
    NotConfigured,
    /// Lowercased full 40-hex commit SHA.
    Anchor(String),
    /// Why the configured value was rejected (does not echo the value).
    Invalid(String),
}

impl AnchorSetting {
    #[must_use]
    pub fn anchor(&self) -> Option<&str> {
        match self {
            Self::Anchor(a) => Some(a),
            _ => None,
        }
    }
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse `LOOM_DAEMON_UPDATE_APPROVED_SOURCE_ANCHOR`. Unset/blank is
/// `NotConfigured`; anything other than a full 40-hex SHA is `Invalid`.
#[must_use]
pub fn parse_anchor(raw: Option<&str>) -> AnchorSetting {
    let Some(v) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return AnchorSetting::NotConfigured;
    };
    if is_full_sha(v) {
        AnchorSetting::Anchor(v.to_ascii_lowercase())
    } else {
        AnchorSetting::Invalid(format!(
            "{APPROVED_SOURCE_ANCHOR_ENV} must be a full 40-character hex commit SHA (got {} \
             characters{})",
            v.chars().count(),
            if v.bytes().all(|b| b.is_ascii_hexdigit()) {
                ""
            } else {
                ", including non-hex characters"
            }
        ))
    }
}

/// The default adoption-record path: `$HOME/.loom/daemon-update/release-adoption.json`.
/// `None` under `cfg(test)` so no test can write into a real home directory.
#[must_use]
pub fn default_record_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| {
        PathBuf::from(h)
            .join(".loom")
            .join("daemon-update")
            .join("release-adoption.json")
    })
}

// ---------------------------------------------------------------------------
// Forge queries (pure parsers + a thin `gh api` runner)
// ---------------------------------------------------------------------------

/// A `gh api <path>` runner: `Ok(stdout)` on success, `Err(reason)` when no
/// answer was obtained. The reason is a fixed category, never raw stderr.
pub type ApiFn<'a> = dyn Fn(&str) -> Result<String, String> + 'a;

/// The real runner, through the counted/bounded `gh` facade.
pub fn gh_api(repo_root: &Path, path: &str) -> Result<String, String> {
    use crate::cmd_out::CmdOutcome;
    use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    let outcome = GhInvocation::new(
        Operation::new("api.rest"),
        AccessIntent::Read,
        GhTarget::None,
        std::time::Duration::from_secs(60),
    )
    .forge_op(crate::forge_call_stats::ops::RELEASE_RESOLVE_AND_FETCH)
    .current_dir(repo_root)
    .args(["api", path])
    .run();
    match outcome {
        CmdOutcome::Ran(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into()),
        CmdOutcome::Ran(o) => Err(format!(
            "`gh api` exited {}",
            o.status
                .code()
                .map_or_else(|| "by signal".to_string(), |c| c.to_string())
        )),
        CmdOutcome::Unavailable(_) => Err("`gh api` could not be run or timed out".to_string()),
    }
}

/// `(object.type, object.sha)` from a `git/ref/tags/*` or `git/tags/*` body.
#[must_use]
pub fn parse_ref_object(body: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let obj = v.get("object")?;
    let kind = obj.get("type")?.as_str()?.to_string();
    let sha = obj.get("sha")?.as_str()?.to_ascii_lowercase();
    is_full_sha(&sha).then_some((kind, sha))
}

/// A tag name safe to splice into an API path (no traversal, no query).
fn tag_is_path_safe(tag: &str) -> bool {
    !tag.is_empty()
        && !tag.contains("..")
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+' | b'/'))
}

/// Resolve `tag` to the commit it names, peeling annotated tags. `Err` is
/// always "unavailable" (no answer), never a mismatch.
pub fn resolve_tag_commit(api: &ApiFn<'_>, slug: &str, tag: &str) -> Result<String, String> {
    if !tag_is_path_safe(tag) {
        return Err(format!("tag {tag:?} is not a plain ref name"));
    }
    let body = api(&format!("repos/{slug}/git/ref/tags/{tag}"))
        .map_err(|e| format!("resolving tag {tag}: {e}"))?;
    let (mut kind, mut sha) = parse_ref_object(&body)
        .ok_or_else(|| format!("resolving tag {tag}: unparsable ref response"))?;
    for _ in 0..MAX_TAG_PEEL {
        match kind.as_str() {
            "commit" => return Ok(sha),
            "tag" => {
                let body = api(&format!("repos/{slug}/git/tags/{sha}"))
                    .map_err(|e| format!("peeling annotated tag {tag}: {e}"))?;
                (kind, sha) = parse_ref_object(&body).ok_or_else(|| {
                    format!("peeling annotated tag {tag}: unparsable tag-object response")
                })?;
            }
            other => {
                return Err(format!("tag {tag} points at a {other:?} object, not a commit"));
            }
        }
    }
    if kind == "commit" {
        Ok(sha)
    } else {
        Err(format!("tag {tag}: annotated-tag chain deeper than {MAX_TAG_PEEL}"))
    }
}

/// The answer to "is `commit` the anchor or a descendant of it".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ancestry {
    Identical,
    Ahead,
    Behind,
    Diverged,
    /// No answer (failed call, unparsable or unknown status).
    Unavailable(String),
}

impl Ancestry {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Identical => "identical",
            Self::Ahead => "ahead",
            Self::Behind => "behind",
            Self::Diverged => "diverged",
            Self::Unavailable(_) => "unavailable",
        }
    }
}

/// Classify a `compare/{anchor}...{commit}` body by its `status` field.
#[must_use]
pub fn classify_compare(body: &str) -> Ancestry {
    let status = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string));
    match status.as_deref() {
        Some("identical") => Ancestry::Identical,
        Some("ahead") => Ancestry::Ahead,
        Some("behind") => Ancestry::Behind,
        Some("diverged") => Ancestry::Diverged,
        Some(_) => Ancestry::Unavailable("unrecognized compare status".to_string()),
        None => Ancestry::Unavailable("unparsable compare response".to_string()),
    }
}

pub fn compare(api: &ApiFn<'_>, slug: &str, anchor: &str, commit: &str) -> Ancestry {
    match api(&format!("repos/{slug}/compare/{anchor}...{commit}")) {
        Ok(body) => classify_compare(&body),
        Err(e) => Ancestry::Unavailable(format!("comparing against the anchor: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Adoption record
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdoptionEntry {
    pub slug: String,
    pub tag: String,
    pub target: String,
    /// `None` when the tag was never resolved (no anchor configured).
    pub commit: Option<String>,
    pub asset_sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdoptionRecord {
    pub schema: u32,
    pub entries: BTreeMap<String, AdoptionEntry>,
}

fn record_key(slug: &str, tag: &str, target: &str) -> String {
    format!("{slug} {tag} {target}")
}

/// Read the record. A missing file is an empty record; an unreadable or
/// corrupt one is `Err` (refused as unavailable, never read as "first seen").
pub fn read_record(path: &Path) -> Result<AdoptionRecord, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|_| {
            format!("adoption record {} is not valid JSON; inspect or remove it", path.display())
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AdoptionRecord::default()),
        Err(e) => Err(format!("adoption record {} could not be read: {e}", path.display())),
    }
}

/// Upsert one entry, atomically (temp file + rename). A commit already on
/// record is kept when this fetch did not resolve one.
pub fn write_entry(path: &Path, mut entry: AdoptionEntry) -> Result<(), String> {
    let mut record = read_record(path)?;
    record.schema = 1;
    let key = record_key(&entry.slug, &entry.tag, &entry.target);
    if entry.commit.is_none() {
        entry.commit = record.entries.get(&key).and_then(|e| e.commit.clone());
    }
    record.entries.insert(key, entry);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let text = serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// Why the source gate refused. `Unavailable` and `ConfigError` are never
/// worded as tampering; the other three are definite mismatches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalClass {
    ConfigError,
    Unavailable,
    NotDescendant,
    TagMoved,
    AssetReplaced,
    /// The adoption pin could not be persisted, so drift protection for the
    /// next fetch would be silently lost.
    RecordWriteFailed,
}

#[derive(Debug, Clone)]
pub struct Refusal {
    pub class: RefusalClass,
    /// `err()`-worded lines, WITHOUT the shared abort line.
    pub lines: Vec<String>,
}

/// What the source gate established, for the evidence line. Only checks that
/// actually ran populate a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceReport {
    pub source_commit: Option<String>,
    pub source_anchor: Option<String>,
    /// `not_configured` | `identical` | `ahead`.
    pub source_check: &'static str,
    /// `not_recorded` (no record path) | `first_seen` | `matched`.
    pub adoption: &'static str,
}

impl SourceReport {
    /// Add this report's fields to an evidence JSON object.
    pub fn extend_evidence(&self, obj: &mut serde_json::Map<String, serde_json::Value>) {
        obj.insert("source_commit".into(), self.source_commit.clone().into());
        obj.insert("source_anchor".into(), self.source_anchor.clone().into());
        obj.insert("source_check".into(), self.source_check.into());
        obj.insert("adoption_record".into(), self.adoption.into());
    }
}

pub struct GateInputs<'a> {
    pub slug: &'a str,
    pub tag: &'a str,
    pub target: &'a str,
    pub asset_sha256: &'a str,
    pub anchor: &'a AnchorSetting,
    pub record_path: Option<&'a Path>,
}

fn unavailable(why: &str) -> Refusal {
    Refusal {
        class: RefusalClass::Unavailable,
        lines: vec![
            format!(
                "Required source assurance is on: source assurance unavailable -- {why}. This is \
                 NOT evidence of tampering; the release's source could not be established."
            ),
            "Refusing the artifact before it is executed or provisioned.".to_string(),
        ],
    }
}

fn mismatch(class: RefusalClass, head: String) -> Refusal {
    Refusal {
        class,
        lines: vec![
            head,
            "Refusing the artifact before it is executed or provisioned. If this change is \
             intended, update the policy explicitly (anchor / adoption record) -- it is never \
             accepted implicitly."
                .to_string(),
        ],
    }
}

/// Run the source gate. Nothing here executes the candidate.
pub fn gate(api: &ApiFn<'_>, i: &GateInputs<'_>) -> Result<SourceReport, Refusal> {
    let anchor = match i.anchor {
        AnchorSetting::Invalid(why) => {
            return Err(Refusal {
                class: RefusalClass::ConfigError,
                lines: vec![
                    format!("Required source assurance: configuration error -- {why}."),
                    "Refusing rather than skipping the source check; fix or unset the variable."
                        .to_string(),
                ],
            })
        }
        AnchorSetting::Anchor(a) => Some(a.as_str()),
        AnchorSetting::NotConfigured => None,
    };

    let commit = match anchor {
        Some(_) => Some(resolve_tag_commit(api, i.slug, i.tag).map_err(|e| unavailable(&e))?),
        None => None,
    };

    let mut adoption = "not_recorded";
    if let Some(path) = i.record_path {
        let record = read_record(path).map_err(|e| unavailable(&e))?;
        match record.entries.get(&record_key(i.slug, i.tag, i.target)) {
            None => adoption = "first_seen",
            Some(prev) => {
                if let (Some(was), Some(now)) = (prev.commit.as_deref(), commit.as_deref()) {
                    if was != now {
                        return Err(mismatch(
                            RefusalClass::TagMoved,
                            format!(
                                "Tag movement detected: release {} now resolves to commit {now}, \
                                 but this host adopted it at commit {was} (adoption record {}).",
                                i.tag,
                                path.display()
                            ),
                        ));
                    }
                }
                if prev.asset_sha256 != i.asset_sha256 {
                    return Err(mismatch(
                        RefusalClass::AssetReplaced,
                        format!(
                            "Asset replacement detected: loom-daemon-{} for release {} now has \
                             sha256 {}, but this host adopted sha256 {} (adoption record {}).",
                            i.target,
                            i.tag,
                            i.asset_sha256,
                            prev.asset_sha256,
                            path.display()
                        ),
                    ));
                }
                adoption = "matched";
            }
        }
    }

    let source_check = match (anchor, commit.as_deref()) {
        (Some(a), Some(c)) => match compare(api, i.slug, a, c) {
            Ancestry::Identical => "identical",
            Ancestry::Ahead => "ahead",
            st @ (Ancestry::Behind | Ancestry::Diverged) => {
                return Err(mismatch(
                    RefusalClass::NotDescendant,
                    format!(
                        "Source revision check FAILED: release {} (commit {c}) is not a \
                         descendant of approved source {a} (compare status: {}).",
                        i.tag,
                        st.as_str()
                    ),
                ))
            }
            Ancestry::Unavailable(e) => return Err(unavailable(&e)),
        },
        _ => "not_configured",
    };

    Ok(SourceReport {
        source_commit: commit,
        source_anchor: anchor.map(str::to_string),
        source_check,
        adoption,
    })
}

/// Persist the adoption pin after every check passed, BEFORE the candidate is
/// executed. A write failure is a refusal: adopting without a pin would make a
/// later replaced tag/asset look `first_seen` and be accepted.
pub fn record_adoption(report: &SourceReport, i: &GateInputs<'_>) -> Result<(), Refusal> {
    let Some(path) = i.record_path else {
        return Ok(());
    };
    let entry = AdoptionEntry {
        slug: i.slug.to_string(),
        tag: i.tag.to_string(),
        target: i.target.to_string(),
        commit: report.source_commit.clone(),
        asset_sha256: i.asset_sha256.to_string(),
    };
    write_entry(path, entry).map_err(|e| Refusal {
        class: RefusalClass::RecordWriteFailed,
        lines: vec![
            format!(
                "Required source assurance is on: could not write the release adoption record \
                 {} ({e}); without it a later tag or asset replacement could not be detected. \
                 This is NOT evidence of tampering.",
                path.display()
            ),
            "Refusing the artifact before it is executed or provisioned.".to_string(),
        ],
    })
}

#[cfg(test)]
mod tests;
