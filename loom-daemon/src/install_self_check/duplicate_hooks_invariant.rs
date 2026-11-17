//! [`super::Invariant::DuplicateHookWiring`] (#13958): Loom guard hooks are
//! wired at **both** scopes — the user-scope `~/.claude/settings.json`
//! machine-level wrapper (`provision_loom_hooks`) and the project-level
//! `.claude/settings.json` entry (`ensure_project_hook_wiring`, #4401) — and
//! the wrapper's transition defer (#4806) does **not** hold, so both entries
//! fire per tool call instead of exactly one.
//!
//! Canonical scope, by existing design (Epic #3835 Phase 5): **user-scope,
//! machine-level**. A project-level entry exists only as the transition
//! fallback for repos still carrying `.loom/hooks/<name>` copies. Both scopes
//! wired is *expected* during that transition and must NOT warn: the wrapper
//! defers (exits 0) when
//!
//! 1. the root is a Loom workspace (`.loom-project/project.json` or
//!    `.loom/config.json` — the wrapper's workspace gate, which runs *before*
//!    the defer check, so a non-workspace root cannot double-fire at all),
//! 2. the per-repo copy `.loom/hooks/<name>` exists and is executable, and
//! 3. the project settings text contains `.loom/hooks/<name>` (the wrapper
//!    probes the **raw file text**, not parsed JSON — `$(<file)` in
//!    `_phook_cmd`).
//!
//! Only case (2) can drift after wiring, and it is the one genuine double-fire:
//! a project entry whose copy was deleted, made non-executable, or never
//! written while the wrapper stops deferring and execs the machine hook on top
//! of it. Warn-only and **never auto-repairable** — both settings files are
//! operator-owned; the report names the hook, both files, and the exact fix
//! (remove the stale project entry, or restore the executable copy).
//!
//! Detection reads the same substring markers the provisioner dedups on
//! (#4806): `defaults/hooks/<name>` for the machine-level wrapper form and
//! `.loom/hooks/<name>` for the project-level form. A project file that also
//! carries `defaults/hooks/<name>` for that hook is holding a machine-level
//! wrapper, not a project entry — the same exclusion `_phook_merge_one` /
//! `is_wired` apply — so such a hit never counts as project-scope wiring. The
//! hook-name table is single-sourced from [`crate::init::hook_wiring::HOOKS`],
//! which a drift test pins to `provision-hooks.sh`'s `_PHOOK_NAMES`.

use std::path::Path;

use super::InvariantStatus;
use crate::fs_util::is_executable;
use crate::init::hook_wiring::HOOKS;

/// Marker prefix a machine-level (user-scope) wrapper carries.
const MACHINE_MARKER: &str = "defaults/hooks/";
/// Marker prefix a project-level entry carries.
const PROJECT_MARKER: &str = ".loom/hooks/";

/// Live check: reads the real settings files. Skipped in a unit-test build so
/// a developer's own `~/.claude` can never change a test's outcome (the pure
/// half is [`check_with`]).
pub(super) fn check(repo_root: &Path) -> InvariantStatus {
    if cfg!(test) {
        return InvariantStatus::Skipped("unit-test build".to_string());
    }
    let read = |p: &Path| std::fs::read_to_string(p).ok();
    let user = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .and_then(|h| read(&Path::new(&h).join(".claude/settings.json")));
    let project = read(&repo_root.join(".claude/settings.json"));
    check_with(user.as_deref(), project.as_deref(), repo_root)
}

/// The pure decision: `user` / `project` are the raw settings files' contents,
/// `repo_root` supplies the workspace-gate files and the `.loom/hooks/<name>`
/// exec probes (the wrapper resolves its `$ROOT` to the main checkout, which
/// is what `repo_root` is for a registered workspace).
#[must_use]
pub fn check_with(user: Option<&str>, project: Option<&str>, repo_root: &Path) -> InvariantStatus {
    // The wrapper's workspace gate (step 1) exits before its defer check, so
    // on a non-Loom root the user-scope entry can never double-fire.
    if !repo_root.join(".loom-project/project.json").is_file()
        && !repo_root.join(".loom/config.json").is_file()
    {
        return InvariantStatus::Skipped(format!(
            "{} is not a Loom workspace — the user-scope wrapper exits at its workspace gate, \
             so both scopes cannot double-fire",
            repo_root.display()
        ));
    }

    let has = |text: Option<&str>, marker: &str| text.is_some_and(|t| t.contains(marker));

    // Unique hook names (HOOKS wires roll-pause.sh and guard-uncommitted-work.sh
    // several times; dedup/detection is per name, not per wiring entry).
    let mut names: Vec<&str> = HOOKS.iter().map(|(_, _, name, _)| *name).collect();
    names.sort_unstable();
    names.dedup();

    let mut both_wired: Vec<&str> = Vec::new();
    let mut broken: Vec<String> = Vec::new();
    let project_settings = repo_root.join(".claude/settings.json");
    for name in &names {
        // Machine-level form: the user-scope wrapper embeds defaults/hooks/<name>.
        let user_wired = has(user, &format!("{MACHINE_MARKER}{name}"));
        // Project-level form: .loom/hooks/<name>, excluding a machine-level
        // wrapper carried in the project file (its own defer probe embeds the
        // same .loom/hooks/<name> text — the #4806 trap).
        let project_wired = has(project, &format!("{PROJECT_MARKER}{name}"))
            && !has(project, &format!("{MACHINE_MARKER}{name}"));
        if !user_wired || !project_wired {
            continue;
        }
        both_wired.push(name);
        // Defer condition (steps 2/3 of the wrapper): the copy must exist and be
        // executable, and the project settings text must carry the marker. The
        // marker is already established by `project_wired` above (and a file we
        // could read exists), so the only drift-able leg is the executable copy.
        let copy = repo_root.join(".loom").join("hooks").join(name);
        let defers = copy.is_file() && is_executable(&copy);
        if !defers {
            broken.push(format!(
                "  - {name}: wired at both scopes but the user-scope wrapper cannot defer — \
                 {} is missing or not executable, so the machine-level hook AND the \
                 project-level entry both fire.\n    Fix: remove the stale {PROJECT_MARKER}{name} \
                 entry from {} (the canonical scope is the user-scope machine-level wiring; \
                 `loom migrate` retires the project copies), or restore an executable copy at \
                 {PROJECT_MARKER}{name} so the wrapper defers.",
                copy.display(),
                project_settings.display(),
            ));
        }
    }

    if !broken.is_empty() {
        return InvariantStatus::Violation(format!(
            "Loom hooks are wired at both ~/.claude/settings.json and {} with a broken \
             transition defer (each listed hook fires twice per matching tool call):\n{}",
            project_settings.display(),
            broken.join("\n")
        ));
    }
    if both_wired.is_empty() {
        return InvariantStatus::Skipped(
            "no hook is wired at both scopes — duplicate wiring is not possible".to_string(),
        );
    }
    InvariantStatus::Ok
}
