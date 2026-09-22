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

/// Where a native-harness spawn actually ran (Issue #8507), independent of
/// credentials: the runtime adapter, its resolved provider namespace, and the
/// resolved model profile — read off the SAME `# LOOM_LAUNCH` record
/// [`parse_launch_credential`] reads, so a `runtime`/`provider`/`profile`
/// consumer can never disagree with a `credentialSource`/`credentialProvider`
/// consumer about which launch they are describing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAttribution {
    /// The runtime adapter (`"pi"`, `"opencode"`, …). Always present when the
    /// record parses at all — `worker_spawn::run` sets this key on every
    /// `# LOOM_LAUNCH` line it writes, unconditionally.
    pub runtime: String,
    /// The runtime's resolved provider namespace (`"zai-coding-plan"`,
    /// `"friendli"`, …), when the launch resolved one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The resolved model profile name, when one was selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// Parse one `# LOOM_LAUNCH {…}` record body's runtime attribution (Issue
/// #8507) — the [`RuntimeAttribution`] counterpart of
/// [`parse_launch_credential`].
///
/// `None` when the body is not an object or carries no non-empty `runtime` —
/// a record from a binary that predates this key (there is none today; kept
/// for the same "never fabricate a reading of silence" contract
/// [`parse_launch_credential`] documents).
#[must_use]
pub fn parse_launch_runtime(record_json: &str) -> Option<RuntimeAttribution> {
    let value: serde_json::Value = serde_json::from_str(record_json.trim()).ok()?;
    let string = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let runtime = string("runtime")?;
    Some(RuntimeAttribution {
        runtime,
        provider: string("provider"),
        profile: string("profile"),
    })
}

/// [`parse_launch_runtime`] scoped to the region at/after `header_anchor`,
/// taking the LAST record in that region — the anchored counterpart of
/// [`parse_launch_credential_after`], for a caller (a sweep) that has a
/// `sweep_id=`-shaped anchor to scope by.
#[must_use]
pub fn parse_launch_runtime_after(
    contents: &str,
    header_anchor: &str,
) -> Option<RuntimeAttribution> {
    let region = &contents[contents.rfind(header_anchor)?..];
    let body_start = region.rfind(LAUNCH_RECORD_MARKER)? + LAUNCH_RECORD_MARKER.len();
    parse_launch_runtime(region[body_start..].lines().next()?)
}

/// [`parse_launch_runtime`] over the WHOLE of `contents`, taking the last
/// `# LOOM_LAUNCH` record in the file with no anchor scoping at all (Issue
/// #8507).
///
/// Only safe where the caller's own concurrency contract already guarantees
/// at most one live writer to this log at a time — a role tick's per-role log
/// has no `sweep_id=`-shaped anchor the way a sweep's does (a role tick has no
/// per-invocation id at all), but `role_runner`'s per-`(root, role)` run guard
/// makes two concurrent ticks of the same role impossible, so "the last record
/// in the file, read right after this tick's child exited" cannot belong to
/// any tick but this one. A caller without that guarantee must use
/// [`parse_launch_runtime_after`] instead.
#[must_use]
pub fn last_launch_runtime(contents: &str) -> Option<RuntimeAttribution> {
    let body_start = contents.rfind(LAUNCH_RECORD_MARKER)? + LAUNCH_RECORD_MARKER.len();
    parse_launch_runtime(contents[body_start..].lines().next()?)
}

/// A sweep's per-issue log path, derived from the workspace root alone
/// (Issue #8507).
///
/// `SweepRegistry::compute_log_path` delegates here, and safehouse's
/// completion narration — which holds a workspace root and an issue number but
/// no registry handle — calls it directly, so the two can never disagree about
/// where a sweep's log (and therefore its `# LOOM_LAUNCH` record) lives.
#[must_use]
pub fn sweep_log_path(workspace_root: &std::path::Path, issue: u32) -> std::path::PathBuf {
    workspace_root
        .join(".loom")
        .join("logs")
        .join(format!("sweep-issue-{issue}.log"))
}

