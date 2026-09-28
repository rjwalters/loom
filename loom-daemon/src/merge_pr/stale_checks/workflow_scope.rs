//! Narrowing `.github/workflows/ci.yml` from a whole-file global input to the
//! job/component block a required check actually runs (#9065).
//!
//! # The problem
//!
//! [`super::inputs`] lists [`CI_WORKFLOW`] in the `G` (global input) set of
//! **every** component spec, and it has to: a job's steps, its runner and its
//! path filters all live in that one file, so a change to the block that
//! defines a gate can flip that gate's verdict for every file.
//!
//! The cost is that `G` is keyed on a *path*, and one path covers 2200 lines
//! that define ~25 jobs of which three are required. Editing the `backend-tests`
//! partition count — which no required context reads — therefore made **every
//! open PR that touches anything** stale, because clause 1 fired on
//! `D ∩ G ∋ ci.yml`. Measured cost on 2026-09-26 (#9065's own problem
//! statement): `main` moves about every 11 minutes, CI takes 6-8 min, and any
//! `ci.yml` change on `main` re-loses the race for every PR in flight.
//!
//! # The narrowing
//!
//! `ci.yml` in `D` still counts as a global-input move — but only for the
//! components whose own definition the base move actually edited. The mapping
//! is derived from the tip's `ci.yml` itself, never hand-maintained:
//!
//! - lines **outside** the `jobs:` mapping (`on:`, `env:`, `concurrency:`,
//!   `permissions:`, `defaults:`) affect every component;
//! - lines in the job that runs component `C`, but **before/outside** any
//!   `# component:` marker (checkout, artifact download), affect every
//!   component of that job;
//! - lines under `# component: C` affect **only** `C`;
//! - lines in any job in `C`'s job's `needs:` closure (`build-daemon`,
//!   `changes`) affect `C`, because that job's output is an input to it.
//!
//! This is emphatically **not** path-filtering a check (`ci-principles.md`
//! rule 3). Nothing is skipped and no check's own coverage narrows: the
//! question here is only whether a green result the check *already produced*
//! can still be trusted, and a job block the check does not run is not part of
//! what it read.
//!
//! # Fail-closed, in every direction
//!
//! Everything below answers [`CiScope::Unscoped`] — i.e. "`ci.yml` is a global
//! input of everything", the pre-#9065 behaviour — the moment it cannot prove
//! otherwise:
//!
//! - `ci.yml` was added, removed or renamed rather than `modified`;
//! - the compare suppressed its patch (too large / binary);
//! - a hunk header will not parse, or names a line past the tip file's end;
//! - a **deleted** line is structural: a top-level key, a job key, or a
//!   `# component:` marker. A deletion is attributed by position on the *new*
//!   side, and a deleted job is not in the tip's map at all, so any structural
//!   deletion is treated as unattributable rather than guessed at;
//! - any touched line lands outside every job (the preamble);
//! - a required check's job cannot be located in the tip's workflow, or its
//!   `needs:` closure names a job that is not there;
//! - the tip's `# component:` markers inside a required job do not name
//!   exactly the components [`REQUIRED_CHECKS`] lists for it (see
//!   [`markers_agree`]) — the case that covers a gate `main` added to a
//!   required job after this binary was built.
//!
//! The narrowing therefore only ever *removes* refusals it can justify from
//! the workflow's own text, and any doubt restores the old, broader answer.

use super::evidence::ChangedFile;
use super::inputs::{CI_WORKFLOW, REQUIRED_CHECKS};
use std::collections::{BTreeSet, VecDeque};

/// Which components' `ci.yml` inputs a base move touched.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CiScope {
    /// `ci.yml` is a global input of every component — the pre-#9065 behaviour
    /// and the answer to every unattributable edit.
    #[default]
    Unscoped,
    /// Only these components (by [`super::inputs::CheckSpec::context`]) have a
    /// `ci.yml` input in this base move.
    Scoped(BTreeSet<String>),
}

