//! Fix-weighted progress for epic #7810 (#8233).
//!
//! # Why lines are the wrong unit on their own
//!
//! The line figure measures the COST side of a port honestly and the BENEFIT
//! side not at all. The two ends of the pool are indistinguishable to it:
//!
//! - 15 scripts hold 33% of the lines and **54% of the fix-commits**.
//! - 76 scripts hold 23% of the lines and **zero**.
//!
//! Porting that quiet 23% would register as 23% of the epic complete, cost
//! roughly 24,000 lines of Rust at the observed ~2.7x ratio, and change
//! nothing about how often this system needs fixing. In a repo that runs
//! autonomy-by-default, a metric that rewards the wrong work will get the
//! wrong work done — reliably, and at scale.
//!
//! # What counts as retired, and why it is NOT the `stub` category
//!
//! #8233 originally specified "scripts now in the `stub` category". That is
//! wrong, and measuring it reported 3 — eight pieces of trivial glue.
//!
//! The allowlist's categories record **why a file is allowed to be shell**,
//! not **whether its logic has moved**. A ported script keeps `contract`,
//! because the reason it still exists is that its *name* is an invocation
//! contract: `dep-recheck-fingerprint.sh` is 7 code lines that hand off to
//! Rust and is categorised `contract`, while `stub` is a separate shape test
//! for glue that never carried logic in the first place. The two taxonomies
//! are orthogonal and neither one answers the question.
//!
//! So this asks the structural question directly: **has this script stopped
//! carrying its logic?** That is true when both halves hold:
//!
//! 1. It hands off to the daemon — `exec`s the binary, or calls the repo's
//!    shared [`loom_exec_script_helper`] handoff.
//! 2. It is under the trivial-glue cap, so there is no logic left to carry.
//!    STRICTLY under: `check-shell-allowlist.sh` gates on `-lt`, and a review
//!    caught this reading `<=`. The two then disagreed about a script of
//!    exactly [`super::STUB_CAP`] lines — no such script exists today, which
//!    is precisely why it would have sat there until one did.
//!
//! Both halves are load-bearing. `loom-daemon-start.sh` is 1,184 code lines
//! with 24 fixes — the single largest remaining target — and it `exec`s the
//! daemon at the end of a long startup sequence. On the handoff test alone it
//! scores as retired and moves the headline from 5% to 10%, which is to say
//! the metric would lie in precisely the direction that flatters the epic.
//! The cap is what stops that, and it is [`super::STUB_CAP`] — the same
//! number the allowlist gate already machine-checks — rather than a threshold
//! invented here to make a number come out.
//!
//! # What this does NOT touch
//!
//! The ratchet. `shell-budget --check` gates on portable lines growing, and it
//! works precisely because it is simple and un-gameable — it is what stopped
//! the pool growing (+252 in a single afternoon before it, roughly flat
//! after). This is the progress REPORT, not the gate.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// How much of the recent churn now lives in Rust.
#[derive(Debug, Clone, Default)]
pub struct Churn {
    /// Script-fixes against scripts that have stopped carrying their logic.
    pub retired: u64,
    /// Script-fixes against scripts still carrying theirs.
    pub remaining: u64,
    /// Every remaining script that took a fix, most fixes first:
    /// `(path, code lines, fixes)`. The report and `--json` each take a
    /// prefix; see [`JSON_WORST`].
    pub worst: Vec<(String, u64, u64)>,
    /// How many portable scripts have handed off, for the report to name.
    pub delegating: u64,
    /// The window actually measured, for the report to name.
    pub window: &'static str,
}

impl Churn {
    /// Share of the window's script-fixes that porting has retired.
    #[must_use]
    pub fn retired_pct(&self) -> f64 {
        let total = self.retired + self.remaining;
        if total == 0 {
            return 0.0;
        }
        (self.retired as f64) * 100.0 / (total as f64)
    }
}

/// How many remaining scripts `--json` carries.
///
/// A cap, because this is a progress report and not a dataset: uncapped it
/// emits all 119 scripts that took a fix, most of them with one. Twenty covers
/// the "dangerous top 15" the retarget scopes to, which is the decision this
/// figure actually feeds.
pub const JSON_WORST: usize = 20;

/// The trailing window. Fixed rather than configurable: a movable window is a
/// movable goalpost, and the figure is meant to be comparable between runs.
const WINDOW: &str = "6.months";

/// Whether a commit subject describes a fix.
///
/// Deliberately the same crude prefix test used to pick the dangerous set, so
/// the report and the scoping argument cannot disagree about what a fix is.
fn is_fix(subject: &str) -> bool {
    let s = subject.trim_start().to_ascii_lowercase();
    s.starts_with("fix") || s.starts_with("revert") || s.starts_with("hotfix")
}

