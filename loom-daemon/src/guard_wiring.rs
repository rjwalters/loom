//! The `PreToolUse` matcher-coverage contract for the `mcp__loom__.*` guard
//! (Issue #9108).
//!
//! # The question this answers
//!
//! `scripts/check-guard-scan-contracts.sh` asserts *"does the guard read the
//! right copy of the command?"*. It says nothing about the prior question:
//! **"is the guard on the tool call's path at all?"** Until #9108 the answer
//! for one whole tool class was no. `.claude/settings.json` wired exactly three
//! `PreToolUse` matchers — `Bash` twice and `Edit|Write` — so every
//! `mcp__loom__*` tool call bypassed the guard surface entirely, while
//! `get_agent_metrics` turned its raw MCP arguments into a shell command line
//! (fixed server-side in #9107). The failure was invisible for the same reason
//! #7755's was: nothing asserted the wiring, and **a matcher that matches
//! nothing does not error — it simply never fires.**
//!
//! # Why this is Rust and not four more `case` statements in that script
//!
//! `scripts/check-guard-scan-contracts.sh` is `contract`-category shell. Epic
//! #7810's `shell-budget` gate ratchets that pool DOWN, never up, and
//! `.loom/docs/shell-language-policy.md` is unconditional about the remedy:
//! new executable logic is a `loom-daemon` subcommand, not more portable
//! shell. This module is that subcommand's logic — the same move
//! [`crate::points_marker`] made for #9056's points check, and the same move
//! this issue's own guard *decision* ([`crate::mcp_tool_guard`]) made from the
//! start.
//!
//! # What it asserts
//!
//! Two things no test could otherwise see:
//!
//! 1. the `mcp__loom__.*` matcher exists in this repo's own
//!    `.claude/settings.json` **and** in `scripts/install/provision-hooks.sh`'s
//!    `_PHOOK_*` wiring set — the repo's own file covers THIS checkout; the
//!    installer's arrays are what every fresh consumer gets, and one without
//!    the other is a hole nobody would notice here;
//! 2. the entry carries the same **fail-closed floor** the `Bash` /
//!    `Edit|Write` entries carry: it routes through `hook-wiring.sh`, and its
//!    inline fallback denies rather than allows when the hook file is absent
//!    from a `.loom/hooks`-bearing workspace, with `LOOM_GUARD_WIRING_FAILOPEN`
//!    as the only way past it.
//!
//! Plus (4) the hook file the wiring names must actually exist at its source of
//! truth, because `hook-wiring.sh`'s rung 5 turns a missing file into a DENY of
//! every MCP call in every workspace carrying a `.loom/hooks/` directory.
//!
//! # Deliberately a text-level assertion, not a JSON query
//!
//! The fail-closed floor being asserted is a property of the emitted **command
//! string** — what a reviewer reads, and what breaks when someone "simplifies"
//! the wrapper. Parsing the settings file into a JSON tree and then re-reading
//! that one string would add a failure mode (a settings file this repo's own
//! tooling cannot parse) without making the assertion sharper.

use std::path::Path;

/// The `PreToolUse` matcher that selects the MCP namespace. A **wildcard**, not
/// an enumerated tool list: mcp-loom's tool set is discovered at runtime, so an
/// enumerated matcher would silently stop covering a tool added later.
pub const MCP_MATCHER: &str = "mcp__loom__.*";
/// The hook file name the wiring must resolve.
pub const MCP_GUARD: &str = "guard-mcp-tools.sh";
/// The launcher whose rung ladder owns the absent / lost-`+x` /
/// machine-level-fallback cases.
const WIRING_LAUNCHER: &str = "hook-wiring.sh";
/// The exact argument list the launcher is handed. Asserted separately from
/// [`WIRING_LAUNCHER`] because the launcher path and its arguments are
/// separated by the wrapper's own `$L` indirection.
const WIRING_ARGS: &str = "PreToolUse guard-mcp-tools.sh";
/// The fail-closed floor, as three independent properties of the SAME command:
/// the workspace gate, the deny document, and the single sanctioned escape
/// hatch.
const FLOOR_PROPERTIES: &[&str] = &[
    ".loom/hooks",
    "permissionDecision",
    "LOOM_GUARD_WIRING_FAILOPEN",
];