impl CiScope {
    /// Does a `ci.yml` entry in `D` count as a global-input change for
    /// `component`?
    #[must_use]
    pub fn affects(&self, component: &str) -> bool {
        match self {
            Self::Unscoped => true,
            Self::Scoped(set) => set.contains(component),
        }
    }
}

// --- The workflow map --------------------------------------------------------

/// One `# component: <name>` block inside a composite job, as a 1-based
/// inclusive line range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub name: String,
    pub start: usize,
    pub end: usize,
}

/// One `jobs:` entry: where it is, what it is called, and what it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The `jobs:` mapping key (`structural-checks`), which `needs:` names.
    pub key: String,
    /// The rendered `name:` values — the status-check contexts this job
    /// reports as. More than one when the name interpolates `matrix.os`.
    pub names: Vec<String>,
    /// The job keys this job's `needs:` lists.
    pub needs: Vec<String>,
    /// 1-based line of the `  <key>:` line.
    pub start: usize,
    /// 1-based last line belonging to this job.
    pub end: usize,
    /// The `# component:` blocks in step order.
    pub components: Vec<Component>,
    /// The job's body lines (everything after the key line), for callers that
    /// want to read its steps.
    pub lines: Vec<String>,
}

/// A parsed `ci.yml`: its jobs, plus how long the file is (so a hunk naming a
/// line past the end can fail closed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    pub jobs: Vec<Job>,
    pub line_count: usize,
}

/// Who owns a line of `ci.yml`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Owner {
    /// Outside every job: `on:`, `env:`, `concurrency:`, the `jobs:` key
    /// itself, and the comment block before the first job.
    Preamble,
    /// Inside job `key`, outside every `# component:` block.
    JobSetup(String),
    /// Inside job `key`'s `# component: name` block.
    Component(String, String),
}

/// A deliberately small `ci.yml` reader: job keys at indent 2 under `jobs:`,
/// `name:`/`needs:` at indent 4, `# component:` markers at indent 6, and
/// `${{ matrix.os }}` expanded from the job's own `os:` list.
///
/// Hand-rolled rather than pulled from a YAML crate on purpose: the guard must
/// behave identically to the pin test in `inputs/tests.rs` (which reads the
/// same file for the same reason), and a dependency that resolves anchors,
/// merges and aliases would answer a *different* question from "which lines of
/// this file define this gate".
#[must_use]
pub fn parse(yaml: &str) -> Workflow {
    let all: Vec<&str> = yaml.lines().collect();
    let mut jobs: Vec<Job> = Vec::new();
    let mut in_jobs = false;
    let mut current: Option<Job> = None;

    for (idx, line) in all.iter().enumerate() {
        let lineno = idx + 1;
        if !in_jobs {
            in_jobs = *line == "jobs:";
            continue;
        }
        if !line.starts_with(' ') && !line.trim().is_empty() && !line.starts_with('#') {
            break; // left the `jobs:` mapping
        }
        if is_job_key(line) {
            if let Some(job) = current.take() {
                jobs.push(job);
            }
            current = Some(Job {
                key: line.trim().trim_end_matches(':').to_string(),
                names: Vec::new(),
                needs: Vec::new(),
                start: lineno,
                end: lineno,
                components: Vec::new(),
                lines: Vec::new(),
            });
            continue;
        }
        let Some(job) = current.as_mut() else {
            continue; // the comment block between `jobs:` and the first key
        };
        job.end = lineno;
        job.lines.push((*line).to_string());
        if let Some(name) = line.strip_prefix("      # component: ") {
            if let Some(prev) = job.components.last_mut() {
                prev.end = lineno - 1;
            }
            job.components.push(Component {
                name: name.trim().to_string(),
                start: lineno,
                end: lineno,
            });
        } else if let Some(last) = job.components.last_mut() {
            last.end = lineno;
        }
    }
    if let Some(job) = current.take() {
        jobs.push(job);
    }
    for job in &mut jobs {
        job.names = rendered_names(&job.lines);
        job.needs = declared_needs(&job.lines);
    }
    Workflow {
        jobs,
        line_count: all.len(),
    }
}

