//! `worktree.sh`'s shared-artifact symlink provisioning (#8195 slice 4, epic
//! #7810) — the post-`git worktree add` step that links the main workspace's
//! gitignored, expensive-to-rebuild artifacts into a fresh worktree and
//! records each one in that worktree's `info/exclude`.
//!
//! # What moved here
//!
//! Four link families and the exclude bookkeeping that binds them, in the
//! shell's own order (the order is observable — it is the order the lines
//! print in):
//!
//! 1. Root `node_modules` (the 30–60s `pnpm install` this saves per worktree).
//! 2. Nested per-package `node_modules` for pnpm/monorepo layouts, discovered
//!    by directory scan rather than by parsing a workspace manifest (#3528 —
//!    no YAML parser dependency).
//! 3. `worktree.linkPaths` from the resolved config tier chain (#4062), e.g.
//!    generated `wasm-pack` bindings.
//! 4. `.mcp.json`, which is gitignored and therefore invisible from a worktree
//!    git root, so Claude Code cannot discover MCP servers without it.
//!
//! Every one of them appends its path to the worktree's `info/exclude`
//! (#5474), because a `.gitignore` rule written as `node_modules/` matches
//! *directories* and not the symlink this creates — so without the exclude
//! entry, `git add -A` stages it and `git status` is never clean.
//!
//! # Why this slice, and why it is safe to take now
//!
//! The create path is `worktree.sh`'s largest unported region and it is under
//! active concurrent repair (#8351 on the stale-reset arm, #8702 on the
//! claim-lock pre-flight, #8486 on the post-add cargo target dir). This family
//! is the one cohesive unit inside it that touches none of those: it is a pure
//! function of (main workspace, worktree path, resolved config), it runs after
//! every branch/lock decision has already been made, and it creates only
//! symlinks — no `rm -rf`, no `git branch -D`, no forge call.
//!
//! It is still squarely in the issue's stated danger class, for a reason that
//! is easy to miss: **it is the part of the create path that is all path
//! interpolation.** `find … -print0`, a `${var#"$prefix"/}` prefix strip, four
//! `ln -s "$src" "$dst"` pairs and a `grep -qxF "$entry" "$file"` — the exact
//! construct family whose one miss (#7858) turned an orphan guard into an
//! `rm -rf` on a live worktree. In Rust a path is an `OsString`, `symlink`
//! takes two of them, and there is no word splitting to get wrong. That is
//! pinned in two places rather than asserted once: this module's
//! `run_provisions_every_family_through_paths_containing_spaces`, where every
//! path in play (workspace, worktree, package dir, `linkPaths` entry) carries
//! a space and one carries `$(touch pwned)`; and
//! `tests/worktree_link_differential.rs`, whose corpus replays the same shapes
//! through the RETIRED shell as well, so "the port is safe" and "the port
//! still agrees" are separate claims with separate evidence.
//!
//! # Exit code
//!
//! **Always 0.** Argued, not defaulted: this step is best-effort by contract —
//! `worktree.sh`'s own help text says "a failed link warns and continues; it
//! never aborts worktree creation" — and every failure mode here (no
//! `node_modules` to link, a pre-existing destination, an unwritable exclude
//! file) leaves a *usable* worktree that merely rebuilds something. There is
//! no answer a caller branches on, so there is no code to reserve for one, and
//! a non-zero code would be read by the `set -e` script above as a reason to
//! abandon a worktree it has already successfully created. The caller keeps
//! its own `|| true` regardless, so the two agree even if this ever changes.
//!
//! # Behaviour deliberately preserved from the shell
//!
//! - `linkPaths` entries are concatenated onto the workspace root as *strings*
//!   (`"$MAIN_WORKSPACE_DIR/$link_path"`), not joined as paths. An absolute
//!   entry therefore yields `/main/workspace//etc/passwd` and finds nothing,
//!   where `Path::join` would have silently resolved it to `/etc/passwd` and
//!   linked a file from outside the workspace. Preserved on purpose, and
//!   pinned by a corpus case.
//! - An `info/exclude` entry is appended verbatim with a trailing newline and
//!   no preceding separator, exactly as `echo "$entry" >>` did.
//! - The membership test is byte-exact whole-line matching (`grep -qxF`), so a
//!   non-UTF-8 exclude file still de-duplicates correctly.
//!
//! # The one divergence
//!
//! The shell gated the whole `linkPaths` family on `command -v jq`. There is
//! no `jq` here, so the family now runs on a host without it. That is a
//! retirement, not a regression — see `test-worktree-nested-symlinks.sh`'s
//! Test 5, where the retired assertion and its stronger successor are recorded
//! in the suite itself per `verification-recipes.md` §6.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::wip::Out;

