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

use crate::runtime_preference::Tap;

/// The marker `worker_spawn::run` prefixes the launch record's JSON with.
///
/// A re-export, not a second definition (issue #8541): `api_keys_pool::ingest`
/// already owns the canonical constant (as `LAUNCH_RECORD_PREFIX`) and the
/// tested line-anchored parser that goes with it. Aliasing here means the two
/// modules can never drift onto different marker strings.
pub use crate::api_keys_pool::ingest::LAUNCH_RECORD_PREFIX as LAUNCH_RECORD_MARKER;

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
///
/// Composes [`crate::api_keys_pool::ingest::region_after`] and
/// [`crate::api_keys_pool::ingest::parse_launch_record`] rather than
/// re-deriving the anchor/marker scan (issue #8541): those helpers scan lines
/// in reverse and require each candidate to *start with* the marker after
/// trim, falling back to an earlier candidate line rather than giving up — so
/// a log line that merely *mentions* `# LOOM_LAUNCH ` mid-line (e.g. an agent
/// transcript echoing it) can never suppress attribution of the real,
/// earlier, line-start record the way an unanchored substring search could.
#[must_use]
pub fn parse_launch_credential_after(
    contents: &str,
    header_anchor: &str,
) -> Option<CredentialAttribution> {
    let region = crate::api_keys_pool::ingest::region_after(contents, header_anchor)?;
    let record = crate::api_keys_pool::ingest::parse_launch_record(region)?;
    // Mirrors parse_launch_credential's "no reading of silence" contract: an
    // empty/missing credentialSource yields None, not a fabricated source.
    if record.credential_source.is_empty() {
        return None;
    }
    Some(CredentialAttribution {
        source: record.credential_source,
        provider: record.provider,
        account: record.account,
    })
}

// ============================================================================
// Tap attribution (Issue #8556)
// ============================================================================

/// Wire name of the credential half of a tap key when the launch record says
/// the key came from an operator-exported environment variable rather than any
/// Loom-managed pool. Its own bucket on purpose: an `env` key is real spend on
/// a credential Loom never selected and cannot govern.
pub const CREDENTIAL_WIRE_ENV: &str = "env";

/// Wire name for a launch that resolved no Loom-visible credential at all and
/// fell through to the harness's own auth store — spend Loom cannot attribute
/// to an account it knows, and must not silently fold into a pool's bucket.
pub const CREDENTIAL_WIRE_HARNESS_OWN: &str = "harness-own";

/// The **accounting identity** of one launch: the tap it ran on.
///
/// The operator's framing (2026-09-20, #8436) is that the unit whose economics
/// differ is the **tap** — `(runtime, credential source)` — not the runtime and
/// not the model. Issue #8556 is the consequence for accounting: a metered
/// OpenAI-compatible key is one credential shared across every fleet host, so
/// "how much went to the metered backstop vs. the subscriptions" has to be a
/// *query* over a key that names the credential, not a reconstruction from
/// runtime names and a mental model of which profile binds which provider.
///
/// Two axes, deliberately kept separate and both rendered into [`Self::key`]:
///
/// * the **configured** identity — [`Tap`] (`runtime`, optional model profile);
///   the profile is what binds *which* provider and credential source, so
///   `opencode` alone cannot distinguish a flat-rate coding plan from a metered
///   endpoint reached through the same runtime;
/// * the **realized** credential — what `worker_spawn::credential` actually
///   resolved, as the launch record's `credentialSource` / `credentialProvider`
///   / `credentialAccount` report it (names only, never key material).
///
/// Secret-free by construction, exactly like [`CredentialAttribution`], which
/// it embeds rather than re-derives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapAttribution {
    /// The runtime id (`claude`, `pi`, `opencode`, …).
    pub runtime: String,
    /// The model profile that bound this launch's provider + credential
    /// source, when one did. `None` ⇒ the runtime's own default resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_profile: Option<String>,
    /// Where the credential came from, and which account — never the key.
    pub credential: CredentialAttribution,
}

impl TapAttribution {
    /// The configured half as the resolver's own [`Tap`], so a launch record
    /// and a `# LOOM_RUNTIME_PREFERENCE` marker render the same identity.
    #[must_use]
    pub fn tap(&self) -> Tap {
        match &self.model_profile {
            Some(profile) => Tap::with_profile(&self.runtime, profile),
            None => Tap::runtime(&self.runtime),
        }
    }

