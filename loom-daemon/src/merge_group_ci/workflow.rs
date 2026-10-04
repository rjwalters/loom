//! The slice of a GitHub Actions workflow the merge-group audit reads.

use std::path::Path;

use super::expr::{interpolate, Context, Value};
use super::yaml::{self, Node};

/// The marker that declares a job deliberately PR-only (e.g. a path-filter
/// job whose dependants all run unconditionally on `merge_group`). Written as
/// a comment anywhere inside the job block:
///
/// ```yaml
///   changes:
///     # merge-group-audit: pr-only -- path groups for PR runs; merge_group runs everything
/// ```
///
/// A marked job is excluded from the relied-on set, never reported covered.
/// A marked job that hosts a required context is still audited.
pub const PR_ONLY_MARKER: &str = "# merge-group-audit: pr-only";

/// One `on:` trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub event: String,
    /// `types:` filter, when present.
    pub types: Option<Vec<String>>,
    /// `paths:` / `paths-ignore:` present.
    pub path_filtered: bool,
    pub line: usize,
}

/// A `concurrency:` stanza (workflow or job level).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Concurrency {
    pub group: String,
    pub cancel_in_progress: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// 0-based position in `steps:`.
    pub index: usize,
    pub line: usize,
    pub name: Option<String>,
    pub uses: Option<String>,
    pub if_cond: Option<String>,
    pub with: Vec<(String, String)>,
}