/// `  <key>:` — exactly two spaces of indent, nothing but a key.
fn is_job_key(line: &str) -> bool {
    line.starts_with("  ")
        && !line.starts_with("   ")
        && line.trim_end().ends_with(':')
        && !line.trim_start().starts_with('#')
}

/// The job's `name:`, with `${{ matrix.os }}` expanded over its `os:` list —
/// the only interpolation this repo's required contexts use. A name carrying
/// any other `${{ … }}` is left as written; it will simply not match a
/// required context, which fails closed in [`scope_for_patch`].
fn rendered_names(lines: &[String]) -> Vec<String> {
    let raw = lines
        .iter()
        .find_map(|l| l.strip_prefix("    name: "))
        .unwrap_or("")
        .trim()
        .to_string();
    if raw.contains("${{ matrix.os }}") {
        let os = matrix_os(lines);
        if !os.is_empty() {
            return os
                .into_iter()
                .map(|o| raw.replace("${{ matrix.os }}", &o))
                .collect();
        }
    }
    vec![raw]
}

/// The job's `os:` matrix values, from either the flow (`os: [a, b]`) or block
/// (`os:` then `- a`) form.
fn matrix_os(lines: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_os = false;
    for line in lines {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("os:") {
            let rest = rest.trim();
            if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                return inner
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            in_os = rest.is_empty();
            continue;
        }
        if in_os {
            match t.strip_prefix("- ") {
                Some(v) => out.push(v.trim().trim_matches('"').to_string()),
                None => in_os = false,
            }
        }
    }
    out
}

/// The job keys a job's `needs:` names, in flow (`needs: [a, b]`), scalar
/// (`needs: a`) or block (`needs:` then `- a`) form. Only the key-indented
/// `    needs:` counts, so prose in a comment cannot be mistaken for one.
fn declared_needs(lines: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in lines {
        if let Some(rest) = line.strip_prefix("    needs:") {
            let rest = rest.trim();
            if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                return inner
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            if rest.is_empty() {
                in_block = true;
            } else {
                out.push(rest.trim_matches('"').trim_matches('\'').to_string());
            }
            continue;
        }
        if in_block {
            match line.trim().strip_prefix("- ") {
                Some(v) => out.push(v.trim().trim_matches('"').to_string()),
                None => in_block = false,
            }
        }
    }
    out
}

impl Job {
    /// The body line at file line `lineno`, if this job holds it.
    fn line_at(&self, lineno: usize) -> Option<&str> {
        // `lines` holds every line after the key line, in order, so body index
        // `i` is file line `start + 1 + i`.
        lineno
            .checked_sub(self.start + 1)
            .and_then(|i| self.lines.get(i))
            .map(String::as_str)
    }

    /// The lines of `component`'s block, its `# component:` marker included.
    /// Empty for a name this job does not declare.
    #[must_use]
    pub fn component_lines(&self, component: &str) -> Vec<&str> {
        self.components
            .iter()
            .find(|c| c.name == component)
            .map(|c| (c.start..=c.end).filter_map(|n| self.line_at(n)).collect())
            .unwrap_or_default()
    }

    /// The lines before the first `# component:` marker — a composite job's
    /// shared setup (checkout, artifact download), which belongs to no gate.
    #[must_use]
    pub fn setup_lines(&self) -> Vec<&str> {
        let end = self.components.first().map_or(self.end + 1, |c| c.start);
        (self.start + 1..end)
            .filter_map(|n| self.line_at(n))
            .collect()
    }
}

impl Workflow {
    /// The job whose rendered `name:` is `context`, i.e. the one that reports
    /// that status-check context.
    #[must_use]
    pub fn job_named(&self, context: &str) -> Option<&Job> {
        self.jobs
            .iter()
            .find(|j| j.names.iter().any(|n| n == context))
    }

