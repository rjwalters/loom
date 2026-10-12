//! Preserve the actual launch provenance for native curation completion.
use std::process::Command;

pub(super) fn prepare(cmd: &mut Command, role: &str, model: &str, runtime: Option<&str>) {
    if role == "curator" {
        cmd.env("LOOM_CURATION_MODEL", if model.is_empty() { "unknown" } else { model })
            .env("LOOM_CURATION_RUNTIME", runtime.unwrap_or("unknown"));
    } else {
        cmd.env_remove("LOOM_CURATION_MODEL")
            .env_remove("LOOM_CURATION_RUNTIME");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn daemon_curator_uses_launch_identity_and_never_builder_provenance() {
        let mut cmd = Command::new("unused-fixture");
        prepare(&mut cmd, "curator", "actual-curator", Some("codex"));
        let value = |cmd: &Command, name: &str| {
            cmd.get_envs()
                .find(|(key, _)| *key == name)
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().into_owned())
        };
        assert_eq!(value(&cmd, "LOOM_CURATION_MODEL").as_deref(), Some("actual-curator"));
        assert_eq!(value(&cmd, "LOOM_CURATION_RUNTIME").as_deref(), Some("codex"));
        prepare(&mut cmd, "builder", "builder-model", Some("claude"));
        assert_eq!(value(&cmd, "LOOM_CURATION_MODEL"), None);
        assert_eq!(value(&cmd, "LOOM_CURATION_RUNTIME"), None);
        prepare(&mut cmd, "curator", "", None);
        assert_eq!(value(&cmd, "LOOM_CURATION_MODEL").as_deref(), Some("unknown"));
        assert_eq!(value(&cmd, "LOOM_CURATION_RUNTIME").as_deref(), Some("unknown"));
    }
}