impl Step {
    /// A human label: the step name, else its `uses:`, else its position.
    #[must_use]
    pub fn label(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.uses.clone())
            .unwrap_or_else(|| format!("step {}", self.index + 1))
    }

    #[must_use]
    pub fn with_value(&self, key: &str) -> Option<&str> {
        self.with
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[must_use]
    pub fn is_checkout(&self) -> bool {
        self.uses
            .as_deref()
            .is_some_and(|u| u.starts_with("actions/checkout@"))
    }

    #[must_use]
    pub fn is_paths_filter(&self) -> bool {
        self.uses
            .as_deref()
            .is_some_and(|u| u.contains("paths-filter") || u.contains("changed-files"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    pub line: usize,
    /// The raw `name:` (may embed `${{ matrix.* }}`).
    pub raw_name: Option<String>,
    /// Every check-run name this job posts, matrix-expanded. Empty when the
    /// name embeds an expression that cannot be expanded statically.
    pub names: Vec<String>,
    pub if_cond: Option<String>,
    pub needs: Vec<String>,
    pub concurrency: Option<Concurrency>,
    pub permissions: Option<String>,
    pub steps: Vec<Step>,
    /// Reason text after [`PR_ONLY_MARKER`], when the job carries one.
    pub pr_only: Option<String>,
}

impl Job {
    #[must_use]
    pub fn uses_paths_filter(&self) -> bool {
        self.steps.iter().any(Step::is_paths_filter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    /// Repo-relative path (or file name) used in findings.
    pub file: String,
    pub name: String,
    pub triggers: Vec<Trigger>,
    pub concurrency: Option<Concurrency>,
    pub permissions: Option<String>,
    pub jobs: Vec<Job>,
}

impl Workflow {
    #[must_use]
    pub fn trigger(&self, event: &str) -> Option<&Trigger> {
        self.triggers.iter().find(|t| t.event == event)
    }

    #[must_use]
    pub fn job(&self, id: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }
}

/// Parse one workflow file's text.
///
/// # Errors
///
/// When the YAML subset cannot read it, or it has no `jobs:` mapping.
pub fn parse(file: &str, src: &str) -> Result<Workflow, String> {
    let doc = yaml::parse(src)?;
    let name = doc
        .get("name")
        .and_then(Node::as_str)
        .map_or_else(|| default_name(file), str::to_string);
    let on_line = doc
        .entries()
        .iter()
        .find(|e| e.key == "on" || e.key == "true")
        .map_or(0, |e| e.line);
    let on = doc.get("on").or_else(|| doc.get("true"));
    let triggers = on.map_or_else(Vec::new, |n| triggers(n, on_line));
    let Some(Node::Map(job_entries)) = doc.get("jobs") else {
        return Err("no `jobs:` mapping".to_string());
    };
    let raw: Vec<&str> = src.lines().collect();
    let mut jobs = Vec::new();
    for (i, entry) in job_entries.iter().enumerate() {
        let end = job_entries.get(i + 1).map_or(raw.len() + 1, |e| e.line);
        let block = raw
            .get(entry.line.saturating_sub(1)..end.saturating_sub(1).min(raw.len()))
            .unwrap_or_default();
        jobs.push(job(&entry.key, entry.line, &entry.value, block));
    }
    Ok(Workflow {
        file: file.to_string(),
        name,
        triggers,
        concurrency: doc
            .get("concurrency")
            .map(|n| concurrency(n, line_of(&doc, "concurrency"))),
        permissions: doc.get("permissions").map(render_compact),
        jobs,
    })
}

/// One workflow file: parsed, or `(file, why)` when it could not be.
pub type Loaded = Result<Workflow, (String, String)>;

/// Load every `*.yml` / `*.yaml` under `<root>/.github/workflows`, sorted.
/// A file that cannot be read or parsed is returned as `Err((file, why))` so
/// the caller can report it rather than silently dropping it.
///
/// # Errors
///
/// When the workflows directory itself cannot be listed.
pub fn load_dir(root: &Path) -> Result<Vec<Loaded>, String> {
    let dir = root.join(".github/workflows");
    let rd = std::fs::read_dir(&dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    let mut files: Vec<_> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x == "yml" || x == "yaml")
        })
        .collect();
    files.sort();
    Ok(files
        .iter()
        .map(|p| {
            let rel = format!(
                ".github/workflows/{}",
                p.file_name().and_then(|f| f.to_str()).unwrap_or_default()
            );
            std::fs::read_to_string(p)
                .map_err(|e| e.to_string())
                .and_then(|src| parse(&rel, &src))
                .map_err(|why| (rel, why))
        })
        .collect())
}

fn default_name(file: &str) -> String {
    file.rsplit('/').next().unwrap_or(file).to_string()
}

fn line_of(doc: &Node, key: &str) -> usize {
    doc.entries()
        .iter()
        .find(|e| e.key == key)
        .map_or(0, |e| e.line)
}

fn strings(n: &Node) -> Vec<String> {
    match n {
        Node::Scalar(s) => vec![s.clone()],
        Node::Seq(items) => items
            .iter()
            .filter_map(Node::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn triggers(on: &Node, on_line: usize) -> Vec<Trigger> {
    let simple = |event: String| Trigger {
        event,
        types: None,
        path_filtered: false,
        line: on_line,
    };
    match on {
        Node::Scalar(_) | Node::Seq(_) => strings(on).into_iter().map(simple).collect(),
        Node::Map(entries) => entries
            .iter()
            .map(|e| Trigger {
                event: e.key.clone(),
                types: e.value.get("types").map(strings),
                path_filtered: e.value.get("paths").is_some()
                    || e.value.get("paths-ignore").is_some(),
                line: e.line,
            })
            .collect(),
        Node::Null => Vec::new(),
    }
}

fn concurrency(n: &Node, line: usize) -> Concurrency {
    match n {
        Node::Scalar(s) => Concurrency {
            group: s.clone(),
            cancel_in_progress: None,
            line,
        },
        _ => Concurrency {
            group: n
                .get("group")
                .and_then(Node::as_str)
                .unwrap_or_default()
                .to_string(),
            cancel_in_progress: n
                .get("cancel-in-progress")
                .and_then(Node::as_str)
                .map(str::to_string),
            line,
        },
    }
}

/// `read-all`, or `contents: read, pull-requests: read`.
fn render_compact(n: &Node) -> String {
    match n {
        Node::Scalar(s) => s.clone(),
        Node::Map(entries) => entries
            .iter()
            .map(|e| format!("{}: {}", e.key, e.value.as_str().unwrap_or("?")))
            .collect::<Vec<_>>()
            .join(", "),
        Node::Seq(_) => "?".to_string(),
        Node::Null => "{}".to_string(),
    }
}

fn job(id: &str, line: usize, n: &Node, block: &[&str]) -> Job {
    let raw_name = n.get("name").and_then(Node::as_str).map(str::to_string);
    let names = expand_names(raw_name.as_deref().unwrap_or(id), n.get("strategy"));
    let steps = n
        .get("steps")
        .map(|s| {
            s.items()
                .iter()
                .enumerate()
                .map(|(index, st)| Step {
                    index,
                    line: st.entries().first().map_or(line, |e| e.line),
                    name: st.get("name").and_then(Node::as_str).map(str::to_string),
                    uses: st.get("uses").and_then(Node::as_str).map(str::to_string),
                    if_cond: st.get("if").and_then(Node::as_str).map(str::to_string),
                    with: st
                        .get("with")
                        .map(|w| {
                            w.entries()
                                .iter()
                                .map(|e| {
                                    (
                                        e.key.clone(),
                                        e.value.as_str().unwrap_or_default().to_string(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    let pr_only = block.iter().find_map(|l| {
        l.trim_start().strip_prefix(PR_ONLY_MARKER).map(|rest| {
            rest.trim_start_matches([' ', '-', ':', '—'])
                .trim()
                .to_string()
        })
    });
    let concurrency_line = n
        .entries()
        .iter()
        .find(|e| e.key == "concurrency")
        .map_or(line, |e| e.line);
    Job {
        id: id.to_string(),
        line,
        raw_name,
        names,
        if_cond: n.get("if").and_then(Node::as_str).map(str::to_string),
        needs: n.get("needs").map(strings).unwrap_or_default(),
        concurrency: n
            .get("concurrency")
            .map(|c| concurrency(c, concurrency_line)),
        permissions: n.get("permissions").map(render_compact),
        steps,
        pr_only,
    }
}

/// Context that resolves only `matrix.*` and `strategy.job-total`.
struct MatrixScope<'a> {
    combo: &'a [(String, String)],
    total: usize,
}

impl Context for MatrixScope<'_> {
    fn lookup(&self, path: &[String]) -> Value {
        match path {
            [m, k] if m == "matrix" => self
                .combo
                .iter()
                .find(|(key, _)| key == k)
                .map_or(Value::Unknown, |(_, v)| Value::Str(v.clone())),
            [s, t] if s == "strategy" && t == "job-total" => {
                Value::Num(f64::from(u32::try_from(self.total).unwrap_or(u32::MAX)))
            }
            _ => Value::Unknown,
        }
    }
}

/// Expand a job display name over its matrix. Supports a matrix of plain
/// scalar lists (no `include` / `exclude`); anything else that the name
/// actually depends on yields no names (fail closed: a required context then
/// cannot be matched, which is reported).
fn expand_names(raw: &str, strategy: Option<&Node>) -> Vec<String> {
    if !raw.contains("${{") {
        return vec![raw.to_string()];
    }
    let axes: Option<Vec<(String, Vec<String>)>> =
        strategy
            .and_then(|s| s.get("matrix"))
            .and_then(|m| match m {
                Node::Map(entries) => entries
                    .iter()
                    .map(|e| match &e.value {
                        Node::Seq(_) if e.key != "include" && e.key != "exclude" => {
                            let vals = strings(&e.value);
                            (vals.len() == e.value.items().len()).then(|| (e.key.clone(), vals))
                        }
                        _ => None,
                    })
                    .collect(),
                _ => None,
            });
    let axes = axes.unwrap_or_default();
    let mut combos: Vec<Vec<(String, String)>> = vec![Vec::new()];
    for (k, vals) in &axes {
        combos = combos
            .into_iter()
            .flat_map(|c| {
                vals.iter().map(move |v| {
                    let mut next = c.clone();
                    next.push((k.clone(), v.clone()));
                    next
                })
            })
            .collect();
    }
    let total = combos.len();
    let mut out = Vec::new();
    for combo in &combos {
        match interpolate(raw, &MatrixScope { combo, total }) {
            Value::Str(s) => out.push(s),
            _ => return Vec::new(),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn matrix_names_expand_with_job_total() {
        let wf = parse(
            "ci.yml",
            "on: [pull_request]\njobs:\n  t:\n    name: Tests (${{ matrix.p }}/${{ strategy.job-total }})\n    strategy:\n      matrix:\n        p: [1, 2]\n    steps: []\n  u:\n    name: U ${{ matrix.missing }}\n    steps: []\n",
        )
        .unwrap();
        assert_eq!(wf.jobs[0].names, vec!["Tests (1/2)", "Tests (2/2)"]);
        assert!(wf.jobs[1].names.is_empty());
        assert_eq!(wf.triggers[0].event, "pull_request");
    }

    #[test]
    fn marker_and_triggers_are_read() {
        let wf = parse(
            "ci.yml",
            "on:\n  pull_request:\n    paths: ['a/**']\n  merge_group:\n    types: [checks_requested]\njobs:\n  changes:\n    # merge-group-audit: pr-only -- helper\n    if: github.event_name == 'pull_request'\n    steps: []\n  b:\n    steps: []\n",
        )
        .unwrap();
        assert!(wf.trigger("pull_request").unwrap().path_filtered);
        assert_eq!(
            wf.trigger("merge_group").unwrap().types.as_deref(),
            Some(&["checks_requested".to_string()][..])
        );
        assert_eq!(wf.jobs[0].pr_only.as_deref(), Some("helper"));
        assert_eq!(wf.jobs[1].pr_only, None);
    }
}
