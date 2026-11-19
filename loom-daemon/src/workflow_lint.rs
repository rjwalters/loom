//! `loom-daemon workflow-lint <PATH…>` — structural lint for GitHub Actions
//! workflow YAML (issue #13896).
//!
//! # Why this exists: the vanished-gate false green
//!
//! A textual conflict-merge union of two independently-added copies of the
//! same CI setup block can leave a workflow with (a) two steps carrying the
//! same `id` inside one job, or (b) a duplicated mapping key (which a
//! pair-collapsing YAML loader silently resolves to the last copy). GitHub
//! rejects such a workflow **at validation, before any job exists**: the run
//! concludes `failure` with zero jobs and registers no check runs, so the
//! gate that workflow was supposed to provide does not fail — it
//! *disappears*, and every rollup an agent consults (the PR checks list,
//! `forge wait-checks`, `mergeStateStatus`, `check-ci-status.sh`) reads
//! green. Observed twice in a consumer repo (#13896). The Doctor now lints
//! every workflow file a resolved merge/rebase touched before pushing
//! (`defaults/.claude/commands/loom/doctor.md`, push block).
//!
//! # Contract
//!
//! Each PATH is a file or a directory (a directory contributes its direct
//! `*.yml` / `*.yaml` children, sorted). Zero paths = nothing to lint, a
//! clean exit 0, so `xargs -r`-style callers stay green on an empty diff.
//!
//! Per file, two checks:
//!
//! 1. **Duplicate mapping key, anywhere in the file** — detected while
//!    parsing, at every mapping level including nested ones. Anchors,
//!    aliases and merge keys (`<<:`) are expanded by the parser first, so
//!    reusing an anchor is not a false duplicate; a genuinely repeated key
//!    is.
//! 2. **Duplicate step `id` within one job** — `jobs.<job>.steps[].id`
//!    values compared per job. The same `id` in two *different* jobs is
//!    valid (GitHub scopes step ids to their job) and must stay clean.
//!
//! Exit codes (fail closed — every non-zero result blocks a caller's push):
//! `0` clean; non-zero (`1`) on any defect, including unparseable YAML
//! (GitHub would reject the file at validation, so a parse error is a
//! defect here, not an "ignore") and unreadable paths.
//!
//! Output: one `WORKFLOW-LINT-FAIL <path>: <detail>` line per defect on
//! stderr, naming the file, the job and the duplicated id/key so a caller's
//! report can quote them verbatim; `WORKFLOW-LINT-OK <n> file(s) checked`
//! on stdout when clean.

use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// An order-preserving YAML document: mappings keep every key pair in file
/// order, so a structural walk can see the whole file exactly as written.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

impl Node {
    /// The scalar's text as a YAML reader would write it, for id/key names in
    /// defect messages. `None` for collections (never valid as a step id).
    fn scalar_text(&self) -> Option<String> {
        match self {
            Node::String(s) => Some(s.clone()),
            Node::Bool(b) => Some(b.to_string()),
            Node::Number(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => {
                Some(format!("{}", *n as i64))
            }
            Node::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }

    fn new_string(s: &str) -> Node {
        Node::String(s.to_owned())
    }
}

struct NodeVisitor;

impl<'de> Visitor<'de> for NodeVisitor {
    type Value = Node;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "any YAML value")
    }

    fn visit_unit<E>(self) -> std::result::Result<Node, E> {
        Ok(Node::Null)
    }

    fn visit_none<E>(self) -> std::result::Result<Node, E> {
        Ok(Node::Null)
    }

