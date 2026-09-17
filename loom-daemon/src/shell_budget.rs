//! Measuring the shell epic #7810 is actually trying to retire.
//!
//! # Why a category-aware number rather than one total
//!
//! The first cut of this gate (#8084) ratcheted ONE number: total production
//! shell, 51,814 code lines. That number cannot go to zero and was never
//! supposed to, because a fixed part of it is shell that must stay shell:
//!
//! - **`bootstrap`** runs before a `loom-daemon` binary exists, or after it is
//!   removed. Requiring the binary to install itself is circular.
//! - **`vendored`** is owned upstream in rjwalters/repo; its structure is not
//!   ours to change.
//!
//! Together that floor is 13,230 lines — a quarter of the total — and it is
//! **correctly permanent**. Ratcheting the sum of "what we are retiring" and
//! "what we are keeping" produces a number with no target, so no run of it ever
//! says how far along the epic is or what finished would look like.
//!
//! What the epic is actually retiring is the **portable** pool:
//!
//! - **`contract`** — the file's NAME is consumed by role prompts, CI workflows,
//!   hooks or consumer repos, so the name survives as a stub while the logic
//!   moves into the daemon. The allowlist's own reason line says it: *"The name
//!   stays; logic ports behind it."*
//! - **`hook-entry`** — a `PreToolUse`/`SessionStart` hook needs a
//!   shell-invocable command. Same deal: the stub stays, the logic behind it is
//!   a port target.
//!
//! That pool is 38,439 lines and its target is ~145 — the `stub` category,
//! which is what a fully ported file becomes (under 40 code lines, last line
//! `exec`). **That** is the number worth putting on a ratchet, and the number
//! worth being able to look at.
//!
//! # What it shows today
//!
//! Immediately before epic #7810's first port commit the portable pool was
//! 38,374 lines. It is now 38,439. Four scripts have been ported and roughly
//! 2,200 lines deleted, and the pool is nonetheless **65 lines larger** than
//! when the epic started, because more portable shell arrived than left.
//!
//! Reporting that is the point. A gate that only prevents growth, without ever
//! showing the trend, let four merged ports read as progress while the thing
//! they were retiring quietly grew.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Categories whose logic is a port target — the pool the epic drives to zero.
pub const PORTABLE: &[&str] = &["contract", "hook-entry"];
/// Categories that must stay shell. This floor is not a failure; it is the
/// answer to "what does finished look like".
pub const FLOOR: &[&str] = &["bootstrap", "vendored"];
/// What a fully ported file becomes: under 40 code lines, ending in `exec`.
pub const STUB: &str = "stub";

/// One measurement of the tree.
#[derive(Debug, Clone, Default)]
pub struct Budget {
    /// Code lines per allowlist category.
    pub by_category: BTreeMap<String, u64>,
    /// Files per allowlist category.
    pub files_by_category: BTreeMap<String, u64>,
    /// Production scripts carrying no allowlist entry at all. Non-zero means
    /// the allowlist and the tree have drifted, which makes every other number
    /// here an undercount — so it is surfaced rather than folded in silently.
    pub unlisted: Vec<PathBuf>,
}

impl Budget {
    fn sum(&self, cats: &[&str]) -> u64 {
        cats.iter().filter_map(|c| self.by_category.get(*c)).sum()
    }

    /// The pool epic #7810 is retiring. **This is the headline number.**
    #[must_use]
    pub fn portable(&self) -> u64 {
        self.sum(PORTABLE)
    }

    /// Shell that must stay shell — the epic's floor, not its debt.
    #[must_use]
    pub fn floor(&self) -> u64 {
        self.sum(FLOOR)
    }