    fn job(&self, key: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.key == key)
    }

    fn owner(&self, line: usize) -> Owner {
        let Some(job) = self.jobs.iter().find(|j| line >= j.start && line <= j.end) else {
            return Owner::Preamble;
        };
        match job
            .components
            .iter()
            .find(|c| line >= c.start && line <= c.end)
        {
            Some(c) => Owner::Component(job.key.clone(), c.name.clone()),
            None => Owner::JobSetup(job.key.clone()),
        }
    }

    /// `key` plus every job reachable through `needs:`. `None` when a `needs:`
    /// names a job this workflow does not define — an unattributable state.
    fn needs_closure(&self, key: &str) -> Option<BTreeSet<String>> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(key.to_string());
        while let Some(k) = queue.pop_front() {
            if !seen.insert(k.clone()) {
                continue;
            }
            let job = self.job(&k)?;
            for n in &job.needs {
                queue.push_back(n.clone());
            }
        }
        Some(seen)
    }
}

// --- Attribution -------------------------------------------------------------

/// Everything a base move's `ci.yml` patch touched, bucketed by owner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Touched {
    /// A line outside every job — makes every component affected.
    preamble: bool,
    /// Job keys touched outside any `# component:` block.
    jobs: BTreeSet<String>,
    /// `(job key, component name)` blocks touched.
    components: BTreeSet<(String, String)>,
}

/// [`scope_for_patch`] for the `ci.yml` entry of a compare's file list, if it
/// has one. A base move that did not touch `ci.yml` gets
/// [`CiScope::Unscoped`], which is inert: `D` holds no `ci.yml` path for the
/// scope to narrow.
#[must_use]
pub fn scope_for_files(workflow: &Workflow, files: &[ChangedFile]) -> CiScope {
    let Some(f) = files.iter().find(|f| f.path == CI_WORKFLOW) else {
        return CiScope::Unscoped;
    };
    // An add/remove/rename is not a within-file edit, so line attribution
    // against the tip's map is meaningless.
    if f.status != "modified" {
        return CiScope::Unscoped;
    }
    let Some(patch) = f.patch.as_deref() else {
        return CiScope::Unscoped;
    };
    scope_for_patch(workflow, patch)
}

/// The components whose `ci.yml` definition this patch edited, or
/// [`CiScope::Unscoped`] whenever that cannot be established — see the module
/// header for the full fail-closed list.
#[must_use]
pub fn scope_for_patch(workflow: &Workflow, patch: &str) -> CiScope {
    if has_structural_deletion(patch) {
        return CiScope::Unscoped;
    }
    let Some(lines) = changed_new_lines(patch, workflow.line_count) else {
        return CiScope::Unscoped;
    };
    let mut touched = Touched::default();
    for line in lines {
        match workflow.owner(line) {
            Owner::Preamble => touched.preamble = true,
            Owner::JobSetup(key) => {
                touched.jobs.insert(key);
            }
            Owner::Component(key, name) => {
                touched.components.insert((key, name));
            }
        }
    }
    if touched.preamble {
        return CiScope::Unscoped;
    }
    match affected_components(workflow, &touched) {
        Some(set) => CiScope::Scoped(set),
        None => CiScope::Unscoped,
    }
}

/// Which of [`REQUIRED_CHECKS`]'s components the touched blocks reach.
/// `None` = the tip workflow and [`REQUIRED_CHECKS`] do not describe the same
/// set of gates, or a component could not be located — both of which fail
/// closed.
fn affected_components(workflow: &Workflow, touched: &Touched) -> Option<BTreeSet<String>> {
    let mut affected = BTreeSet::new();
    for req in REQUIRED_CHECKS {
        let job = workflow.job_named(req.context)?;
        markers_agree(job, req.components)?;
        let closure = workflow.needs_closure(&job.key)?;
        for component in req.components {
            let hit = touched.jobs.iter().any(|k| closure.contains(k))
                || touched.components.iter().any(|(k, name)| {
                    // Inside the component's OWN job, only its own block
                    // counts; inside any job it merely depends on, every
                    // block does.
                    if *k == job.key {
                        name == component
                    } else {
                        closure.contains(k)
                    }
                });
            if hit {
                affected.insert((*component).to_string());
            }
        }
    }
    Some(affected)
}

