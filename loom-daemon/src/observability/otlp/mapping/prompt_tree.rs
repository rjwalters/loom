//! `role_tick.outcome`'s prompt tree as `loom.prompt.*` attributes (#11989),
//! so "role sessions running prompts older than main" is a SigNoz query:
//! `loom.prompt.commits_behind > 0`, or `loom.prompt.loom_version` against
//! the daemon's `loom.version`. Each attribute is absent when unknown.

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};

use super::{kv, kv_int, kv_string};
use crate::role_tick_telemetry::prompt_tree::PromptTree;

pub(super) fn attributes(prompt: Option<&PromptTree>) -> Vec<KeyValue> {
    let Some(prompt) = prompt else {
        return Vec::new();
    };
    let mut out = vec![kv_string("loom.prompt.commit", prompt.commit.clone())];
    if let Some(version) = &prompt.loom_version {
        out.push(kv_string("loom.prompt.loom_version", version.clone()));
    }
    if let Some(behind) = prompt.commits_behind {
        let behind = i64::try_from(behind).unwrap_or(i64::MAX);
        out.push(kv_int("loom.prompt.commits_behind", behind));
    }
    if let Some(on_branch) = prompt.on_branch {
        let value = AnyValue {
            value: Some(any_value::Value::BoolValue(on_branch)),
        };
        out.push(kv("loom.prompt.on_branch", value));
    }
    if let Some(tip) = &prompt.remote_tip {
        out.push(kv_string("loom.prompt.remote_tip", tip.clone()));
    }
    if let Some(n) = prompt.commits_behind_remote {
        let n = i64::try_from(n).unwrap_or(i64::MAX);
        out.push(kv_int("loom.prompt.commits_behind_remote", n));
    }
    for (key, flag) in [
        ("loom.prompt.origin_ref_stale", prompt.origin_ref_stale),
        ("loom.prompt.remote_foreign", prompt.remote_foreign),
    ] {
        if let Some(flag) = flag {
            let value = AnyValue {
                value: Some(any_value::Value::BoolValue(flag)),
            };
            out.push(kv(key, value));
        }
    }
    if let Some(repo) = &prompt.remote_repo {
        out.push(kv_string("loom.prompt.remote_repo", repo.clone()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(kvs: &[KeyValue]) -> Vec<&str> {
        kvs.iter().map(|kv| kv.key.as_str()).collect()
    }

    #[test]
    fn a_full_tree_maps_every_key_and_an_absent_one_maps_none() {
        let tree = PromptTree {
            commit: "a".repeat(40),
            loom_version: Some("0.19.1100".to_string()),
            commits_behind: Some(121),
            on_branch: Some(false),
            remote_tip: Some("c".repeat(40)),
            commits_behind_remote: Some(3),
            origin_ref_stale: Some(true),
            remote_repo: Some("o/r".to_string()),
            remote_foreign: Some(false),
        };
        let kvs = attributes(Some(&tree));
        assert_eq!(
            keys(&kvs),
            [
                "loom.prompt.commit",
                "loom.prompt.loom_version",
                "loom.prompt.commits_behind",
                "loom.prompt.on_branch",
                "loom.prompt.remote_tip",
                "loom.prompt.commits_behind_remote",
                "loom.prompt.origin_ref_stale",
                "loom.prompt.remote_foreign",
                "loom.prompt.remote_repo"
            ]
        );
        assert_eq!(kvs[2].value.as_ref().unwrap().value, Some(any_value::Value::IntValue(121)));
        assert!(attributes(None).is_empty());
        let bare = PromptTree {
            commit: "b".repeat(40),
            ..PromptTree::default()
        };
        assert_eq!(keys(&attributes(Some(&bare))), ["loom.prompt.commit"]);
    }
}