/// The runtime attribution of the most recent launch recorded in issue
/// `issue`'s own sweep log under `workspace_root` (Issue #8507).
///
/// Unanchored, unlike [`parse_launch_runtime_after`]: a caller here has no
/// `sweep_id` to scope by. The log is per-issue, so the last record in it is
/// the most recent launch **for that issue** — which is the launch whose work
/// a completion narrated now is reporting. `None` for a Claude/legacy-adapter
/// spawn (writes no launch record at all), a missing or rotated log, or a
/// record with no usable `runtime`.
#[must_use]
pub fn sweep_runtime_attribution(
    workspace_root: &std::path::Path,
    issue: u32,
) -> Option<RuntimeAttribution> {
    let contents = std::fs::read_to_string(sweep_log_path(workspace_root, issue)).ok()?;
    last_launch_runtime(&contents)
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

    // --- RuntimeAttribution (Issue #8507) -------------------------------

    #[test]
    fn parse_launch_runtime_after_reads_runtime_provider_and_profile() {
        let log = log_with("sweep_id=s1", &launch_line("pool", "zai", "alpha"));
        let attribution = parse_launch_runtime_after(&log, "sweep_id=s1").unwrap();
        assert_eq!(attribution.runtime, "pi");
        assert_eq!(attribution.provider.as_deref(), Some("zai-coding-plan"));
        assert_eq!(attribution.profile.as_deref(), Some("zai-flash"));
    }

    #[test]
    fn parse_launch_runtime_after_omits_absent_provider_and_profile() {
        let line =
            format!("# LOOM_LAUNCH {}", serde_json::json!({"schema": 1, "runtime": "opencode"}));
        let log = log_with("sweep_id=s1", &line);
        let attribution = parse_launch_runtime_after(&log, "sweep_id=s1").unwrap();
        assert_eq!(attribution.runtime, "opencode");
        assert_eq!(attribution.provider, None);
        assert_eq!(attribution.profile, None);
    }

    #[test]
    fn parse_launch_runtime_after_respects_the_same_anchor_and_last_record_rules() {
        // A missing anchor: not this dispatch.
        let log = log_with("sweep_id=other", &launch_line("pool", "zai", "alpha"));
        assert_eq!(parse_launch_runtime_after(&log, "sweep_id=s1"), None);

        // A previous dispatch's record in a reused log is not attributed here.
        let reused = format!(
            "{}{}",
            log_with(
                "sweep_id=old",
                &format!(
                    "# LOOM_LAUNCH {}",
                    serde_json::json!({"schema": 1, "runtime": "pi", "profile": "stale"})
                )
            ),
            log_with(
                "sweep_id=new",
                &format!(
                    "# LOOM_LAUNCH {}",
                    serde_json::json!({"schema": 1, "runtime": "opencode", "profile": "fresh"})
                )
            ),
        );
        let attribution = parse_launch_runtime_after(&reused, "sweep_id=new").unwrap();
        assert_eq!(attribution.runtime, "opencode");
        assert_eq!(attribution.profile.as_deref(), Some("fresh"));
    }

    #[test]
    fn parse_launch_runtime_yields_nothing_for_a_missing_or_empty_runtime() {
        assert_eq!(parse_launch_runtime(r#"{"provider":"zai"}"#), None);
        assert_eq!(parse_launch_runtime(r#"{"runtime":""}"#), None);
        assert_eq!(parse_launch_runtime("{not json"), None);
    }

    #[test]
    fn last_launch_runtime_scans_the_whole_log_with_no_anchor() {
        let log = format!(
            "some earlier prose\n# LOOM_LAUNCH {}\nmore prose\n# LOOM_LAUNCH {}\ntail\n",
            serde_json::json!({"schema": 1, "runtime": "pi", "profile": "first"}),
            serde_json::json!({"schema": 1, "runtime": "opencode", "profile": "second"}),
        );
        let attribution = last_launch_runtime(&log).unwrap();
        assert_eq!(attribution.runtime, "opencode");
        assert_eq!(attribution.profile.as_deref(), Some("second"));

        assert_eq!(last_launch_runtime("no launch record here"), None);
    }

    #[test]
    fn sweep_runtime_attribution_reads_the_issues_own_sweep_log() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let issue = 8507;
        let path = sweep_log_path(root, issue);
        assert!(path.ends_with(".loom/logs/sweep-issue-8507.log"), "{}", path.display());

        // No log at all (and no launch record in one) ⇒ no attribution, never
        // a fabricated default.
        assert_eq!(sweep_runtime_attribution(root, issue), None);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "ordinary sweep prose\n").unwrap();
        assert_eq!(sweep_runtime_attribution(root, issue), None);

        std::fs::write(
            &path,
            format!(
                "==== dispatch ====\n# LOOM_LAUNCH {}\nprose\n",
                serde_json::json!({
                    "schema": 1,
                    "runtime": "opencode",
                    "provider": "friendli",
                    "profile": "glm-coding",
                }),
            ),
        )
        .unwrap();
        let attribution = sweep_runtime_attribution(root, issue).unwrap();
        assert_eq!(attribution.runtime, "opencode");
        assert_eq!(attribution.provider.as_deref(), Some("friendli"));
        assert_eq!(attribution.profile.as_deref(), Some("glm-coding"));

        // A DIFFERENT issue's log is never consulted.
        assert_eq!(sweep_runtime_attribution(root, 8508), None);
    }
}