/// `find … -maxdepth 3`. The depth a candidate `node_modules` may sit at,
/// counted the way `find` counts it: the scan root is depth 0.
const MAX_SCAN_DEPTH: usize = 3;

/// `find … -mindepth 2`. Depth 1 is the ROOT `node_modules`, already handled
/// by [`link_root_node_modules`]; re-linking it here would double its message.
const MIN_SCAN_DEPTH: usize = 2;

/// What to provision, and where.
pub struct Options {
    /// The main workspace root — `git rev-parse --show-toplevel` in the
    /// shell, resolved there and passed in rather than re-derived, because by
    /// this point the script has already auto-navigated out of any worktree
    /// and its answer is the one the rest of the create path used.
    pub repo_root: PathBuf,
    /// The absolute path of the worktree that was just created.
    pub worktree: PathBuf,
    /// Print nothing at all.
    ///
    /// Mirrors the shell's `if [[ "$JSON_OUTPUT" != "true" ]]` guard around
    /// every one of these lines — NOT `Out`'s stderr routing. In `--json` mode
    /// `worktree.sh` has already pointed fd 1 at stderr, so routing rather
    /// than suppressing would still be invisible on stdout, but it would add
    /// lines to stderr that the pre-port script never emitted.
    pub quiet: bool,
}

