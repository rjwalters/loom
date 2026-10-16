//! OTLP mapping for `session.prompt_tree` (#12509): one log record per
//! attended session start, stamped at `captured_at`. The `loom.prompt.*`
//! attributes are the ones a daemon role tick already exports, plus the four
//! remote facts; the collector's log `keep_keys` must list each
//! (contract-tested). The body is the record's JSON.

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_string, nanos, prompt_tree};
use crate::telemetry::TelemetryRecord;

/// `(event_name, severity, record time, attributes, body)` for a
/// `session.prompt_tree` record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::SessionPromptTree(r) = record else {
        return None;
    };
    let mut attributes = vec![
        kv_string("loom.session.output.launch", "attended".to_string()),
        kv_string("loom.session_id", r.session_id.clone()),
    ];
    if r.prompt.remote_foreign != Some(true) {
        if let Some(repo) = &r.prompt.remote_repo {
            attributes.push(kv_string("loom.repo", repo.clone()));
        }
    }
    attributes.extend(prompt_tree::attributes(Some(&r.prompt)));
    let body = serde_json::to_string(r).unwrap_or_default();
    Some((
        "session.prompt_tree",
        SeverityNumber::Info,
        nanos(r.captured_at),
        attributes,
        body,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::role_tick_telemetry::prompt_tree::PromptTree;
    use crate::telemetry::kinds::session_prompt_tree::{
        SessionPromptTreeRecord, SESSION_PROMPT_TREE_LOG_ATTRIBUTE_KEYS,
    };

    fn record(foreign: bool) -> SessionPromptTreeRecord {
        SessionPromptTreeRecord {
            captured_at: chrono::Utc::now(),
            session_id: "sess-1".to_string(),
            source: Some("startup".to_string()),
            prompt: PromptTree {
                commit: "a".repeat(40),
                loom_version: Some("0.19.1100".to_string()),
                commits_behind: Some(3),
                on_branch: Some(true),
                remote_tip: Some("b".repeat(40)),
                origin_ref_stale: Some(true),
                remote_repo: Some("o/r".to_string()),
                remote_foreign: Some(foreign),
            },
        }
    }

    fn has(attrs: &[KeyValue], key: &str) -> bool {
        attrs.iter().any(|kv| kv.key == key)
    }

    #[test]
    fn every_attribute_is_declared_and_the_collector_keeps_it() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let (name, _, _, attributes, body) =
            log_parts(&TelemetryRecord::SessionPromptTree(record(false))).unwrap();
        assert_eq!(name, "session.prompt_tree");
        assert!(has(&attributes, "loom.session.output.launch"));
        assert!(has(&attributes, "loom.repo"));
        assert!(has(&attributes, "loom.prompt.commits_behind"));
        assert!(body.contains("sess-1"), "{body}");
        for kv in &attributes {
            assert!(
                SESSION_PROMPT_TREE_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str()),
                "undeclared attribute key {}",
                kv.key
            );
        }
        let log_keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.eta.pr.state"))
            .expect("the transform/privacy log keep_keys line");
        for key in SESSION_PROMPT_TREE_LOG_ATTRIBUTE_KEYS {
            assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }

    #[test]
    fn a_foreign_origin_never_becomes_loom_repo() {
        let (_, _, _, attributes, _) =
            log_parts(&TelemetryRecord::SessionPromptTree(record(true))).unwrap();
        assert!(!has(&attributes, "loom.repo"));
        assert!(has(&attributes, "loom.prompt.remote_repo"));
    }
}
