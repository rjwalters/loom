//! The `# LOOM_LAUNCH` record a native-harness spawn writes to its own log,
//! read back as **secret-free credential attribution** (issue #8447).
//!
//! `worker_spawn::run` writes one `# LOOM_LAUNCH {json}` line per native
//! harness launch, carrying `credentialSource` / `credentialProvider` /
//! `credentialAccount` (issue #8401, PR #8428). Those three values are the
//! only per-account attribution a pool-sourced spawn ever emits, and until
//! this module existed they lived **only** in retained log prose: a reader
//! asking "which API-key account did this sweep burn, and which account was
//! holding the bag when it died?" had to grep logs that rotate.
//!
//! # Why a parser rather than a dispatch-time capture
//!
//! The launch record is written by the *child*, after the daemon has already
//! spawned it — the same race `sweep_registry::resolve_token_account`
//! documents for the Claude OAuth pool, where a 5s dispatch-time poll can lose
//! to the harness's own log write. Re-reading the log once at the terminal
//! transition (a rare, per-sweep event) is what makes the attribution durable
//! in both directions: it survives a daemon restart that dropped the in-memory
//! entry, and it survives a selection logged after the capture window closed.
//!
//! # Secret-free by construction
//!
//! [`parse_launch_credential`] reads exactly three keys off the record and
//! copies nothing else. The launch record itself never carries key material
//! (`credentialAccount` is an account NAME — see
//! `worker_spawn::credential::Resolved`, whose `Debug` redacts and which has
//! no `Serialize` at all), so there is nothing to redact here; the narrow
//! key list is what keeps that true if the record ever grows a field.

use serde::{Deserialize, Serialize};

/// The marker `worker_spawn::run` prefixes the launch record's JSON with.
pub const LAUNCH_RECORD_MARKER: &str = "# LOOM_LAUNCH ";

/// Where a native-harness spawn's credential came from, and (when the pool
/// decided) which account — never the key itself.
///
/// Mirrors `worker_spawn::credential::Resolved`'s reportable fields. Stored on
/// [`crate::sweep_outcomes::OutcomeRecord`] so per-account usage and
/// per-account failure attribution are reconstructable from the journal alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialAttribution {
    /// `"pool"`, `"env"` or `"none"` — `worker_spawn::credential::Source`.
    pub source: String,
    /// The API-key pool's provider namespace (`zai`, …). `None` for an
    /// env-sourced or unpooled spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The selected account's **name**. `None` for an env-sourced or unpooled
    /// spawn — only a pool selection has an account to name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// Parse one `# LOOM_LAUNCH {…}` record body (the JSON after the marker).
///
/// `None` when the body is not an object or carries no non-empty
/// `credentialSource` — a record from a pre-#8401 binary has no credential
/// fields at all, and recording `source: ""` for it would be a fabricated
/// reading of silence.
#[must_use]
pub fn parse_launch_credential(record_json: &str) -> Option<CredentialAttribution> {
    let value: serde_json::Value = serde_json::from_str(record_json.trim()).ok()?;
    // Exactly three keys are read; nothing else on the record is copied.
    let string = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let source = string("credentialSource")?;
    Some(CredentialAttribution {
        source,
        provider: string("credentialProvider"),
        account: string("credentialAccount"),
    })
}