/// Repo-relative path of the settings file whose matchers cover THIS checkout.
const SETTINGS_PATH: &str = ".claude/settings.json";
/// Repo-relative path of the installer whose `_PHOOK_*` arrays cover every
/// fresh consumer install.
const PROVISION_PATH: &str = "scripts/install/provision-hooks.sh";
/// Repo-relative path of the hook file's source of truth.
const GUARD_SOURCE_DIR: &str = "defaults/hooks";

/// One contract violation: a short machine-greppable headline plus the
/// explanation a reader needs to act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Stable, all-caps headline (e.g. `MISSING MCP MATCHER`). Kept stable so
    /// a test — or an operator's `grep` — can pin *which* property failed
    /// rather than matching prose.
    pub headline: String,
    /// The rest of the message: why it matters and what to do.
    pub detail: String,
}

impl Violation {
    fn new(headline: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            headline: headline.into(),
            detail: detail.into(),
        }
    }

    /// The full message as printed, headline first.
    #[must_use]
    pub fn render(&self) -> String {
        format!("{}\n{}", self.headline, self.detail)
    }
}

/// Check `root`'s MCP guard wiring. An empty result means the contract holds.
///
/// Never returns `Err`: every failure mode here is itself a violation to
/// report, and a checker that aborts on an unreadable file would report
/// "no violations" for the tree it could not read.
#[must_use]
pub fn check(root: &Path) -> Vec<Violation> {
    let mut out = Vec::new();

    // A repo that opted out of the guards wholesale (`guards.enabled:false`,
    // #10335) has intentionally no guard wiring — nothing to audit.
    if crate::config_resolver::guards_master_disabled(root) {
        return out;
    }

    let settings_path = root.join(SETTINGS_PATH);
    let Ok(settings) = std::fs::read_to_string(&settings_path) else {
        out.push(Violation::new(
            "MISSING SETTINGS FILE",
            format!(
                "{}: no readable settings file — cannot verify the \"{MCP_MATCHER}\" PreToolUse matcher exists (#9108).",
                settings_path.display()
            ),
        ));
        return out;
    };

    check_matcher(&settings, &settings_path.display().to_string(), &mut out);
    check_entry(&settings, &settings_path.display().to_string(), &mut out);
    check_installer(root, &mut out);
    check_guard_file(root, &mut out);

    out
}

/// (1) the matcher itself.
fn check_matcher(settings: &str, where_: &str, out: &mut Vec<Violation>) {
    // Quoted, so a matcher named in a comment or in a longer string cannot
    // satisfy it — the settings file spells it as a JSON string value.
    if settings.contains(&format!("\"{MCP_MATCHER}\"")) {
        return;
    }
    out.push(Violation::new(
        "MISSING MCP MATCHER",
        format!(
            "{where_} has no PreToolUse entry with matcher \"{MCP_MATCHER}\" (#9108).\n\
             \x20   Every mcp__loom__* tool call then runs with NO guard hook on its path, which is\n\
             \x20   the exact gap #9108 closed: mcp-loom is registered at user scope and callable\n\
             \x20   from every agent Loom spawns, and get_agent_metrics built a shell command line\n\
             \x20   from its raw arguments until #9107. A matcher that is absent does not error — it\n\
             \x20   just never fires, which is why this is asserted rather than tested."
        ),
    ));
}

