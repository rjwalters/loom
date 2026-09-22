//! Runtime attribution on a `completion-v1` payload (Issue #8507).
//!
//! # The failure this fixes
//!
//! Per-model token attribution used to be Claude-transcript-only end to end,
//! so work done on a non-Claude runtime reached the public fleet feed with no
//! `tokens_by_model` — and therefore **no model badge at all**, not a wrong
//! one. The 2026-09-21 GLM-5.3 trial merged 46 PRs that narrated completions
//! with nothing identifying the model, the provider, or even the runtime.
//!
//! Two independent halves live here, both reached from
//! [`super::CompletionMeta`]:
//!
//! 1. **Labels.** [`insert_runtime_labels`] publishes `runtime`/`provider`/
//!    `profile`, and [`validate_runtime_labels`] refuses a blank one. These
//!    carry no token counts, so a completion is labelled by runtime and
//!    provider **even when no usage numbers were found anywhere** — which is
//!    the minimum the feed needed and could not get before.
//! 2. **Numbers.** [`fetch_tokens_by_model`] routes the per-model lookup
//!    through [`crate::usage_source`], so an OpenCode launch reads OpenCode's
//!    own session store instead of the Claude transcripts it never wrote.
//!
//! A sibling module rather than more lines in `safehouse.rs`, which is over
//! `.loom/docs/file-size-policy.md`'s threshold and frozen.
//!
//! # The omission contract, restated
//!
//! Every value here comes from the launch's own `# LOOM_LAUNCH` record. A
//! Claude or legacy-adapter spawn writes none, so all three labels are absent
//! for it — never a fabricated `"claude"` default — which is exactly what
//! keeps every pre-#8507 completion payload byte-identical.

use std::path::PathBuf;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use super::{transcript_tokens_enabled, CompletionMeta, TOKEN_LOOKUP_TIMEOUT};
use crate::script_helpers::sweep_experiment::ModelUsageTotals;

/// The three launch-attribution keys, in the order they are published.
const RUNTIME_LABEL_KEYS: [&str; 3] = ["runtime", "provider", "profile"];

/// Publish `meta`'s runtime labels onto the `completion-v1` object under
/// construction.
///
/// Each is independently optional and trimmed; a blank value is an omission,
/// not a publishable label (a blank would render as an empty badge). With all
/// three absent the object is byte-identical to a pre-#8507 one.
pub(super) fn insert_runtime_labels(obj: &mut Map<String, Value>, meta: &CompletionMeta) {
    let values = [
        meta.runtime.as_deref(),
        meta.provider.as_deref(),
        meta.profile.as_deref(),
    ];
    for (key, value) in RUNTIME_LABEL_KEYS.iter().zip(values) {
        if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
            obj.insert((*key).into(), json!(v));
        }
    }
}

/// Reject a runtime label that is present but unusable.
///
/// Deliberately **not** a closed enum like `visibility`: the runtime and
/// provider vocabularies grow with every new adapter, and a feed that renders
/// an unknown label as plain text degrades correctly. What is enforced is that
/// a present label is a non-empty string — [`insert_runtime_labels`] omits
/// rather than emits a blank one, so a blank reaching here is a hand-built
/// payload and a caller bug.
///
/// # Errors
/// When any of the three keys is present and is not a non-empty string.
pub(super) fn validate_runtime_labels(obj: &Map<String, Value>) -> Result<()> {
    for key in RUNTIME_LABEL_KEYS {
        if let Some(v) = obj.get(key) {
            match v.as_str() {
                Some(s) if !s.trim().is_empty() => {}
                _ => bail!(
                    "completion `meta.{key}`, when present, must be a non-empty string, got {v}"
                ),
            }
        }
    }
    Ok(())
}