    /// Already-ported files' remaining glue.
    #[must_use]
    pub fn stubbed(&self) -> u64 {
        self.by_category.get(STUB).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn total(&self) -> u64 {
        self.by_category.values().sum()
    }

    #[must_use]
    pub fn file_count(&self) -> u64 {
        self.files_by_category.values().sum()
    }
}

/// Whether a tracked `.sh` path counts as PRODUCTION shell.
///
/// Tests are excluded, and the reason is load-bearing rather than tidy: the
/// retained black-box suite method every port in this epic runs on REQUIRES
/// test shell to grow as production shell shrinks — a port keeps the old suite
/// and runs its assertions against the Rust. Counting tests would make the
/// correct move look like a regression, and a gate that punishes the right
/// behaviour gets gamed or deleted.
///
/// `.loom/` is excluded as an installed mirror of `defaults/`; counting both
/// double-counts one source.
#[must_use]
pub fn is_production_shell(path: &str) -> bool {
    if path.starts_with(".loom/") {
        return false;
    }
    if path.split('/').any(|seg| seg == "tests" || seg == "test") {
        return false;
    }
    !path.rsplit('/').next().unwrap_or(path).starts_with("test-")
}

/// Code lines: leading whitespace stripped, blanks skipped, `#`-leading lines
/// skipped. Byte-for-byte the rule `check-file-size-budget.sh` applies to `.sh`
/// (its `measure_all` awk pass), so the two gates never disagree about a file.
#[must_use]
pub fn code_lines(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with('#')
        })
        .count()
}

/// Parse `scripts/shell-allowlist.txt` into `path -> category`.
#[must_use]
pub fn parse_allowlist(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.to_string()))
        })
        .collect()
}

