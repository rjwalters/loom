//! `fleet.yml`, the store's one hand-edited file, and its renders, for a
//! proposal (#10905).
//!
//! fleet-gitops compiles `fleet.yml` into `fleet.json` and the legacy files
//! with its own `scripts/render.py`, and its `validate` check fails
//! (`fleet-stale`) when a committed render differs from what `fleet.yml`
//! renders to. So a proposal edits `fleet.yml` ([`load`]) and then runs that
//! renderer on the edit ([`render`]), so the PR carries the edited source and
//! every render it changes, and passes `validate`.
//!
//! The renderer runs from a scratch directory holding the edited `fleet.yml`
//! and the store's `scripts/` and `schema/` at the proposal's base commit:
//! the store's own code, at the commit the operator's merge policy already
//! governs, never anything from this host. It needs `python3` with PyYAML.
//! Without them, or when the store has no `scripts/render.py`, the proposal
//! carries `fleet.yml` alone and says the renders still have to be
//! regenerated ([`Rendered::Skipped`]). A renderer that refuses the edit is
//! an error, and nothing is proposed.
//!
//! A render never removes a file here: no `propose` edit drops a host's
//! config tier, the only thing whose render a render removes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use super::FileChange;
use crate::fleet_store::fetch::{fetch_blob, list_tree, Snapshot, Transport};

/// Store-relative path of the hand-edited fleet model.
pub const SOURCE_PATH: &str = "fleet.yml";

/// Store-relative path of the store's renderer.
pub const RENDERER: &str = "scripts/render.py";

/// The directories the renderer reads besides `fleet.yml`.
const RENDERER_DIRS: [&str; 2] = ["scripts/", "schema/"];

/// The interpreter the renderer runs under.
const PYTHON: &str = "python3";

/// The store's `fleet.yml` at a proposal's base commit.
#[derive(Debug, Clone)]
pub struct Source {
    /// The store, `OWNER/REPO`.
    pub repo: String,
    /// Every file in the tree at the base commit: path → blob SHA.
    pub tree: BTreeMap<String, String>,
    /// `fleet.yml`'s text.
    pub text: String,
    /// `fleet.yml`'s blob SHA.
    pub sha: String,
}

impl Source {
    /// The [`FileChange`] that writes `after` over `fleet.yml`.
    #[must_use]
    pub fn change(&self, after: String) -> FileChange {
        FileChange {
            path: SOURCE_PATH.to_string(),
            before: Some(self.text.clone()),
            before_sha: Some(self.sha.clone()),
            after,
        }
    }

    fn text_of(&self, transport: &dyn Transport, path: &str) -> Result<Option<String>> {
        let Some(sha) = self.tree.get(path) else {
            return Ok(None);
        };
        let bytes =
            fetch_blob(transport, &self.repo, sha).with_context(|| format!("fetching {path}"))?;
        String::from_utf8(bytes)
            .map(Some)
            .with_context(|| format!("{path} is not UTF-8"))
    }
}

/// `fleet.yml` at `snapshot`'s commit. A store without one cannot take a
/// proposal: its renders are what `propose` would otherwise have to edit.
pub fn load(transport: &dyn Transport, repo: &str, snapshot: &Snapshot) -> Result<Source> {
    let commit = &snapshot.manifest.commit;
    let tree = list_tree(transport, repo, commit)?;
    let Some(sha) = tree.get(SOURCE_PATH).cloned() else {
        bail!(
            "the store has no {SOURCE_PATH} (commit {}): `propose` edits fleet.yml, the source \
             the store's fleet.json and legacy files are rendered from",
            snapshot.short_commit()
        );
    };
    let mut source = Source {
        repo: repo.to_string(),
        tree,
        text: String::new(),
        sha,
    };
    source.text = source
        .text_of(transport, SOURCE_PATH)?
        .ok_or_else(|| anyhow!("{SOURCE_PATH} vanished from the tree"))?;
    Ok(source)
}

/// What [`render`] did.
#[derive(Debug)]
pub enum Rendered {
    /// The renders the edit changes (possibly none).
    Files(Vec<FileChange>),
    /// The renderer could not run here; why.
    Skipped(String),
}

/// Run the store's renderer on `after` (the edited `fleet.yml`) and return
/// every render that differs from the store's at the base commit.
pub fn render(
    transport: &dyn Transport,
    source: &Source,
    snapshot: &Snapshot,
    after: &str,
) -> Result<Rendered> {
    render_with(transport, source, snapshot, after, PYTHON, "yaml")
}

/// Whether `python` runs and can import `module` (the renderer's one
/// dependency, PyYAML's `yaml`).
fn python_ready(python: &str, module: &str) -> bool {
    Command::new(python)
        .args(["-c", &format!("import {module}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn render_with(
    transport: &dyn Transport,
    source: &Source,
    snapshot: &Snapshot,
    after: &str,
    python: &str,
    module: &str,
) -> Result<Rendered> {
    if !source.tree.contains_key(RENDERER) {
        return Ok(Rendered::Skipped(format!("the store has no {RENDERER}")));
    }
    if !python_ready(python, module) {
        return Ok(Rendered::Skipped(format!(
            "`{python}` with PyYAML is not available on this host"
        )));
    }
    let scratch = tempfile::tempdir().context("creating a scratch directory for the render")?;
    let root = scratch.path();
    let mut written = BTreeSet::from([SOURCE_PATH.to_string()]);
    write_file(root, SOURCE_PATH, after.as_bytes())?;
    for (path, sha) in &source.tree {
        if RENDERER_DIRS.iter().any(|d| path.starts_with(d)) {
            let body = fetch_blob(transport, &source.repo, sha)
                .with_context(|| format!("fetching {path}"))?;
            write_file(root, path, &body)?;
            written.insert(path.clone());
        }
    }
    let out = Command::new(python)
        .arg("-B")
        .arg(root.join(RENDERER))
        .arg("--root")
        .arg(root)
        .current_dir(root)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .with_context(|| format!("running {RENDERER}"))?;
    if !out.status.success() {
        bail!(
            "the store's {RENDERER} refused the edited {SOURCE_PATH}:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let mut changes = Vec::new();
    for path in files_under(root)? {
        if written.contains(&path) {
            continue;
        }
        let rendered = std::fs::read_to_string(root.join(&path))
            .with_context(|| format!("reading the render of {path}"))?;
        let before = match snapshot.text(&path)? {
            Some(t) => Some(t),
            None => source.text_of(transport, &path)?,
        };
        if before.as_deref() != Some(rendered.as_str()) {
            changes.push(FileChange {
                before_sha: source.tree.get(&path).cloned(),
                path,
                before,
                after: rendered,
            });
        }
    }
    Ok(Rendered::Files(changes))
}

/// Write a store file under `root`, refusing a path that could leave it.
fn write_file(root: &Path, rel: &str, body: &[u8]) -> Result<()> {
    let safe = !rel.is_empty()
        && rel
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    if !safe {
        bail!("refusing the store path `{rel}`");
    }
    let path = root.join(rel);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))
}

/// Every file under `root`, as `/`-separated relative paths, `__pycache__`
/// skipped.
fn files_under(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if entry.file_name() != "__pycache__" {
                    stack.push(path);
                }
            } else if kind.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .context("a render outside the scratch directory")?;
                let parts: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect();
                out.push(parts.join("/"));
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
#[path = "tests/source_tests.rs"]
mod tests;