    /// The credential half's stable wire name, matching
    /// [`crate::runtime_preference::CredentialSource::wire`] where the two
    /// overlap (`api_keys:<provider>`) so one grep finds a credential source
    /// across the preference marker, the launch record and this key.
    ///
    /// The two remaining launch-record sources have no `CredentialSource`
    /// counterpart because the resolver never selects them:
    /// [`CREDENTIAL_WIRE_ENV`] (an operator-exported key) and
    /// [`CREDENTIAL_WIRE_HARNESS_OWN`] (the harness's own auth store). Both get
    /// their own bucket rather than being folded into a pool's — spend on a
    /// credential Loom did not select is exactly the spend a fleet ceiling
    /// cannot govern, so it must stay visibly separate.
    #[must_use]
    pub fn credential_wire(&self) -> String {
        match (self.credential.source.as_str(), &self.credential.provider) {
            ("pool", Some(provider)) => format!("api_keys:{provider}"),
            ("pool", None) => "api_keys".to_string(),
            ("env", _) => CREDENTIAL_WIRE_ENV.to_string(),
            ("none", _) => CREDENTIAL_WIRE_HARNESS_OWN.to_string(),
            (other, _) => other.to_string(),
        }
    }

    /// The accounting key: `<tap>@<credential wire>`, e.g. `claude@env`,
    /// `opencode:zai-metered@api_keys:zai`, `pi@harness-own`.
    ///
    /// One string, so folding usage by tap is a `BTreeMap` insert and an
    /// operator grep is one token — and so the *account* stays out of the key.
    /// Per-account attribution already has a home
    /// ([`CredentialAttribution::account`], `config.credential_account`); a
    /// fleet spend ceiling is per-**credential**, and the metered case #8556
    /// describes is one key shared by every host, so keying spend by account
    /// would split exactly the number a ceiling has to add up.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}@{}", self.tap(), self.credential_wire())
    }
}

/// Parse the tap attribution out of one `# LOOM_LAUNCH {…}` record body.
///
/// `None` when the record carries no credential attribution at all (a
/// pre-#8401 binary — see [`parse_launch_credential`]) or names no runtime:
/// a tap key whose runtime half was fabricated would silently merge unrelated
/// spend, which is worse for this purpose than reporting nothing.
///
/// The `tap` field a post-#8556 binary writes is preferred as the configured
/// identity, but `runtime` + `profile` are the fallback so a record written by
/// an older binary still attributes — the same forward/backward-compatible
/// discipline every other reader of this record applies.
#[must_use]
pub fn parse_launch_tap(record_json: &str) -> Option<TapAttribution> {
    let credential = parse_launch_credential(record_json)?;
    let value: serde_json::Value = serde_json::from_str(record_json.trim()).ok()?;
    let stamped = value
        .get("tap")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // `tap` renders as `<runtime>[:<profile>]` (`Tap`'s Display), so splitting
    // on the first `:` recovers both halves. A record without it falls back to
    // #8507's own reader of the same record, so the two never drift in what
    // they consider a usable `runtime`/`profile`.
    let (runtime, model_profile) = match stamped {
        Some(tap) => match tap.split_once(':') {
            Some((runtime, profile)) => (
                runtime.trim().to_string(),
                Some(profile.trim().to_string()).filter(|p| !p.is_empty()),
            ),
            None => {
                let profile = parse_launch_runtime(record_json).and_then(|r| r.profile);
                (tap.to_string(), profile)
            }
        },
        None => {
            let attribution = parse_launch_runtime(record_json)?;
            (attribution.runtime, attribution.profile)
        }
    };
    if runtime.is_empty() {
        return None;
    }
    Some(TapAttribution {
        runtime,
        model_profile,
        credential,
    })
}