/// Measure the tree at `root`.
///
/// # Errors
/// When `git ls-files` cannot be run or fails.
pub fn measure(root: &Path) -> Result<Budget, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--", "*.sh"])
        .output()
        .map_err(|e| format!("could not run `git ls-files` in {}: {e}", root.display()))?;
    if !out.status.success() {
        return Err(format!(
            "`git ls-files` failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let tracked: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    // A discovery failure must not read as "no shell left". Without this, a
    // broken pathspec or a non-git checkout reports a budget of zero, which
    // every downstream check would happily accept as a win.
    if tracked.len() < 400 {
        return Err(format!(
            "expected 400+ tracked .sh files, found {} — shell discovery is broken, and a \
             measurement of nothing must not read as progress",
            tracked.len()
        ));
    }

    let allowlist_text = std::fs::read_to_string(root.join("scripts/shell-allowlist.txt"))
        .map_err(|e| format!("could not read scripts/shell-allowlist.txt: {e}"))?;
    let categories = parse_allowlist(&allowlist_text);

    let mut budget = Budget::default();
    for rel in tracked.iter().filter(|p| is_production_shell(p)) {
        let text = std::fs::read_to_string(root.join(rel))
            .map_err(|e| format!("tracked shell file {rel} could not be read as text: {e}"))?;
        let n = code_lines(&text) as u64;
        match categories.get(rel) {
            Some(cat) => {
                *budget.by_category.entry(cat.clone()).or_default() += n;
                *budget.files_by_category.entry(cat.clone()).or_default() += 1;
            }
            None => budget.unlisted.push(PathBuf::from(rel)),
        }
    }
    Ok(budget)
}

/// Render the progress report — the thing a human reads to answer "are we
/// actually getting anywhere".
#[must_use]
pub fn render_report(budget: &Budget, origin_portable: u64) -> String {
    let portable = budget.portable();
    let net = i128::from(portable) - i128::from(origin_portable);
    let direction = if net > 0 {
        format!("+{net} — the pool has GROWN since the epic began")
    } else if net < 0 {
        format!("{net} — retired since the epic began")
    } else {
        "0 — unchanged since the epic began".to_string()
    };

    // Progress is measured against what can actually be retired, not against a
    // total that includes a permanent floor.
    let target = budget.stubbed();
    let retirable = portable.saturating_sub(target);

    let mut s = String::new();
    s.push_str("Epic #7810 — retire portable shell\n\n");
    s.push_str(&format!(
        "  portable remaining   {portable:>7}   <- the number that must reach ~{target}\n"
    ));
    s.push_str(&format!(
        "  irreducible floor    {:>7}   bootstrap + vendored; correctly permanent\n",
        budget.floor()
    ));
    s.push_str(&format!(
        "  already stubbed      {:>7}   what a ported file leaves behind\n",
        budget.stubbed()
    ));
    s.push_str(&format!(
        "  ----------------------------\n  total production     {:>7}   across {} files\n\n",
        budget.total(),
        budget.file_count()
    ));
    s.push_str(&format!("  net vs epic start    {direction}\n"));
    s.push_str(&format!("  still to retire      {retirable:>7}\n"));
    s.push_str("\n  by category:\n");
    for (cat, lines) in &budget.by_category {
        let files = budget.files_by_category.get(cat).copied().unwrap_or(0);
        let role = if PORTABLE.contains(&cat.as_str()) {
            "port target"
        } else if FLOOR.contains(&cat.as_str()) {
            "stays shell"
        } else {
            "ported"
        };
        s.push_str(&format!("    {cat:<12} {lines:>7} lines  {files:>4} files   ({role})\n"));
    }
    if !budget.unlisted.is_empty() {
        s.push_str(&format!(
            "\n  WARNING: {} production script(s) carry no allowlist entry, so every\n  \
             number above is an undercount:\n",
            budget.unlisted.len()
        ));
        for p in budget.unlisted.iter().take(10) {
            s.push_str(&format!("    {}\n", p.display()));
        }
    }
    s
}

/// The portable-shell figure at epic #7810's first port commit, read from the
/// baseline file. It is the denominator progress is measured against and is
/// never regenerated — an origin that moves measures nothing.
#[must_use]
pub fn read_origin_portable(root: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(root.join("scripts/shell-budget-baseline.txt")).ok()?;
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .find_map(|l| {
            let mut it = l.split_whitespace();
            (it.next()? == "origin_portable").then(|| it.next()?.parse().ok())?
        })
}

/// The invariants that hold of any single tree, independent of comparison.
///
/// An unlisted production script makes every figure an undercount whichever
/// revision you measure, and a scope filter that stops seeing whole directories
/// makes the gate pass for the wrong reason. Neither needs a `before` to judge.
///
/// # Errors
/// Returns the operator-facing explanation when the tree is malformed.
pub fn check_invariants(budget: &Budget) -> Result<(), String> {
    if !budget.unlisted.is_empty() {
        return Err(format!(
            "{} production script(s) carry no allowlist entry, so every figure is an \
             undercount — add them to scripts/shell-allowlist.txt:\n{}",
            budget.unlisted.len(),
            budget
                .unlisted
                .iter()
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if budget.file_count() < 150 {
        return Err(format!(
            "only {} production shell files were counted — the scope filter is too broad, and a \
             gate that measures almost nothing passes for the wrong reason",
            budget.file_count()
        ));
    }
    Ok(())
}

/// Measure the tree at a git revision rather than the working copy.
///
/// # Why this exists
///
/// A ratchet that compares against a committed baseline number breaks every
/// time `main` moves: the number is a snapshot of one tree, and any other tree
/// disagrees with it. That is not a theoretical cost — this baseline went stale
/// twice in one day, the role-prompt ratchet red-lined `main` at its own merge
/// commit for the same reason (#8105), and the standing instruction it produces
/// is "regenerate the baseline on every merge", which is a chore that teaches
/// people to regenerate without looking.
///
/// Comparing against the **merge-base** instead asks the only question the gate
/// actually cares about: *does this change add portable shell?* Whatever `main`
/// did in the meantime is not this change's doing and not this gate's business.
/// It needs no committed number, so it cannot go stale.
///
/// # Errors
/// When git cannot be run, or the revision does not resolve.
pub fn measure_at_rev(root: &Path, rev: &str) -> Result<Budget, String> {
    let git = |args: &[&str]| -> Result<String, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .map_err(|e| format!("could not run git {args:?}: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };

    let allowlist_text = git(&["show", &format!("{rev}:scripts/shell-allowlist.txt")])?;
    let categories = parse_allowlist(&allowlist_text);

    let listing = git(&["ls-tree", "-r", "--name-only", "-z", rev])?;
    let tracked: Vec<&str> = listing.split('\0').filter(|s| !s.is_empty()).collect();
    let shell: Vec<&str> = tracked
        .iter()
        .copied()
        .filter(|p| p.ends_with(".sh") && is_production_shell(p))
        .collect();

    // Same floor as the working-tree path: a revision that yields almost no
    // shell means the query broke, and a measurement of nothing must not read
    // as a clean comparison.
    if shell.len() < 100 {
        return Err(format!(
            "only {} production shell files found at {rev} — the revision query is broken, and \
             an empty comparison must not read as no growth",
            shell.len()
        ));
    }

    let mut budget = Budget::default();
    for rel in shell {
        let text = git(&["show", &format!("{rev}:{rel}")])?;
        let n = code_lines(&text) as u64;
        match categories.get(rel) {
            Some(cat) => {
                *budget.by_category.entry(cat.clone()).or_default() += n;
                *budget.files_by_category.entry(cat.clone()).or_default() += 1;
            }
            None => budget.unlisted.push(PathBuf::from(rel)),
        }
    }
    Ok(budget)
}

/// What this change should be measured against.
pub struct Comparison {
    /// The revision to measure `before` at.
    pub rev: String,
    /// How to describe it to a human.
    pub desc: String,
}

/// Resolve the revision to compare against.
///
/// Normally the merge-base of `HEAD` and `base_ref` — which is precisely "the
/// tree this change started from", so the comparison measures the change and
/// nothing else.
///
/// **On the base branch itself** (a push to `main`, where the merge-base of
/// `HEAD` and `origin/main` is `HEAD`) that would compare a tree against
/// itself and pass vacuously. There, the analogous question is what the most
/// recent commit did, so it falls back to `HEAD~1`. Without this the gate
/// would be a no-op on every direct push, which is exactly where nobody is
/// reviewing.
///
/// # Errors
/// When git cannot be run at all, or `HEAD` has no parent to compare against.
pub fn comparison(root: &Path, base_ref: &str) -> Result<Comparison, String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let head = git(&["rev-parse", "HEAD"]).ok_or_else(|| "could not resolve HEAD".to_string())?;

    // No common ancestor (a shallow clone, or an unrelated base) — compare
    // against the base ref directly rather than silently skipping the check.
    let Some(base) = git(&["merge-base", "HEAD", base_ref]) else {
        return Ok(Comparison {
            rev: base_ref.to_string(),
            desc: format!("{base_ref} (no merge-base found)"),
        });
    };

    if base == head {
        // We are ON the base branch. Measure what the tip commit did.
        let parent = git(&["rev-parse", "HEAD~1"]).ok_or_else(|| {
            format!(
                "HEAD is {base_ref} and has no parent to compare against — refusing to compare \
                 a tree with itself, which would pass without measuring anything"
            )
        })?;
        return Ok(Comparison {
            rev: parent.clone(),
            desc: format!(
                "HEAD~1 ({}) — on {base_ref}, so this measures the tip commit",
                &parent[..parent.len().min(8)]
            ),
        });
    }

    Ok(Comparison {
        rev: base.clone(),
        desc: format!("{base_ref} ({})", &base[..base.len().min(8)]),
    })
}

/// Compare this tree against a base revision. `Ok` when the change does not
/// grow the portable pool.
///
/// # Errors
/// Returns the operator-facing explanation when it does.
pub fn check_against_rev(now: &Budget, before: &Budget, base_desc: &str) -> Result<(), String> {
    if now.portable() <= before.portable() {
        return Ok(());
    }
    let mut grew: Vec<(&String, u64, u64)> = Vec::new();
    for (cat, lines) in &now.by_category {
        if !PORTABLE.contains(&cat.as_str()) {
            continue;
        }
        let was = before.by_category.get(cat).copied().unwrap_or(0);
        if *lines > was {
            grew.push((cat, was, *lines));
        }
    }
    let detail = grew
        .iter()
        .map(|(c, was, now)| format!("    {c:<12} {was} -> {now}  (+{})", now - was))
        .collect::<Vec<_>>()
        .join("\n");

    Err(format!(
        "This change adds {} code lines of PORTABLE shell (vs {base_desc}: {} -> {}).\n\n{detail}\n\n\
         Portable shell is what epic #7810 is retiring — `contract` + `hook-entry`, whose logic \
         moves into the daemon behind a stub. Adding to it works directly against the epic.\n\n\
         Options, best first:\n\n\
         \x20 1. Put the new logic in the daemon instead. That is the language policy\n\
         \x20    (.loom/docs/shell-language-policy.md) and it makes this gate a non-event.\n\
         \x20 2. Remove portable shell elsewhere in the same change to pay for it.\n\
         \x20 3. If the script must stay shell forever, it may belong in `bootstrap` or\n\
         \x20    `vendored` rather than `contract` — but that is a claim about the script,\n\
         \x20    argued in scripts/shell-allowlist.txt, not a way around this number.\n\n\
         Note this compares against the MERGE-BASE, so it is measuring what YOUR change did.\n\
         Whatever main did meanwhile is not your problem and not this gate's business.",
        now.portable() - before.portable(),
        before.portable(),
        now.portable()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget_of(pairs: &[(&str, u64)]) -> Budget {
        let mut b = Budget::default();
        for (c, n) in pairs {
            b.by_category.insert((*c).to_string(), *n);
            b.files_by_category.insert((*c).to_string(), 1);
        }
        b
    }

    #[test]
    fn portable_excludes_the_permanent_floor() {
        let b = budget_of(&[
            ("contract", 100),
            ("hook-entry", 10),
            ("bootstrap", 999),
            ("vendored", 999),
            ("stub", 5),
        ]);
        assert_eq!(b.portable(), 110, "only contract + hook-entry are retirable");
        assert_eq!(b.floor(), 1998);
        assert_eq!(b.total(), 2113);
    }

    #[test]
    fn the_report_says_plainly_when_the_pool_grew() {
        let b = budget_of(&[("contract", 38439), ("bootstrap", 100), ("stub", 145)]);
        let out = render_report(&b, 38374);
        assert!(out.contains("+65"), "{out}");
        assert!(out.contains("has GROWN"), "the direction must not be buried: {out}");
    }

    #[test]
    fn the_report_says_plainly_when_the_pool_shrank() {
        let b = budget_of(&[("contract", 30000), ("stub", 145)]);
        let out = render_report(&b, 38374);
        assert!(out.contains("-8374"), "{out}");
        assert!(out.contains("retired since"), "{out}");
    }

    #[test]
    fn unlisted_scripts_are_surfaced_because_they_make_the_count_wrong() {
        let mut b = budget_of(&[("contract", 10)]);
        b.unlisted
            .push(PathBuf::from("defaults/scripts/mystery.sh"));
        let out = render_report(&b, 10);
        assert!(out.contains("WARNING"), "{out}");
        assert!(out.contains("undercount"), "{out}");
        assert!(out.contains("mystery.sh"), "{out}");
    }

    #[test]
    fn code_lines_matches_the_file_size_ratchets_rule() {
        assert_eq!(code_lines("#!/usr/bin/env bash\n\nset -e\n# note\nfoo\n"), 2);
        assert_eq!(code_lines("   # indented comment\n"), 0);
        assert_eq!(code_lines("  echo hi   # trailing comment\n"), 1);
    }

    #[test]
    fn the_scope_rule_is_not_fooled_by_substrings() {
        assert!(is_production_shell("defaults/scripts/latest/thing.sh"));
        assert!(is_production_shell("defaults/scripts/testable.sh"));
        assert!(!is_production_shell("defaults/scripts/tests/test-x.sh"));
        assert!(!is_production_shell("scripts/test-installer.sh"));
        assert!(!is_production_shell(".loom/hooks/guard.sh"));
    }

    #[test]
    fn an_allowlist_line_needs_both_a_path_and_a_category() {
        let m = parse_allowlist("a.sh contract reason here\n# comment\n\nb.sh\nc.sh bootstrap\n");
        assert_eq!(m.get("a.sh").map(String::as_str), Some("contract"));
        assert_eq!(m.get("c.sh").map(String::as_str), Some("bootstrap"));
        assert!(!m.contains_key("b.sh"), "a path with no category is not an entry");
    }
}