/// Per-`(model, speed, service_tier)` token totals for a `completion`
/// envelope (#5740), from whichever on-disk store this launch's `runtime`
/// selects (#8507 — see [`crate::usage_source`]).
///
/// The activity DB's rollup has no per-model granularity to offer, so unlike
/// the flat `tokens` this has a single source and degrades straight to `None`
/// when it comes up empty: opted out via `LOOM_SAFEHOUSE_TRANSCRIPT_TOKENS=0`,
/// nothing attributable found, or the lookup timed out. `runtime` is the
/// sweep's own `# LOOM_LAUNCH` value; `None` (a Claude spawn) keeps the
/// pre-#8507 transcript reader. Runs on the blocking pool under the same
/// [`TOKEN_LOOKUP_TIMEOUT`] as every other token lookup, so a pathological
/// store can never delay a completion past the lookup budget.
pub(super) async fn fetch_tokens_by_model(
    runtime: Option<&str>,
    workspace_root: &str,
    issue: u32,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Option<Vec<ModelUsageTotals>> {
    if !transcript_tokens_enabled() {
        return None;
    }
    let root = PathBuf::from(workspace_root);
    let runtime = runtime.map(str::to_owned);
    let scan = tokio::task::spawn_blocking(move || {
        crate::usage_source::sweep_tokens_by_model(runtime.as_deref(), &root, issue, Some(window))
    });
    tokio::time::timeout(TOKEN_LOOKUP_TIMEOUT, scan)
        .await
        .ok()?
        .ok()?
}

/// A valid `completion-v1` source, tweaked per-test.
///
/// Lives here rather than in `safehouse/tests.rs` (also frozen by the
/// file-size ratchet) and is shared with it. The three #8507 labels are
/// populated so every test over this sample exercises the populated,
/// non-Claude shape; the Claude shape is all-absent, asserted explicitly in
/// `safehouse::tests::completion_meta_omits_absent_optional_fields`.
#[cfg(test)]
pub(crate) fn sample_completion_meta() -> CompletionMeta {
    CompletionMeta {
        agent: "loom_daemon".to_owned(),
        repo_slug: "rjwalters/loom".to_owned(),
        pr_url: "https://github.com/rjwalters/loom/pull/4321".to_owned(),
        result: super::CompletionResult::Success,
        started_at: "2026-07-29T10:00:00Z".to_owned(),
        completed_at: "2026-07-29T10:12:30Z".to_owned(),
        issue: Some(4321),
        tokens: Some(791_000),
        tokens_by_model: Some(vec![ModelUsageTotals {
            model: "claude-sonnet-5".to_owned(),
            speed: "standard".to_owned(),
            service_tier: "standard".to_owned(),
            input: 1_000,
            cache_read: 700_000,
            cache_write_5m: 1_000,
            cache_write_1h: 89_000,
            output: 30_000,
        }]),
        title: Some("Add repo-qualified task_id".to_owned()),
        additions: Some(214),
        deletions: Some(37),
        visibility: Some(super::RepoVisibility::Public),
        runtime: Some("opencode".to_owned()),
        provider: Some("friendli".to_owned()),
        profile: Some("glm-coding".to_owned()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::safehouse::validate_completion_meta;

    #[test]
    fn a_completion_publishes_the_launchs_runtime_provider_and_profile() {
        let meta = sample_completion_meta().to_meta_value().unwrap();
        assert_eq!(meta["runtime"], json!("opencode"));
        assert_eq!(meta["provider"], json!("friendli"));
        assert_eq!(meta["profile"], json!("glm-coding"));
    }

    #[test]
    fn the_labels_survive_a_completion_with_no_token_accounting_at_all() {
        // The whole point of #8507: an OpenCode completion whose usage numbers
        // could not be found is still identifiable on the feed.
        let meta = CompletionMeta {
            tokens: None,
            tokens_by_model: None,
            ..sample_completion_meta()
        }
        .to_meta_value()
        .unwrap();
        assert!(meta.get("tokens").is_none());
        assert!(meta.get("tokens_by_model").is_none());
        assert_eq!(meta["runtime"], json!("opencode"));
        assert_eq!(meta["provider"], json!("friendli"));
    }

    #[test]
    fn each_label_is_independently_optional() {
        let meta = CompletionMeta {
            provider: None,
            profile: None,
            ..sample_completion_meta()
        }
        .to_meta_value()
        .unwrap();
        assert_eq!(meta["runtime"], json!("opencode"));
        assert!(meta.get("provider").is_none());
        assert!(meta.get("profile").is_none());
    }

    #[test]
    fn a_blank_label_is_omitted_rather_than_published_as_an_empty_badge() {
        for blank in ["", "   ", "\n\t"] {
            let meta = CompletionMeta {
                runtime: Some(blank.to_owned()),
                provider: Some(blank.to_owned()),
                profile: Some(blank.to_owned()),
                ..sample_completion_meta()
            }
            .to_meta_value()
            .unwrap();
            for key in RUNTIME_LABEL_KEYS {
                assert!(meta.get(key).is_none(), "blank {key} must be omitted: {blank:?}");
            }
        }
    }

    #[test]
    fn a_label_is_trimmed_on_the_way_to_the_wire() {
        let meta = CompletionMeta {
            runtime: Some("  opencode  ".to_owned()),
            ..sample_completion_meta()
        }
        .to_meta_value()
        .unwrap();
        assert_eq!(meta["runtime"], json!("opencode"));
    }

    #[test]
    fn validation_refuses_a_blank_or_non_string_label_on_the_wire() {
        // `to_meta_value` omits a blank one, so a blank reaching the validator
        // is a hand-built payload — refuse it rather than publish it.
        let mut meta = sample_completion_meta().to_meta_value().unwrap();
        validate_completion_meta(&meta).unwrap();
        for key in RUNTIME_LABEL_KEYS {
            let mut blank = meta.clone();
            blank[key] = json!("  ");
            assert!(validate_completion_meta(&blank).is_err(), "{key} blank must be refused");
            let mut wrong_type = meta.clone();
            wrong_type[key] = json!(7);
            assert!(
                validate_completion_meta(&wrong_type).is_err(),
                "{key} non-string must be refused"
            );
        }
        // Absent stays legal — that is the Claude case.
        for key in RUNTIME_LABEL_KEYS {
            meta.as_object_mut().unwrap().remove(key);
        }
        validate_completion_meta(&meta).unwrap();
    }
}