/// (2) an entry that actually runs THIS guard, through `hook-wiring.sh`, with
/// the fail-closed floor intact.
fn check_entry(settings: &str, where_: &str, out: &mut Vec<Violation>) {
    // Anchored on `"command"` rather than on `PreToolUse`: an entry that
    // BYPASSES hook-wiring.sh has no `PreToolUse` argument in it at all, and
    // filtering on that would misreport the bypass as "no command wired".
    // Every hook command in this file is a single physical line.
    let Some(entry) = settings
        .lines()
        .find(|l| l.contains(MCP_GUARD) && l.contains("\"command\""))
    else {
        out.push(Violation::new(
            "MISSING MCP GUARD COMMAND",
            format!("{where_} wires no PreToolUse command for {MCP_GUARD} (#9108)."),
        ));
        return;
    };

    if !entry.contains(WIRING_LAUNCHER) || !entry.contains(WIRING_ARGS) {
        out.push(Violation::new(
            "MCP ENTRY BYPASSES hook-wiring.sh",
            format!(
                "{where_}'s {MCP_GUARD} command does not route through '{WIRING_LAUNCHER} {WIRING_ARGS}' (#9108).\n\
                 \x20   The launcher owns the absent / lost-+x / machine-level-fallback rungs; an\n\
                 \x20   entry that execs the guard directly loses all of them."
            ),
        ));
    }

    for prop in FLOOR_PROPERTIES {
        if entry.contains(prop) {
            continue;
        }
        out.push(Violation::new(
            format!("MCP FAIL-CLOSED FLOOR MISSING ('{prop}')"),
            format!(
                "{where_}'s {MCP_GUARD} command does not carry the same broken-install floor the Bash / Edit|Write entries carry (#9108/#7761).\n\
                 \x20   Required, all three: the '{}' workspace gate, a\n\
                 \x20   '{}' deny document for a .loom/hooks-bearing workspace whose\n\
                 \x20   copy is absent, and '{}' as the only way past it. A\n\
                 \x20   missing guard is a broken install, not an opt-out.",
                FLOOR_PROPERTIES[0], FLOOR_PROPERTIES[1], FLOOR_PROPERTIES[2]
            ),
        ));
    }
}

/// (3) the installer's own wiring set. This repo's settings file covers THIS
/// checkout only; `_PHOOK_MATCHERS` / `_PHOOK_NAMES` are what a fresh consumer
/// gets, and one without the other is a hole nobody would notice here.
fn check_installer(root: &Path, out: &mut Vec<Violation>) {
    let path = root.join(PROVISION_PATH);
    let Ok(text) = std::fs::read_to_string(&path) else {
        // Absent installer: not this contract's business (a consumer checkout
        // carries no scripts/install/). Same posture as the shell original.
        return;
    };
    let where_ = path.display().to_string();
    if !text.contains(MCP_MATCHER) {
        out.push(Violation::new(
            "MISSING MCP MATCHER IN INSTALLER",
            format!(
                "{where_}'s _PHOOK_MATCHERS does not include \"{MCP_MATCHER}\" (#9108) — this repo would be guarded but every fresh install would not."
            ),
        ));
    }
    if !text.contains(MCP_GUARD) {
        out.push(Violation::new(
            "MISSING MCP GUARD IN INSTALLER",
            format!("{where_}'s _PHOOK_NAMES does not include {MCP_GUARD} (#9108)."),
        ));
    }
}

/// (4) the hook file the wiring names must exist at its source of truth.
fn check_guard_file(root: &Path, out: &mut Vec<Violation>) {
    let path = root.join(GUARD_SOURCE_DIR).join(MCP_GUARD);
    if path.is_file() {
        return;
    }
    out.push(Violation::new(
        "MISSING MCP GUARD FILE",
        format!(
            "{} does not exist, but the wiring names it (#9108) — every workspace with a .loom/hooks/ directory would DENY every MCP tool call via hook-wiring.sh rung 5.",
            path.display()
        ),
    ));
}

/// The one-line success message, printed when [`check`] returns empty.
#[must_use]
pub fn ok_message() -> String {
    format!(
        "check-guard-wiring: OK — the {MCP_MATCHER} PreToolUse matcher is wired in both {SETTINGS_PATH} and the installer, and carries the same fail-closed broken-install floor as the Bash / Edit|Write entries."
    )
}

/// The trailer printed after the violations, explaining the stakes once.
#[must_use]
pub fn failure_trailer() -> String {
    "\ncheck-guard-wiring: FAIL — see above. MCP tool calls would run with\n\
     no guard hook on their path, which is the #9108 gap: mcp-loom is registered at\n\
     user scope and callable from every agent Loom spawns, and get_agent_metrics\n\
     built a shell command line from its raw arguments until #9107. A PreToolUse\n\
     matcher that is missing does not error — it just never fires.\n\n\
     Catalog entry and the category's toggle: defaults/docs/guard-hooks.md"
        .to_string()
}

#[cfg(test)]
mod tests;