/// [`parse_launch_tap`] over a whole per-sweep log, anchored exactly as
/// [`parse_launch_credential_after`] anchors: only at/after this dispatch's
/// header, and within that region the **last** record wins.
#[must_use]
pub fn parse_launch_tap_after(contents: &str, header_anchor: &str) -> Option<TapAttribution> {
    let region = &contents[contents.rfind(header_anchor)?..];
    let body_start = region.rfind(LAUNCH_RECORD_MARKER)? + LAUNCH_RECORD_MARKER.len();
    parse_launch_tap(region[body_start..].lines().next()?)
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

    /// Issue #8541: a log line that merely *mentions* the marker mid-line
    /// (e.g. an agent transcript echoing it, not `worker_spawn::run`'s own
    /// line-start write) must not suppress attribution of the real, earlier,
    /// line-start record. An unanchored `rfind` of the marker substring would
    /// match inside the transcript line, take everything after it as the
    /// "record", fail to parse it as JSON, and return `None` — silently
    /// dropping attribution of a launch that really did happen.
    #[test]
    fn a_marker_merely_mentioned_mid_line_does_not_suppress_the_real_record() {
        let log = format!(
            "==== loom-daemon dispatch: sweep_id=s1 issue=42 ====\n{}\nagent transcript: I \
             noticed a line starting with \"{}\" earlier in the log while summarizing it\n",
            launch_line("pool", "zai", "alpha"),
            LAUNCH_RECORD_MARKER,
        );
        let attribution = parse_launch_credential_after(&log, "sweep_id=s1").unwrap();
        assert_eq!(attribution.source, "pool");
        assert_eq!(attribution.provider.as_deref(), Some("zai"));
        assert_eq!(attribution.account.as_deref(), Some("alpha"));
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

    // ========================================================================
    // Tap attribution (Issue #8556)
    // ========================================================================

    /// A launch record shaped exactly as `worker_spawn::run` writes it after
    /// #8556 — `tap` present alongside `runtime`/`profile`.
    fn tap_launch_line(
        tap: Option<&str>,
        runtime: &str,
        profile: Option<&str>,
        source: &str,
        provider: Option<&str>,
    ) -> String {
        let mut record = serde_json::json!({
            "schema": 1,
            "runtime": runtime,
            "model": "glm-5.3",
            "credentialSource": source,
            "credentialAccount": "alpha",
        });
        let map = record.as_object_mut().unwrap();
        if let Some(tap) = tap {
            map.insert("tap".to_string(), tap.into());
        }
        if let Some(profile) = profile {
            map.insert("profile".to_string(), profile.into());
        }
        if let Some(provider) = provider {
            map.insert("credentialProvider".to_string(), provider.into());
        }
        format!("# LOOM_LAUNCH {record}")
    }

    #[test]
    fn a_metered_pool_tap_keys_on_its_provider_not_its_account() {
        let log = log_with(
            "sweep_id=s1",
            &tap_launch_line(
                Some("opencode:zai-metered"),
                "opencode",
                Some("zai-metered"),
                "pool",
                Some("zai"),
            ),
        );
        let tap = parse_launch_tap_after(&log, "sweep_id=s1").unwrap();
        assert_eq!(tap.runtime, "opencode");
        assert_eq!(tap.model_profile.as_deref(), Some("zai-metered"));
        assert_eq!(tap.tap().to_string(), "opencode:zai-metered");
        assert_eq!(tap.credential_wire(), "api_keys:zai");
        // The account is deliberately NOT in the key: a fleet ceiling adds up
        // one shared credential's spend, and keying by account would split it.
        assert_eq!(tap.key(), "opencode:zai-metered@api_keys:zai");
        assert!(!tap.key().contains("alpha"));
    }

    #[test]
    fn an_env_sourced_and_an_unpooled_launch_each_get_their_own_bucket() {
        for (source, expected) in [("env", "env"), ("none", "harness-own")] {
            let log =
                log_with("sweep_id=s1", &tap_launch_line(Some("pi"), "pi", None, source, None));
            let tap = parse_launch_tap_after(&log, "sweep_id=s1").unwrap();
            assert_eq!(tap.credential_wire(), expected, "{source}");
            assert_eq!(tap.key(), format!("pi@{expected}"), "{source}");
        }
    }

    /// A record from a binary that predates the `tap` field still attributes:
    /// `runtime` + `profile` reconstruct the same identity.
    #[test]
    fn a_record_without_a_tap_field_falls_back_to_runtime_plus_profile() {
        let log = log_with(
            "sweep_id=s1",
            &tap_launch_line(None, "opencode", Some("zai-metered"), "pool", Some("zai")),
        );
        let tap = parse_launch_tap_after(&log, "sweep_id=s1").unwrap();
        assert_eq!(tap.key(), "opencode:zai-metered@api_keys:zai");
    }

    #[test]
    fn a_record_with_no_credential_attribution_or_no_runtime_yields_nothing() {
        // Pre-#8401: no credential fields at all.
        let log = log_with(
            "sweep_id=s1",
            r#"# LOOM_LAUNCH {"schema":1,"runtime":"pi","model":"glm-5.3"}"#,
        );
        assert_eq!(parse_launch_tap_after(&log, "sweep_id=s1"), None);
        // Credential present but no runtime to key on — never fabricated.
        let log = log_with(
            "sweep_id=s1",
            r#"# LOOM_LAUNCH {"credentialSource":"pool","credentialProvider":"zai"}"#,
        );
        assert_eq!(parse_launch_tap_after(&log, "sweep_id=s1"), None);
    }

    #[test]
    fn tap_attribution_renders_no_key_material() {
        let line = format!(
            "# LOOM_LAUNCH {}",
            serde_json::json!({
                "tap": "opencode:zai-metered",
                "credentialSource": "pool",
                "credentialProvider": "zai",
                "credentialAccount": "alpha",
                "credentialValue": "sk-fake-secret-material",
            })
        );
        let log = log_with("sweep_id=s1", &line);
        let tap = parse_launch_tap_after(&log, "sweep_id=s1").unwrap();
        let rendered = serde_json::to_string(&tap).unwrap();
        assert!(!rendered.contains("sk-fake-secret-material"), "{rendered}");
    }
}
