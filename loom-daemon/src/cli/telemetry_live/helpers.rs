//! Fail-closed canary credentials and independent native-stream verification.
use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::io::Read;
use std::path::Path;

pub(super) fn key_from_zshrc(path: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .context("cannot open canary credential source")?
        .take(1_048_577)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1_048_576, "canary credential source exceeds 1 MiB");
    let key = literal_key(std::str::from_utf8(&bytes).context("credential source is not UTF-8")?)?;
    if let Some(ambient) = std::env::var_os("ZAI_API_KEY").filter(|v| !v.is_empty()) {
        ensure!(
            ambient == std::ffi::OsStr::new(&key),
            "ambient ZAI_API_KEY differs from literal credential source"
        );
    }
    Ok(key)
}

fn literal_key(source: &str) -> Result<String> {
    let mut found = None;
    for raw in source.lines() {
        let line = raw.trim();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some(value) = line.strip_prefix("ZAI_API_KEY=") else {
            continue;
        };
        ensure!(found.is_none(), "ambiguous multiple ZAI_API_KEY assignments");
        let value = value.trim();
        let (key, remainder) =
            if let Some(quote) = value.chars().next().filter(|c| matches!(c, '\'' | '"')) {
                let after = &value[1..];
                let end = after
                    .find(quote)
                    .context("unsupported ZAI_API_KEY literal syntax")?;
                (&after[..end], &after[end + 1..])
            } else {
                let end = value.find(char::is_whitespace).unwrap_or(value.len());
                (&value[..end], &value[end..])
            };
        ensure!(
            remainder.trim().is_empty() || remainder.trim().starts_with('#'),
            "unsupported ZAI_API_KEY assignment suffix"
        );
        ensure!(
            !key.is_empty()
                && key.len() <= 4096
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-/+=".contains(&b)),
            "ZAI_API_KEY must be a nonempty literal; shell evaluation is prohibited"
        );
        found = Some(key.to_owned());
    }
    found.context("no literal ZAI_API_KEY assignment found")
}

pub(super) fn verify_stream(
    runtime: &str,
    stdout: &[u8],
    stderr: &[u8],
    expected: &str,
) -> Result<()> {
    ensure!(
        stdout.len() + stderr.len() <= 1_048_576,
        "canary output exceeds verification bound"
    );
    let stdout = std::str::from_utf8(stdout).context("harness stdout is not UTF-8")?;
    let stderr = std::str::from_utf8(stderr).context("harness stderr is not UTF-8")?;
    let mut launches = 0;
    let mut read = false;
    let mut final_text = None;
    let provider = match runtime {
        "pi" => "zai",
        "opencode" => "zai-coding-plan",
        _ => bail!("unsupported canary runtime"),
    };
    for line in stdout.lines().chain(stderr.lines()) {
        if let Some(value) = line.strip_prefix("# LOOM_LAUNCH ") {
            let record: Value =
                serde_json::from_str(value).context("malformed launch provenance")?;
            ensure!(
                record["runtime"] == runtime
                    && record["provider"] == provider
                    && record["model"] == "glm-5.3-flash"
                    && record["profile"] == "zai-flash"
                    && record["effort"] == "low"
                    && record["credentialSource"] == "env",
                "canary launch provenance mismatches requested harness/profile/credential"
            );
            launches += 1;
        }
    }
    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = event["type"].as_str().unwrap_or_default();
        if runtime == "pi" {
            if matches!(kind, "tool_execution_start" | "tool_execution_end") {
                ensure!(
                    event["toolName"] == "loom_read",
                    "canary invoked a tool outside the read-only contract"
                );
            }
            if kind == "tool_execution_end"
                && event["toolName"] == "loom_read"
                && event["isError"] != true
            {
                read = true;
            }
            if kind == "message_end"
                && event.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
            {
                let text = event
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|v| v["type"] == "text")
                    .filter_map(|v| v["text"].as_str())
                    .collect::<String>();
                final_text = Some(text);
            }
        } else {
            if matches!(kind, "tool_use" | "tool-use") {
                let name = event
                    .get("tool")
                    .or_else(|| event.pointer("/part/tool"))
                    .and_then(Value::as_str);
                ensure!(
                    name == Some("loom_read"),
                    "canary invoked a tool outside the read-only contract"
                );
                if name == Some("loom_read")
                    && event
                        .pointer("/part/state/status")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s == "completed")
                {
                    read = true;
                }
            }
            if kind == "text" {
                final_text = event
                    .pointer("/part/text")
                    .or_else(|| event.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        }
    }
    ensure!(launches == 1, "expected exactly one pinned native launch");
    ensure!(read, "no successful guarded loom_read observed");
    ensure!(
        final_text.as_deref().map(str::trim) == Some(expected),
        "independent exact-response verification failed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_parser_never_evaluates_shell_or_echoes_material() {
        for text in [
            "export ZAI_API_KEY='literal.value'",
            "ZAI_API_KEY=literal.value # comment",
        ] {
            assert_eq!(literal_key(text).expect("literal"), "literal.value");
        }
        for text in [
            "ZAI_API_KEY=$(touch secret)",
            "ZAI_API_KEY=\"$SECRET\"",
            "ZAI_API_KEY=one\nZAI_API_KEY=two",
            "ZAI_API_KEY=abc;echo secret",
        ] {
            let error = literal_key(text).expect_err("unsafe syntax").to_string();
            assert!(!error.contains("touch secret"));
        }
    }
    #[test]
    fn matching_tool_output_cannot_substitute_for_assistant_response() {
        let provenance = br#"# LOOM_LAUNCH {"runtime":"pi","provider":"zai","model":"glm-5.3-flash","profile":"zai-flash","effort":"low","credentialSource":"env"}"#;
        let events = br#"{"type":"tool_execution_end","toolName":"loom_read","isError":false,"result":"CANARY_RESULT:abc"}
{"type":"message_end","message":{"role":"toolResult","content":[{"type":"text","text":"CANARY_RESULT:abc"}]}}"#;
        assert!(verify_stream("pi", events, provenance, "CANARY_RESULT:abc").is_err());
        let mut correct = events.to_vec();
        correct.extend_from_slice(b"\n{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"CANARY_RESULT:abc\"}]}}\n");
        assert!(verify_stream("pi", &correct, provenance, "CANARY_RESULT:abc").is_ok());
        assert!(verify_stream("opencode", &correct, provenance, "CANARY_RESULT:abc").is_err());
    }

    #[test]
    fn opencode_requires_an_observed_completed_guarded_read() {
        let provenance = br#"# LOOM_LAUNCH {"runtime":"opencode","provider":"zai-coding-plan","model":"glm-5.3-flash","profile":"zai-flash","effort":"low","credentialSource":"env"}"#;
        for status in [None, Some("error"), Some("running"), Some("completed")] {
            let mut tool =
                serde_json::json!({"type":"tool_use","part":{"tool":"loom_read","state":{}}});
            if let Some(status) = status {
                tool["part"]["state"]["status"] = status.into();
            }
            let events = format!(
                "{tool}\n{}\n",
                serde_json::json!({"type":"text","part":{"text":"CANARY_RESULT:abc"}})
            );
            assert_eq!(
                verify_stream("opencode", events.as_bytes(), provenance, "CANARY_RESULT:abc")
                    .is_ok(),
                status == Some("completed")
            );
        }
    }
}
