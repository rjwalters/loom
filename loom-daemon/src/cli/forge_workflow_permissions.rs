//! A structural check of GitHub Actions' `permissions: write-all` setting.
//! The Champion caller owns head pinning and its other capability checks.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{self, Read};

const MAX_WORKFLOW_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
struct Workflow {
    #[serde(default)]
    permissions: Permissions,
    jobs: BTreeMap<String, Job>,
}

#[derive(Deserialize)]
struct Job {
    #[serde(default)]
    permissions: Permissions,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum Permissions {
    #[default]
    #[serde(skip)]
    Unspecified,
    All(String),
    Scopes(BTreeMap<String, String>),
}

impl Permissions {
    fn is_write_all(&self) -> anyhow::Result<bool> {
        match self {
            Self::Unspecified => Ok(false),
            Self::All(value) if value.trim().eq_ignore_ascii_case("write-all") => Ok(true),
            Self::All(value) if value.trim().eq_ignore_ascii_case("read-all") => Ok(false),
            // Named write scopes still go through Champion's existing policy.
            Self::Scopes(scopes)
                if scopes
                    .values()
                    .all(|value| matches!(value.as_str(), "read" | "write" | "none")) =>
            {
                Ok(false)
            }
            _ => anyhow::bail!("unrecognized permissions value"),
        }
    }
}

fn contains_write_all(input: &[u8]) -> anyhow::Result<bool> {
    anyhow::ensure!(input.len() <= MAX_WORKFLOW_BYTES, "workflow exceeds the input size limit");
    let text = std::str::from_utf8(input)?;
    let options = serde_saphyr::options! {
        with_snippet: false,
        reject_unsupported_tags: true,
        budget: serde_saphyr::budget! {
            max_reader_input_bytes: Some(MAX_WORKFLOW_BYTES),
            max_documents: 1,
        },
    };
    let workflow: Workflow = serde_saphyr::from_str_with_options(text, options)
        .map_err(|_| anyhow::anyhow!("unreadable workflow YAML"))?;
    let mut has_write_all = workflow.permissions.is_write_all()?;
    for job in workflow.jobs.values() {
        has_write_all |= job.permissions.is_write_all()?;
    }
    Ok(has_write_all)
}

pub(super) fn run() -> ! {
    let mut input = Vec::new();
    let code = match io::stdin()
        .lock()
        .take(MAX_WORKFLOW_BYTES as u64 + 1)
        .read_to_end(&mut input)
    {
        Ok(_) => match contains_write_all(&input) {
            Ok(false) => 0,
            Ok(true) => 1,
            Err(_) => 2,
        },
        Err(_) => 2,
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME_ONLY: &[u8] = include_bytes!(
        "../../../defaults/scripts/tests/fixtures/champion-workflows/ci-write-all-test-name.yml"
    );
    const JOB_PERMISSION: &[u8] = include_bytes!(
        "../../../defaults/scripts/tests/fixtures/champion-workflows/ci-write-all-permission.yml"
    );

    #[test]
    fn distinguishes_test_text_from_permissions() {
        assert!(matches!(contains_write_all(NAME_ONLY), Ok(false)));
        assert!(matches!(contains_write_all(JOB_PERMISSION), Ok(true)));
        for text in [
            "permissions: write-all\njobs: {check: {}}\n",
            "'permissions': \"write-all\"\njobs: {check: {}}\n",
            "permissions:\n  write-all\njobs: {check: {}}\n",
            "permissions: >\n  write-all\njobs: {check: {}}\n",
            "jobs: {check: {permissions: write-all}}\n",
            "env: {ALL: &all write-all}\njobs: {check: {permissions: *all}}\n",
        ] {
            assert!(matches!(contains_write_all(text.as_bytes()), Ok(true)), "{text}");
        }
    }

    #[test]
    fn leaves_read_only_and_named_scopes_to_existing_policy() {
        for text in [
            "permissions: read-all\njobs: {check: {}}\n",
            "permissions: {}\njobs: {check: {}}\n",
            "permissions: {contents: write}\njobs: {check: {}}\n",
            "jobs: {check: {env: {permissions: write-all}}}\n",
        ] {
            assert!(matches!(contains_write_all(text.as_bytes()), Ok(false)), "{text}");
        }
    }

    #[test]
    fn rejects_unreadable_workflows() {
        for input in [
            b"".as_slice(),
            b"\xff",
            b"jobs: [",
            b"name: not a workflow\n",
            b"permissions: null\njobs: {check: {}}\n",
            b"permissions: unknown\njobs: {check: {}}\n",
            b"permissions: read-all\npermissions: write-all\njobs: {check: {}}\n",
            b"jobs: {check: {}}\n---\njobs: {check: {permissions: write-all}}\n",
        ] {
            assert!(contains_write_all(input).is_err(), "{input:?}");
        }
        assert!(contains_write_all(&vec![b' '; MAX_WORKFLOW_BYTES + 1]).is_err());
    }
}
