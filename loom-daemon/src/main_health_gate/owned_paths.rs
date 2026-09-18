//! Which repo paths are Loom-owned or regenerable (#8243).
//!
//! Extracted from `main_health_gate.rs` because that file is over
//! `.loom/docs/file-size-policy.md`'s threshold and therefore FROZEN — it may
//! shrink, not grow. #8078 needed to add one entry (`.loom-local/`) to
//! `LOOM_OWNED_PREFIXES` and the gate refused it, which is the policy working
//! as designed: the answer to "this table needs another row" is to give the
//! table its own file, not to shave a line off somewhere else in a
//! 1,800-line module.
//!
//! This is a pure move. Every constant keeps its name, its value and its
//! documentation, so `git log -M` follows it and the reasoning recorded
//! against each entry — much of it incident-derived — travels with the data
//! it explains.

// ============================================================================
// Dirty-tree ignore list (#3778 transient paths + build-artifact lockfiles,
// #3950)
// ============================================================================

/// Loom-owned transient state path prefixes the gate's dirty-tree check
/// ignores when deciding whether the workspace is safe to sync/reset before a
/// run (#3950). Mirrors `.loom/scripts/check-main-clean.sh`'s
/// `LOOM_OWNED_PREFIXES` — kept in sync manually since one lives in bash and
/// the other in Rust, but both exist to solve the same #3778 problem: Loom's
/// own runtime bookkeeping showing up as "dirty" and false-positiving a check
/// that exists to protect *real* operator edits. A prefix ending in `/`
/// matches a directory subtree; the others match an exact path.
pub(super) const LOOM_OWNED_PREFIXES: &[&str] = &[
    ".loom/sweep-checkpoint/",
    ".loom/sweep-run/",
    ".loom/tokens/",
    ".loom/accounts.env",
    ".loom/exit-codes/",
    ".loom/stats/",
    ".loom/CANARY",
    ".loom/spawn-loop.pid",
    ".loom/spawn-loop-state.json",
    ".loom/stop-spawn-loop",
    ".loom/locks/",
    ".loom/logs/",
    ".loom/worktrees/",
    ".loom-managed",
    // Host-local config overlay (#4039; added here for #8075, mirroring the
    // bash list). Ungitted by design, so in a repo whose managed .gitignore
    // block predates #8075 it is untracked dirt that would wedge this gate
    // "dirty" forever — the same failure mode the lockfile class below exists
    // to prevent. Safe to ignore here: the gate's remediation is `git reset
    // --hard <remote>` and never `git clean`, so the overlay survives.
    ".loom-local/",
];

/// Common regenerable lockfile basenames (#3950): a package manager can
/// rewrite one of these with no dependency change (formatting/ordering churn
/// from a `buildGate.command` step like `pnpm install`), leaving tracked-file
/// dirt that would otherwise wedge the dirty-tree check indefinitely — the
/// reported symptom was a lone modified `mcp-loom/package-lock.json`
/// disabling the gate for the whole repo, every tick, forever (a hard reset
/// would have discarded it, but the check refused to run one). Matched by
/// exact basename anywhere in the tree: a small, well-known, documented set,
/// not a repo-specific hardcode.
pub(super) const BUILD_ARTIFACT_LOCKFILE_BASENAMES: &[&str] = &[
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "Cargo.lock",
    "uv.lock",
];

/// The daemon's own re-stamped install-manifest (#4239): rewritten by every
/// `resync-installed.sh` run and has no `defaults/` source counterpart (it is
/// generated, not copied), so it cannot be classified by the byte-match rule
/// below — it is ignorable by exact path instead. A hard reset merely reverts
/// its stamps, which the next resync rewrites anyway.
pub(super) const INSTALL_METADATA_PATH: &str = ".loom/install-metadata.json";

/// Installed↔source surface mapping (#4332) for the loom repo's own
/// dogfooded install: each installed prefix's tracked, edited-upstream
/// counterpart lives under `defaults/`. Mirrors the exact surface table
/// `resync-installed.sh` walks (`defaults/scripts/resync-installed.sh`,
/// search `widened pure-copy surfaces`) — **not** a uniform prefix rewrite:
/// `.loom/bin/` and `.claude/commands/loom/` map into
/// `defaults/.loom/bin/` and `defaults/.claude/commands/loom/`
/// respectively, while the others map straight into `defaults/<name>/`.
/// Only meaningful in a repo that carries a local `defaults/` tree (the loom
/// repo itself); a consumer install has no `defaults/` dir so the byte-match
/// lookup below always misses and classification is unchanged (#4332 is
/// loom-repo-scoped by construction).
///
/// This class is intentionally **not** mirrored into
/// `defaults/scripts/check-main-clean.sh`'s `LOOM_OWNED_PREFIXES` — see the
/// divergence note in that script's header (#4332). That script protects a
/// different property (catching a *builder* writing into the main worktree
/// by mistake, not classifying resync output for the dispatch gate), so a
/// byte-match there could mask real contamination instead of only ignoring
/// safe, known-regenerable dirt.
pub(super) const INSTALLED_SURFACE_PREFIXES: &[(&str, &str)] = &[
    (".loom/hooks/", "defaults/hooks/"),
    (".loom/scripts/", "defaults/scripts/"),
    (".loom/roles/", "defaults/roles/"),
    (".loom/docs/", "defaults/docs/"),
    (".loom/bin/", "defaults/.loom/bin/"),
    (".claude/commands/loom/", "defaults/.claude/commands/loom/"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The table's matching rule, as `is_ignorable_dirt_with_readers` applies
    /// it: a trailing `/` matches a subtree, anything else is an exact path.
    fn loom_owned(path: &str) -> bool {
        LOOM_OWNED_PREFIXES.iter().any(|prefix| {
            if prefix.ends_with('/') {
                path.starts_with(prefix)
            } else {
                path == *prefix
            }
        })
    }

    #[test]
    fn the_host_local_overlay_is_loom_owned() {
        // The one line that broke `main`. #8078 added `.loom-local/` to this
        // table and shipped it with no test — the file it lived in was frozen
        // by the size ratchet, which is also why adding one could not land.
        // This module can grow, so the entry gets its coverage here.
        assert!(loom_owned(".loom-local/config.json"));
        assert!(loom_owned(".loom-local/anything/nested.txt"));
    }

    #[test]
    fn a_trailing_slash_matches_a_subtree_and_a_bare_name_does_not() {
        // `.loom-managed` is an exact-path entry; `.loom/tokens/` is a
        // subtree. Getting these two rules the wrong way round would either
        // ignore real operator edits or wedge the gate dirty forever.
        assert!(loom_owned(".loom/tokens/whatever.json"));
        assert!(loom_owned(".loom-managed"));
        assert!(
            !loom_owned(".loom-managed-extra"),
            "an exact-path entry must not match by prefix"
        );
    }

    #[test]
    fn an_ordinary_source_edit_is_never_loom_owned() {
        // The property the table exists to protect: a real operator edit must
        // stay visible as dirt, or the gate resets over someone's work.
        for p in [
            "src/main.rs",
            "defaults/scripts/merge-pr.sh",
            ".loom-local-but-not-really/x",
            "README.md",
        ] {
            assert!(!loom_owned(p), "{p} must remain non-ignorable");
        }
    }
}