/// Whether `word` appears in `line` delimited by shell metacharacters.
///
/// `execute_thing` and `noexec` both contain `exec`; neither runs one.
fn has_word(line: &str, word: &str) -> bool {
    let bound = |c: char| !(c.is_alphanumeric() || c == '_' || c == '-');
    let mut from = 0;
    while let Some(i) = line[from..].find(word) {
        let start = from + i;
        let end = start + word.len();
        let before_ok = start == 0 || line[..start].chars().next_back().is_some_and(bound);
        let after_ok = end == line.len() || line[end..].chars().next().is_some_and(bound);
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// Whether this code line hands the process off to the Rust implementation.
///
/// Three idioms are in use, and all three had to be found by reading the
/// scripts rather than assumed — the first cut matched only a literal
/// `loom-daemon` on an `exec` line and silently missed every script that
/// resolves the binary into a variable first, under-reporting by a third.
fn is_handoff(line: &str) -> bool {
    // The shared handoff helper. Require an argument so that
    // `loom_exec_script_helper() {` — the DEFINITION, in script-helper.sh —
    // is not mistaken for a call to it.
    if let Some(rest) = line.split("loom_exec_script_helper").nth(1) {
        if rest.starts_with([' ', '\t']) && !rest.trim().is_empty() {
            return true;
        }
    }
    // `exec loom-daemon …`, or `exec "$DAEMON_BIN" …` / `"$LOOM_DAEMON_BIN"`
    // once the path has been resolved into a variable.
    has_word(line, "exec") && (line.contains("loom-daemon") || line.contains("DAEMON_BIN"))
}

/// Whether `text` is a script that has stopped carrying its own logic.
///
/// See the module docs: both halves are load-bearing.
#[must_use]
pub fn has_been_ported(text: &str) -> bool {
    let code: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    code.len() < super::STUB_CAP && code.iter().any(|l| is_handoff(l))
}

/// Count fix-commits per path over the window.
///
/// ONE `git log` pass, not one per file. The per-file shape is what makes an
/// analysis like this too slow to put in a report that runs on every check —
/// 195 invocations against six months of history.
///
/// The unit is a **script-fix**: one count per (commit, script) pair, so a
/// single commit fixing three scripts contributes three. That is the unit the
/// ratio needs — attributing a commit to one script would require guessing
/// which one it was really about.
///
/// # Errors
/// Returns a message when `git log` cannot be run.
pub fn per_path(root: &Path) -> Result<BTreeMap<String, u64>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "log",
            &format!("--since={WINDOW}"),
            "--name-only",
            // A NUL between the subject and the file list, so a subject
            // containing a newline cannot be mistaken for a path.
            "--format=%x00%s%x00",
        ])
        .output()
        .map_err(|e| format!("could not run git log: {e}"))?;
    if !out.status.success() {
        return Err(format!("git log failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    // Each commit is framed `\0<subject>\0<paths>`, so splitting yields a
    // leading EMPTY element before the first record. Consuming it is what
    // aligns subjects with their own file lists — without this the parse is
    // off by one, every subject is read as a path list, and the counts come
    // back empty. Caught by driving real `git log` output rather than by
    // reasoning about the format.
    let mut parts = text.split('\0');
    let _leading_empty = parts.next();
    while let (Some(subject), Some(paths)) = (parts.next(), parts.next()) {
        if !is_fix(subject) {
            continue;
        }
        for p in paths.lines().map(str::trim).filter(|l| !l.is_empty()) {
            *counts.entry(p.to_string()).or_default() += 1;
        }
    }
    Ok(counts)
}

/// Fix-weighted progress: how much of the recent churn is now behind Rust.
///
/// `categories` is the parsed allowlist, used only to select the **portable**
/// pool — the scripts the epic can actually move. Whether one has been ported
/// is read from the script itself, not from its category; see the module docs
/// for why the category cannot answer that.
///
/// # Errors
/// Returns a message when the history cannot be read.
pub fn measure(root: &Path, categories: &BTreeMap<String, String>) -> Result<Churn, String> {
    let counts = per_path(root)?;

    let mut churn = Churn {
        window: WINDOW,
        ..Churn::default()
    };
    for (path, cat) in categories {
        // bootstrap / vendored / test / stub: never going to be ported, so
        // their churn is neither retired nor retirable. Counting it would put
        // something in the denominator that the epic cannot move.
        if !super::PORTABLE.contains(&cat.as_str()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(path)) else {
            continue;
        };
        let ported = has_been_ported(&text);
        if ported {
            churn.delegating += 1;
        }
        let fixes = counts.get(path).copied().unwrap_or(0);
        if fixes == 0 {
            continue;
        }
        if ported {
            churn.retired += fixes;
        } else {
            churn.remaining += fixes;
            churn
                .worst
                .push((path.clone(), super::code_lines(&text) as u64, fixes));
        }
    }
    churn
        .worst
        .sort_by(|a, b| b.2.cmp(&a.2).then_with(|| b.1.cmp(&a.1)));
    Ok(churn)
}

/// The report section. Empty when the history is unreadable — a shallow clone
/// has no six-month window, and a report that refuses to print because of that
/// is worse than one that omits a section.
#[must_use]
pub fn render(churn: &Churn) -> String {
    if churn.retired + churn.remaining == 0 {
        return String::new();
    }
    let mut s =
        String::from("\n  fix-weighted progress (the benefit side; lines above are the cost)\n");
    // Right-aligned to the same column as the line figures above, so the cost
    // and the benefit read as one table rather than two reports.
    s.push_str(&format!(
        "    {:<19}{:>7}   of {} script-fixes in the last {} ({:.0}%)\n",
        "churn retired",
        churn.retired,
        churn.retired + churn.remaining,
        churn.window.replace('.', " "),
        churn.retired_pct()
    ));
    s.push_str(&format!(
        "    {:<19}{:>7}   portable scripts have handed off to loom-daemon\n",
        "ported", churn.delegating
    ));
    if !churn.worst.is_empty() {
        s.push_str("    worst remaining:\n");
        for (path, lines, fixes) in churn.worst.iter().take(5) {
            s.push_str(&format!("      {fixes:>3} fixes  {lines:>5} lines  {path}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests;