    fn visit_bool<E>(self, v: bool) -> std::result::Result<Node, E> {
        Ok(Node::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> std::result::Result<Node, E> {
        Ok(Node::Number(v as f64))
    }

    fn visit_u64<E>(self, v: u64) -> std::result::Result<Node, E> {
        Ok(Node::Number(v as f64))
    }

    fn visit_f64<E>(self, v: f64) -> std::result::Result<Node, E> {
        Ok(Node::Number(v))
    }

    fn visit_str<E>(self, v: &str) -> std::result::Result<Node, E> {
        Ok(Node::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> std::result::Result<Node, E> {
        Ok(Node::String(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Node, A::Error> {
        let mut out = Vec::new();
        while let Some(item) = seq.next_element::<Node>()? {
            out.push(item);
        }
        Ok(Node::Seq(out))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Node, A::Error> {
        let mut entries: Vec<(Node, Node)> = Vec::new();
        while let Some(entry) = map.next_entry::<Node, Node>()? {
            let (key, value) = entry;
            if entries.iter().any(|(seen, _)| seen == &key) {
                let key_text = key.scalar_text().unwrap_or_else(|| "<non-scalar>".into());
                return Err(serde::de::Error::custom(format!(
                    "duplicate mapping key '{key_text}'"
                )));
            }
            entries.push((key, value));
        }
        Ok(Node::Map(entries))
    }
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Node, D::Error> {
        deserializer.deserialize_any(NodeVisitor)
    }
}

/// The two structural checks of one file's content: parse errors (which
/// include duplicate mapping keys, rejected by the visitor above) and
/// duplicate step ids per job. Each returned line is job/id/key specific but
/// file-agnostic — the caller prefixes the path.
fn lint_document(content: &str) -> Vec<String> {
    let root: Node = match serde_yaml_ng::from_str(content) {
        Ok(root) => root,
        Err(err) => return vec![format!("parse error: {err}")],
    };
    duplicate_step_ids(&root)
}

/// `jobs.<job>.steps[].id` values repeated within one job. Same id in two
/// different jobs is legal and never reported.
fn duplicate_step_ids(root: &Node) -> Vec<String> {
    let mut defects = Vec::new();
    let Node::Map(top) = root else {
        return defects;
    };
    let Some((_, jobs)) = top.iter().find(|(key, _)| key == &Node::new_string("jobs")) else {
        return defects;
    };
    let Node::Map(jobs) = jobs else {
        return defects;
    };
    for (job_key, job) in jobs {
        let Node::Map(job) = job else {
            continue;
        };
        let Some((_, steps)) = job
            .iter()
            .find(|(key, _)| key == &Node::new_string("steps"))
        else {
            continue;
        };
        let Node::Seq(steps) = steps else {
            continue;
        };
        let job_name = job_key
            .scalar_text()
            .unwrap_or_else(|| "<non-scalar job key>".into());
        let mut seen: Vec<String> = Vec::new();
        let mut duplicated: Vec<String> = Vec::new();
        for step in steps {
            let Node::Map(step) = step else {
                continue;
            };
            let Some((_, id)) = step.iter().find(|(key, _)| key == &Node::new_string("id")) else {
                continue;
            };
            let Some(id_text) = id.scalar_text() else {
                continue;
            };
            if seen.contains(&id_text) {
                if !duplicated.contains(&id_text) {
                    duplicated.push(id_text.clone());
                }
            } else {
                seen.push(id_text);
            }
        }
        for id in duplicated {
            defects.push(format!("job '{job_name}': duplicate step id '{id}'"));
        }
    }
    defects
}

/// Workflow YAML files selected from the given paths: files pass through
/// as-is; a directory contributes its direct `*.yml` / `*.yaml` children,
/// sorted for stable output.
fn select_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = Vec::new();
    for path in paths {
        let meta =
            std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
        if meta.is_dir() {
            let mut children: Vec<PathBuf> = std::fs::read_dir(path)
                .with_context(|| format!("listing directory {}", path.display()))?
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .filter(|child| {
                    child.is_file()
                        && matches!(
                            child.extension().and_then(|ext| ext.to_str()),
                            Some("yml") | Some("yaml")
                        )
                })
                .collect();
            children.sort();
            files.extend(children);
        } else {
            files.push(path.clone());
        }
    }
    Ok(files)
}

/// Lint the given paths, printing one `WORKFLOW-LINT-FAIL` line per defect.
/// Returns the defect count (0 = clean); the caller turns non-zero into the
/// failing exit.
fn lint_paths(paths: &[PathBuf]) -> Result<usize> {
    if paths.is_empty() {
        println!("WORKFLOW-LINT-OK 0 files (no paths given)");
        return Ok(0);
    }
    let files = select_files(paths)?;
    let mut defect_count = 0usize;
    for file in &files {
        let content =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        for detail in lint_document(&content) {
            eprintln!("WORKFLOW-LINT-FAIL {}: {}", file.display(), detail);
            defect_count += 1;
        }
    }
    if defect_count == 0 {
        println!("WORKFLOW-LINT-OK {} file(s) checked, no defects", files.len());
    } else {
        eprintln!(
            "workflow-lint: {defect_count} defect(s) across {} file(s). An invalid workflow is rejected by GitHub at validation with zero jobs: its gate vanishes and every rollup reads green. Fix before pushing or claiming a conflict resolved (#13896).",
            files.len()
        );
    }
    Ok(defect_count)
}

#[derive(Debug, clap::Args)]
pub struct WorkflowLintArgs {
    /// Workflow YAML files (or directories of `*.yml`/`*.yaml`) to lint.
    /// No paths = nothing to lint, exit 0.
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
}

impl WorkflowLintArgs {
    pub fn run(self) -> Result<()> {
        let defect_count = lint_paths(&self.paths)?;
        if defect_count > 0 {
            std::process::exit(1);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::Path;

    const CLEAN_WORKFLOW: &str = "\
name: CI
on:
  push:
    branches: [main]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - id: setup
        run: echo setup
      - id: build
        run: make
";

    #[test]
    fn clean_workflow_has_no_defects() {
        assert!(lint_document(CLEAN_WORKFLOW).is_empty());
    }

    #[test]
    fn duplicate_step_id_in_one_job_is_found_with_job_and_id_named() {
        let content = "\
jobs:
  build:
    steps:
      - id: setup
        run: echo one
      - id: setup
        run: echo two
";
        assert_eq!(
            lint_document(content),
            vec!["job 'build': duplicate step id 'setup'".to_string()]
        );
    }

    /// The exact defect the issue exists for: a textual conflict union of two
    /// identical setup blocks added independently on two branches.
    #[test]
    fn textual_union_of_two_identical_setup_blocks_is_found() {
        let content = "\
name: CI
on: [push]
jobs:
  gate:
    runs-on: ubuntu-latest
    steps:
      - id: setup
        uses: actions/checkout@v4
      - id: setup
        uses: actions/setup-node@v4
        with:
          node-version: 22
      - id: build
        run: make
";
        assert_eq!(
            lint_document(content),
            vec!["job 'gate': duplicate step id 'setup'".to_string()]
        );
    }

    #[test]
    fn duplicate_mapping_key_at_top_level_is_found() {
        let content = "name: a\nname: b\njobs: {}\n";
        let defects = lint_document(content);
        assert_eq!(defects.len(), 1);
        assert!(defects[0].contains("duplicate mapping key 'name'"), "{defects:?}");
    }

    #[test]
    fn duplicate_mapping_key_nested_inside_a_job_is_found() {
        let content = "\
jobs:
  build:
    runs-on: ubuntu-latest
    steps: []
    steps: []
";
        let defects = lint_document(content);
        assert_eq!(defects.len(), 1);
        assert!(defects[0].contains("duplicate mapping key 'steps'"), "{defects:?}");
    }

    #[test]
    fn quoted_and_plain_copies_of_one_key_are_a_duplicate() {
        let content = "\"name\": a\nname: b\n";
        assert!(lint_document(content)
            .iter()
            .any(|d| d.contains("duplicate mapping key 'name'")));
    }

    #[test]
    fn same_step_id_in_two_different_jobs_is_valid() {
        let content = "\
jobs:
  build:
    steps:
      - id: setup
        run: echo a
  test:
    steps:
      - id: setup
        run: echo b
";
        assert!(lint_document(content).is_empty());
    }

    #[test]
    fn anchors_and_merge_keys_are_not_duplicates() {
        let content = "\
defaults: &d
  runs-on: ubuntu-latest
jobs:
  build:
    <<: *d
    steps:
      - id: one
        run: echo
  test:
    <<: *d
    steps:
      - id: one
        run: echo
";
        assert!(lint_document(content).is_empty());
    }

    #[test]
    fn empty_steps_is_clean() {
        let content = "jobs:\n  build:\n    steps: []\n";
        assert!(lint_document(content).is_empty());
    }

    #[test]
    fn unparseable_yaml_is_a_defect_not_an_ignore() {
        let content = "jobs: [unclosed\n";
        let defects = lint_document(content);
        assert_eq!(defects.len(), 1);
        assert!(defects[0].starts_with("parse error:"), "{defects:?}");
    }

    #[test]
    fn non_workflow_yaml_without_jobs_is_clean() {
        assert!(lint_document("just: a document\n").is_empty());
    }

    #[test]
    fn duplicate_mapping_key_short_circuits_parse() {
        // A parse-level duplicate key means there is no valid tree to walk:
        // the file is reported once (the parse error) and the step-id walk
        // never starts. Fail-closed either way — GitHub rejects the file.
        let content = "\
name: a
name: b
jobs:
  build:
    steps:
      - id: setup
        run: echo one
      - id: setup
        run: echo two
";
        let defects = lint_document(content);
        assert_eq!(defects.len(), 1, "{defects:?}");
        assert!(defects[0].contains("duplicate mapping key 'name'"), "{defects:?}");
    }

    #[test]
    fn directory_selects_only_yaml_children_sorted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.yml"), CLEAN_WORKFLOW).unwrap();
        std::fs::write(dir.path().join("a.yaml"), CLEAN_WORKFLOW).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not yaml").unwrap();
        let files = select_files(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(files, vec![dir.path().join("a.yaml"), dir.path().join("b.yml")]);
    }

    #[test]
    fn explicit_file_path_is_selected_regardless_of_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.txt");
        std::fs::write(&path, CLEAN_WORKFLOW).unwrap();
        let files = select_files(&[path]).unwrap();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn missing_path_is_an_error() {
        assert!(select_files(&[PathBuf::from("/nonexistent/workflow.yml")]).is_err());
    }

    #[test]
    fn lint_paths_zero_paths_is_clean() {
        assert_eq!(lint_paths(&[]).unwrap(), 0);
    }

    #[test]
    fn lint_paths_reports_defect_count_for_a_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yml");
        std::fs::write(&path, "jobs:\n  build:\n    steps:\n      - id: x\n      - id: x\n")
            .unwrap();
        assert_eq!(lint_paths(&[path]).unwrap(), 1);
    }

    #[test]
    fn lint_paths_clean_file_prints_ok_and_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.yml");
        std::fs::write(&path, CLEAN_WORKFLOW).unwrap();
        assert_eq!(lint_paths(&[path]).unwrap(), 0);
    }

    #[test]
    fn scalar_text_renders_each_scalar_shape() {
        assert_eq!(Node::String("id".into()).scalar_text().as_deref(), Some("id"));
        assert_eq!(Node::Bool(true).scalar_text().as_deref(), Some("true"));
        assert_eq!(Node::Number(42.0).scalar_text().as_deref(), Some("42"));
        assert_eq!(Node::Number(0.5).scalar_text().as_deref(), Some("0.5"));
        assert_eq!(Node::Null.scalar_text(), None);
        assert_eq!(Node::Seq(vec![]).scalar_text(), None);
    }

    #[test]
    fn aliased_whole_step_expands_to_a_duplicate_id() {
        // An anchor reused as a whole second step materializes two identical
        // steps — same id — exactly like the textual union it often comes
        // from. The alias is expanded below the visitor, so the walk sees it.
        let content = "\
jobs:
  build:
    steps:
      - &s
        id: setup
        run: echo one
      - *s
";
        assert_eq!(
            lint_document(content),
            vec!["job 'build': duplicate step id 'setup'".to_string()]
        );
    }

    fn lint_file(path: &Path) -> Vec<String> {
        lint_document(&std::fs::read_to_string(path).unwrap())
    }

    #[test]
    fn lint_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wf.yml");
        std::fs::write(&path, CLEAN_WORKFLOW).unwrap();
        assert!(lint_file(&path).is_empty());
    }
}