/// Parse the credential attribution out of a per-sweep log's `contents`,
/// scanning only the region at/after this dispatch's header (`header_anchor`,
/// e.g. `sweep_id=<id>`).
///
/// Anchoring mirrors [`crate::sweep_registry::crash_signals`]'s
/// `parse_token_name_after`: a per-issue log is reused across dispatches, so a
/// previous run's launch record must never be attributed to this one. Within
/// the region the **last** record wins — a containment re-exec writes its own
/// launch record inside the container, and the last one written is the one the
/// harness actually ran on.
#[must_use]
pub fn parse_launch_credential_after(
    contents: &str,
    header_anchor: &str,
) -> Option<CredentialAttribution> {
    let region = &contents[contents.rfind(header_anchor)?..];
    let body_start = region.rfind(LAUNCH_RECORD_MARKER)? + LAUNCH_RECORD_MARKER.len();
    parse_launch_credential(region[body_start..].lines().next()?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    /// The launch record `worker_spawn::run` writes, verbatim in shape.
    fn launch_line(source: &str, provider: &str, account: &str) -> String {
        format!(
            "# LOOM_LAUNCH {}",
            serde_json::json!({
                "schema": 1,
                "runtime": "pi",
                "provider": "zai-coding-plan",
                "model": "glm-5.3",
                "profile": "zai-flash",
                "effort": serde_json::Value::Null,
                "credentialSource": source,
                "credentialProvider": if provider.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(provider.to_string())
                },
                "credentialAccount": if account.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(account.to_string())
                },
                "usage": "native-json-events",
                "billing": "not-measured",
            })
        )
    }

    fn log_with(anchor: &str, line: &str) -> String {
        format!("==== loom-daemon dispatch: {anchor} issue=42 ====\nsome prose\n{line}\nmore\n")
    }

    #[test]
    fn a_pool_sourced_launch_yields_provider_and_account() {
        let log = log_with("sweep_id=s1", &launch_line("pool", "zai", "alpha"));
        let attribution = parse_launch_credential_after(&log, "sweep_id=s1").unwrap();
        assert_eq!(attribution.source, "pool");
        assert_eq!(attribution.provider.as_deref(), Some("zai"));
        assert_eq!(attribution.account.as_deref(), Some("alpha"));
    }

    #[test]
    fn an_env_sourced_launch_records_its_source_with_no_account() {
        let log = log_with("sweep_id=s1", &launch_line("env", "", ""));
        let attribution = parse_launch_credential_after(&log, "sweep_id=s1").unwrap();
        assert_eq!(attribution.source, "env");
        assert_eq!(attribution.provider, None);
        assert_eq!(attribution.account, None);
    }

    #[test]
    fn an_unpooled_launch_records_source_none_with_no_account() {
        let log = log_with("sweep_id=s1", &launch_line("none", "", ""));
        let attribution = parse_launch_credential_after(&log, "sweep_id=s1").unwrap();
        assert_eq!(attribution.source, "none");
        assert_eq!(attribution.account, None);
    }

    #[test]
    fn a_log_without_a_launch_record_yields_nothing() {
        let log = "==== loom-daemon dispatch: sweep_id=s1 issue=42 ====\nno launch record\n";
        assert_eq!(parse_launch_credential_after(log, "sweep_id=s1"), None);
        // A missing anchor is "not this dispatch", never a fallback scan.
        let log = log_with("sweep_id=other", &launch_line("pool", "zai", "alpha"));
        assert_eq!(parse_launch_credential_after(&log, "sweep_id=s1"), None);
    }

    #[test]
    fn a_previous_dispatchs_record_in_a_reused_log_is_not_attributed_to_this_one() {
        let log = format!(
            "{}{}",
            log_with("sweep_id=old", &launch_line("pool", "zai", "stale-account")),
            log_with("sweep_id=new", &launch_line("pool", "zai", "fresh-account")),
        );
        let attribution = parse_launch_credential_after(&log, "sweep_id=new").unwrap();
        assert_eq!(attribution.account.as_deref(), Some("fresh-account"));
    }

    #[test]
    fn the_last_record_in_the_region_wins_over_an_earlier_re_exec() {
        let log = format!(
            "==== loom-daemon dispatch: sweep_id=s1 issue=42 ====\n{}\n{}\n",
            launch_line("pool", "zai", "first"),
            launch_line("pool", "zai", "second"),
        );
        let attribution = parse_launch_credential_after(&log, "sweep_id=s1").unwrap();
        assert_eq!(attribution.account.as_deref(), Some("second"));
    }

    #[test]
    fn a_pre_8401_record_without_credential_fields_yields_nothing() {
        let log = log_with(
            "sweep_id=s1",
            r#"# LOOM_LAUNCH {"schema":1,"runtime":"pi","model":"glm-5.3"}"#,
        );
        assert_eq!(parse_launch_credential_after(&log, "sweep_id=s1"), None);
        // Neither does an empty source string — "" is not a reading of silence.
        let log = log_with("sweep_id=s1", &launch_line("", "zai", "alpha"));
        assert_eq!(parse_launch_credential_after(&log, "sweep_id=s1"), None);
    }

    #[test]
    fn a_malformed_record_is_ignored_rather_than_aborting_the_read() {
        let log = log_with("sweep_id=s1", "# LOOM_LAUNCH {not json");
        assert_eq!(parse_launch_credential_after(&log, "sweep_id=s1"), None);
    }

    /// The whole point of reading only three named keys: whatever else the
    /// record (or the surrounding log) carries never reaches the attribution.
    #[test]
    fn nothing_but_the_three_named_keys_is_copied_off_the_record() {
        let line = format!(
            "# LOOM_LAUNCH {}",
            serde_json::json!({
                "credentialSource": "pool",
                "credentialProvider": "zai",
                "credentialAccount": "alpha",
                "credentialValue": "sk-fake-secret-material",
            })
        );
        let log = log_with("sweep_id=s1", &line);
        let attribution = parse_launch_credential_after(&log, "sweep_id=s1").unwrap();
        let rendered = serde_json::to_string(&attribution).unwrap();
        assert!(!rendered.contains("sk-fake-secret-material"), "{rendered}");
        assert_eq!(rendered, r#"{"source":"pool","provider":"zai","account":"alpha"}"#);
    }
}