/// The invariant that makes per-component attribution safe: the tip's
/// `# component:` markers inside a required job must name **exactly** the
/// components [`REQUIRED_CHECKS`] says that job runs.
///
/// Without this, a gate the running binary's table does not know about — one
/// `main` added to a required job after this daemon was built, or one whose
/// marker was renamed — would be attributed to nobody, and its `ci.yml` edit
/// would narrow away to "affects no component" while a *new required gate* had
/// in fact never run against the PR's tree. That is the one error direction
/// this guard may not take, so a disagreement returns `None` and restores the
/// whole-file meaning.
///
/// A job carrying **no** markers is a single-gate job: its whole body is that
/// one component's, and the table must list exactly it.
fn markers_agree(job: &Job, components: &[&str]) -> Option<()> {
    let declared: BTreeSet<&str> = job.components.iter().map(|c| c.name.as_str()).collect();
    let listed: BTreeSet<&str> = components.iter().copied().collect();
    let ok = if declared.is_empty() {
        listed.len() == 1 && job.names.iter().any(|n| listed.contains(n.as_str()))
    } else {
        declared == listed
    };
    ok.then_some(())
}

/// Does this patch delete a line that defines the file's structure — a
/// top-level key, a job key, or a `# component:` marker? Such a deletion
/// cannot be attributed against the tip's map (the thing it removed is not in
/// it), so the whole answer falls back to [`CiScope::Unscoped`].
fn has_structural_deletion(patch: &str) -> bool {
    patch.lines().any(|line| {
        if line.starts_with("---") || !line.starts_with('-') {
            return false;
        }
        is_structural(&line[1..])
    })
}

fn is_structural(content: &str) -> bool {
    let trimmed = content.trim_start();
    if trimmed.starts_with("# component:") {
        return true;
    }
    let indent = content.len() - trimmed.len();
    // Indent 0 = a top-level key (`jobs:`, `env:`); indent 2 = a job key.
    (indent == 0 || indent == 2)
        && trimmed.ends_with(':')
        && !trimmed.starts_with('#')
        && trimmed
            .trim_end_matches(':')
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !trimmed.trim_end_matches(':').is_empty()
}

/// The new-side line numbers a unified diff changes: every added line, and —
/// for a deletion, which has no new-side line of its own — the lines on either
/// side of the gap it left. `None` when a header will not parse, a body line
/// carries an unexpected marker, an added line lands past `line_count` (the
/// patch and the fetched tip do not describe the same file), or nothing at all
/// was changed.
fn changed_new_lines(patch: &str, line_count: usize) -> Option<Vec<usize>> {
    let mut out: Vec<usize> = Vec::new();
    let mut cursor: Option<usize> = None;
    for line in patch.lines() {
        if line.starts_with("@@") {
            cursor = Some(hunk_new_start(line)?);
            continue;
        }
        if line.starts_with("---") || line.starts_with("+++") || line.starts_with("diff ") {
            continue;
        }
        let Some(pos) = cursor else {
            continue; // preamble before the first hunk header
        };
        if line.starts_with('\\') {
            continue; // "\ No newline at end of file"
        }
        if line.starts_with('+') {
            if pos > line_count {
                return None;
            }
            out.push(pos);
            cursor = Some(pos + 1);
        } else if line.starts_with('-') {
            // The removed text sat between the previous and the current
            // new-side line; charge both, so a deletion at a block boundary
            // cannot be attributed to only one side of it. A deletion past the
            // last line (the file's tail) charges the last line.
            out.push(pos.min(line_count).max(1));
            if pos > 1 {
                out.push((pos - 1).min(line_count));
            }
        } else if line.starts_with(' ') || line.is_empty() {
            cursor = Some(pos + 1);
        } else {
            return None; // an unrecognised marker: do not guess
        }
    }
    (!out.is_empty() && line_count > 0).then_some(out)
}

/// The `+c` of a `@@ -a,b +c,d @@` header.
fn hunk_new_start(header: &str) -> Option<usize> {
    let rest = header.split('+').nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let start: usize = digits.parse().ok()?;
    // `+0,0` means "the new file has nothing here"; there is no line to own.
    (start > 0).then_some(start)
}

#[cfg(test)]
mod tests;