/// Provision every link family. See the module docs for why this cannot fail.
pub fn run(opts: &Options) -> i32 {
    let out = Reporter {
        out: Out::new(false),
        quiet: opts.quiet,
    };
    let exclude = ExcludeFile::resolve(&opts.worktree);

    link_root_node_modules(opts, &exclude, &out);
    link_nested_node_modules(opts, &exclude, &out);
    link_configured_paths(opts, &exclude, &out);
    link_mcp_json(opts, &exclude, &out);

    0
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// [`Out`] plus the shell's all-or-nothing `--json` suppression.
struct Reporter {
    out: Out,
    quiet: bool,
}

impl Reporter {
    fn info(&self, msg: &str) {
        if !self.quiet {
            self.out.info(msg);
        }
    }
    fn success(&self, msg: &str) {
        if !self.quiet {
            self.out.success(msg);
        }
    }
    fn warning(&self, msg: &str) {
        if !self.quiet {
            self.out.warning(msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Path predicates — the bash test operators, spelled out
// ---------------------------------------------------------------------------
//
// All three FOLLOW symlinks, because `[[ -d ]]` / `[[ -f ]]` / `[[ -e ]]` do.
// That is load-bearing in both directions: a dangling symlink at a destination
// makes `-e` FALSE, so the shell went on to `ln -s` and failed, warning rather
// than silently leaving the dangle in place. Preserved.

fn is_dir(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

fn is_file(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

fn exists(path: &Path) -> bool {
    fs::metadata(path).is_ok()
}

/// `"$base/$rel"` — string concatenation, NOT [`Path::join`].
///
/// The difference only shows up when `rel` is absolute, and then it is the
/// whole ballgame: `join` discards `base` and returns `rel`, which would let a
/// `worktree.linkPaths` entry of `/etc/ssh` reach outside the workspace. The
/// shell could not do that, so neither does this.
fn concat(base: &Path, rel: &OsStr) -> PathBuf {
    let mut joined = base.as_os_str().to_os_string();
    joined.push("/");
    joined.push(rel);
    PathBuf::from(joined)
}

// ---------------------------------------------------------------------------
// info/exclude
// ---------------------------------------------------------------------------

/// The worktree's `info/exclude`, resolved once and appended to idempotently.
///
/// `git rev-parse --git-path info/exclude` is asked rather than a path
/// hardcoded: `info/exclude` is a COMMON-dir path, so a worktree inherits the
/// main repo's file, and asking git keeps this correct across git layouts
/// (including the `.git`-file indirection a worktree uses).
struct ExcludeFile {
    /// `None` means "git had no answer" — the shell's empty-string case, where
    /// `_append_worktree_exclude` returned immediately. Every caller still
    /// creates its symlink; only the exclude entry is skipped.
    path: Option<PathBuf>,
}

impl ExcludeFile {
    fn resolve(worktree: &Path) -> Self {
        // `cd "$ABS_WORKTREE_PATH" 2>/dev/null && git rev-parse …` — a failed
        // `cd` yields an empty answer, not an error.
        if !is_dir(worktree) {
            return Self { path: None };
        }
        let output = Command::new("git")
            .current_dir(worktree)
            .args(["rev-parse", "--git-path", "info/exclude"])
            .output();
        let Ok(output) = output else {
            return Self { path: None };
        };
        if !output.status.success() {
            return Self { path: None };
        }
        // `$(…)` strips trailing newlines and nothing else.
        let mut raw = output.stdout;
        while raw.last() == Some(&b'\n') {
            raw.pop();
        }
        if raw.is_empty() {
            return Self { path: None };
        }
        let answer = PathBuf::from(OsString::from_vec(raw));
        // git may answer relative to the cwd it was run in; anchor it there.
        let path = if answer.is_absolute() {
            answer
        } else {
            concat(worktree, answer.as_os_str())
        };
        Self { path: Some(path) }
    }

    /// Append `entry` unless a byte-identical whole line is already present.
    ///
    /// Best-effort in every failure mode, matching the shell's trailing
    /// `|| true`: a missing or unwritable exclude file means git is tracking
    /// the ignore somewhere else, which is not this step's problem.
    fn append(&self, entry: &OsStr) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let wanted = entry.as_bytes();
        // `grep -qxF` — whole-line, fixed-string, byte-exact. Reading bytes
        // rather than a `String` keeps that true for a non-UTF-8 exclude file,
        // where a lossy read would fail to match and append a duplicate.
        if let Ok(existing) = fs::read(path) {
            if existing.split(|b| *b == b'\n').any(|line| line == wanted) {
                return;
            }
        }
        // `echo "$entry" >> "$file"`: append, create if absent, no attempt to
        // insert a separator first.
        if let Ok(mut handle) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = handle.write_all(wanted);
            let _ = handle.write_all(b"\n");
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Root node_modules
// ---------------------------------------------------------------------------

fn link_root_node_modules(opts: &Options, exclude: &ExcludeFile, out: &Reporter) {
    let main_node_modules = opts.repo_root.join("node_modules");
    let worktree_node_modules = opts.worktree.join("node_modules");
    let worktree_package_json = opts.worktree.join("package.json");

    if !(is_dir(&main_node_modules)
        && is_file(&worktree_package_json)
        && !exists(&worktree_node_modules))
    {
        return;
    }

    out.info("Symlinking node_modules from main workspace...");
    if symlink(&main_node_modules, &worktree_node_modules).is_ok() {
        exclude.append(OsStr::new("node_modules"));
        out.success("node_modules symlinked (skipping pnpm install)");
    } else {
        out.warning("Could not symlink node_modules (will install on first build)");
    }
}

// ---------------------------------------------------------------------------
// 2. Nested per-package node_modules
// ---------------------------------------------------------------------------

fn link_nested_node_modules(opts: &Options, exclude: &ExcludeFile, out: &Reporter) {
    // The shell gated the whole scan on the ROOT node_modules existing, which
    // is not obviously necessary (a monorepo could in principle have per-
    // package installs and no root one) but is the behaviour, so it stays.
    if !is_dir(&opts.repo_root.join("node_modules")) {
        return;
    }

    for package_node_modules in discover_nested_node_modules(&opts.repo_root) {
        let Some(package_dir) = package_node_modules.parent() else {
            continue;
        };
        // `rel_path="${pkg_dir#"$MAIN_WORKSPACE_DIR"/}"`, plus the shell's
        // "skip if the prefix strip did nothing" guard — unreachable here
        // because the scan starts at repo_root, but kept as the same refusal.
        let Ok(rel) = package_dir.strip_prefix(&opts.repo_root) else {
            continue;
        };
        if !is_file(&package_dir.join("package.json")) {
            continue;
        }

        let worktree_package_dir = opts.worktree.join(rel);
        let worktree_package_node_modules = worktree_package_dir.join("node_modules");
        if !(is_dir(&worktree_package_dir) && !exists(&worktree_package_node_modules)) {
            continue;
        }

        let mut entry = rel.as_os_str().to_os_string();
        entry.push("/node_modules");
        let display = entry.to_string_lossy().into_owned();

        if symlink(&package_node_modules, &worktree_package_node_modules).is_ok() {
            exclude.append(&entry);
            out.success(&format!("Symlinked {display} from main workspace"));
        } else {
            out.warning(&format!("Could not symlink {display}"));
        }
    }
}

/// `find "$root" -mindepth 2 -maxdepth 3 -type d -name node_modules -not -path "*/node_modules/*"`.
///
/// Three details of that command are load-bearing and each has a counterpart
/// below:
///
/// - `find` defaults to `-P`, so `-type d` is false for a symlink to a
///   directory and the scan never follows one. [`fs::symlink_metadata`].
/// - `-not -path "*/node_modules/*"` excludes any candidate with a
///   `node_modules` ANCESTOR — `find`'s `*` spans `/`, so the predicate is
///   exactly "the full path contains `/node_modules/`". Tested directly rather
///   than inferred, so a scan root that itself lives under a `node_modules`
///   directory is excluded the same way `find` excluded it.
/// - Not descending into a `node_modules` is a pure optimisation, valid
///   *because* of the previous point: everything below one is excluded anyway.
///   The shell paid for that traversal (`node_modules/.pnpm/**` at depth 3);
///   this does not.
///
/// The result is SORTED, where `find` emitted readdir order. Nothing consumed
/// the order — each iteration is independent — but the messages print in it,
/// and a nondeterministic message order cannot be differentially compared.
fn discover_nested_node_modules(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    scan(root, 1, &mut found);
    found.sort();
    found
}

/// `depth` is the depth of the entries `dir` contains, counted from the scan
/// root (whose own entries are depth 1).
fn scan(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        if entry.file_name() == OsStr::new("node_modules") {
            if depth >= MIN_SCAN_DEPTH && !has_node_modules_ancestor(&path) {
                found.push(path);
            }
            continue;
        }
        scan(&path, depth + 1, found);
    }
}

/// `-not -path "*/node_modules/*"`, applied to the whole path.
fn has_node_modules_ancestor(path: &Path) -> bool {
    let needle = b"/node_modules/";
    path.as_os_str()
        .as_bytes()
        .windows(needle.len())
        .any(|window| window == needle)
}

// ---------------------------------------------------------------------------
// 3. worktree.linkPaths
// ---------------------------------------------------------------------------

fn link_configured_paths(opts: &Options, exclude: &ExcludeFile, out: &Reporter) {
    for link_path in configured_link_paths(&opts.repo_root) {
        // `if [[ -z "$link_path" ]]; then continue; fi`
        if link_path.is_empty() {
            continue;
        }
        let entry = OsString::from(link_path);
        let link_src = concat(&opts.repo_root, &entry);
        let link_dst = concat(&opts.worktree, &entry);
        if !(exists(&link_src) && !exists(&link_dst)) {
            continue;
        }
        if let Some(parent) = link_dst.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let display = entry.to_string_lossy().into_owned();
        if symlink(&link_src, &link_dst).is_ok() {
            exclude.append(&entry);
            out.success(&format!("Symlinked {display} from main workspace"));
        } else {
            out.warning(&format!("Could not symlink {display}"));
        }
    }
}

/// `loom_resolve_config <root> | jq -r '.worktree.linkPaths[]? // empty'`.
///
/// The resolver is the same tier chain the shell used (#4062) — this is
/// [`crate::config_resolver`], already established as `lib/config-resolver.sh`'s
/// Rust twin by [`crate::worktree_root`].
///
/// `jq`'s semantics are reproduced rather than approximated, because the shell
/// fed whatever came out straight into a path:
///
/// - `[]?` iterates an array's elements, an OBJECT's values, and emits nothing
///   for anything else (including a missing key) rather than erroring.
/// - `// empty` drops `null` and `false` outputs — the two values jq considers
///   falsy — and keeps everything else.
/// - `-r` prints a string raw and anything else as its JSON text.
fn configured_link_paths(repo_root: &Path) -> Vec<String> {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(node) = crate::config_resolver::get_path(&effective, "worktree.linkPaths") else {
        return Vec::new();
    };
    let elements: Vec<&Value> = match node {
        Value::Array(items) => items.iter().collect(),
        Value::Object(map) => map.values().collect(),
        _ => return Vec::new(),
    };
    elements
        .into_iter()
        .filter_map(|value| match value {
            Value::Null | Value::Bool(false) => None,
            Value::String(text) => Some(text.clone()),
            other => Some(other.to_string()),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 4. .mcp.json
// ---------------------------------------------------------------------------

fn link_mcp_json(opts: &Options, exclude: &ExcludeFile, out: &Reporter) {
    let main_mcp_json = opts.repo_root.join(".mcp.json");
    let worktree_mcp_json = opts.worktree.join(".mcp.json");

    if !(is_file(&main_mcp_json) && !exists(&worktree_mcp_json)) {
        return;
    }

    out.info("Symlinking .mcp.json from main workspace...");
    if symlink(&main_mcp_json, &worktree_mcp_json).is_ok() {
        exclude.append(OsStr::new(".mcp.json"));
        out.success(".mcp.json symlinked");
    } else {
        out.warning("Could not symlink .mcp.json");
    }
}

#[cfg(test)]
mod tests;
