//! Doc-lint test for "Stage -1: Backend detection" in the `/loom:sweep` skill
//! (Issue #3454, Phase D of epic #3449; Stage -1 itself now lives in
//! `defaults/.claude/commands/loom/sweep-backend-detection.md` after the #7726
//! split).
//!
//! Stage -1 probes whether the loom-daemon is reachable AND whether a
//! multi-account token pool exists; if both preconditions hold and the mode is
//! not C, it dispatches each candidate issue to the daemon via
//! `mcp__loom__dispatch_sweep` (Phase A) and exits. Otherwise it falls through
//! to today's in-process subagent dispatch (Modes A/B/C unchanged).
//!
//! ---------------------------------------------------------------------------
//! Assertion classification (#7994 — migrated off code-fence prose pins)
//! ---------------------------------------------------------------------------
//!
//! This file used to assert that specific sentences, pseudocode lines, and
//! shell fragments existed **verbatim** inside the markdown. That failure mode
//! is what parent issue #7979 deprecates: a *correct* rewording or relocation
//! of the asserted text breaks the build with a misleading semantic-sounding
//! message (the concrete incident: #7876 fixed a real bug, #7950/#7948 red-mained
//! on the pinned literal of the *corrected* doc). Assertions are now tagged:
//!
//! - **EXECUTABLE** — the doc's fenced block is *located, extracted, and run*
//!   (real `bash`, or an interpreter for the LLM-directed pseudocode), and the
//!   test asserts on the **observed behaviour**. A behaviour-preserving reword,
//!   re-indent, comment edit, or variable rename inside the fence cannot fail
//!   these; only a change in what the block *does* can. This mirrors the
//!   `defaults/scripts/tests/test-guide-*.sh` locate → extract → execute →
//!   assert-on-behaviour style, and the sibling migration of
//!   `sweep_md_doc_lint.rs` (#7993 / PR #8024).
//! - **REVIEW-ONLY** — LLM-directed orchestration prose or an operator-facing
//!   status table with **no executable surface** (no parser, binary, or shell
//!   block to run it against). These assert STRUCTURE/PRESENCE only (heading
//!   anchors, occurrence floors, tolerant any-of phrasings) and every needle is
//!   satisfiable by text that lives **outside** any code fence, so a fence
//!   rewrite cannot break them. They are kept — rather than deleted — so the
//!   invariant stays visible to a human reviewer; they are enumerated as
//!   review-only in the #7994 PR description.
//!
//! **No assertion in this file pins a literal whose only occurrence in the
//! skill docs is inside a markdown code fence** (#7994 AC #6). If you add one,
//! convert it to an EXECUTABLE check instead — the helpers below
//! (`first_fenced_block_after`, `nth_fenced_block_after`, `run_bash_in`,
//! `write_executable_stub`, the `DECIDE` interpreter) exist for exactly that.
//!
//! Helper-shape note (#7994 / #7993): these helpers intentionally use the same
//! names and shapes as the ones PR #8024 introduces for `sweep_md_doc_lint.rs`
//! (`first_fenced_block_after`, `write_executable_stub`). They are duplicated
//! here on purpose — #7994 explicitly permits duplicate-then-de-duplicate so
//! the two migrations can land independently — and are trivially hoistable into
//! one shared `tests/doc_lint_support/` module once both have merged.
//!
//! Companion test: `sweep_md_doc_lint.rs` (Phase B, #3453).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SWEEP_SKILL_DIR_RELATIVE: &str = "../defaults/.claude/commands/loom";

/// The `/loom:sweep` skill in document order — the `sweep.md` dispatcher plus
/// the sibling reference files #7726 split out of the pre-split monolith. Kept
/// in sync with the identical list in `sweep_md_doc_lint.rs`.
///
/// Stage -1 itself now lives in `sweep-backend-detection.md`, and the flag
/// documentation it cross-checks (`--no-daemon` / `--claim-owned` occurrence
/// floors) lives in `sweep-arguments.md`, so the occurrence-count assertions
/// below only remain meaningful against the concatenated skill.
const SWEEP_SKILL_FILES: &[&str] = &[
    "sweep.md",
    "sweep-arguments.md",
    "sweep-examples.md",
    "sweep-execution-model.md",
    "sweep-backend-detection.md",
    "sweep-scheduling-signals.md",
    "sweep-dry-run.md",
    "sweep-mode-c-lifecycle.md",
    "sweep-wave-lifecycle.md",
    "sweep-summary-output.md",
    "sweep-run-hygiene.md",
    "sweep-reference.md",
];

/// Repo root (the workspace parent of `loom-daemon/`), used to reach both the
/// skill markdown and the real shell libraries the extracted blocks source.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon/ has a parent")
        .to_path_buf()
}

/// Reads the whole `/loom:sweep` skill as one string, in [`SWEEP_SKILL_FILES`]
/// order. A missing sibling is a hard failure — otherwise the file it documents
/// would silently stop being linted.
fn read_sweep_md() -> String {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SWEEP_SKILL_DIR_RELATIVE);
    let mut combined = String::new();
    for name in SWEEP_SKILL_FILES {
        let path = dir.join(name);
        let text = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "sweep skill file `{name}` not found at {} (dir-relative path: {}): {e} — \
                 if it was intentionally renamed/removed, update SWEEP_SKILL_FILES",
                path.display(),
                SWEEP_SKILL_DIR_RELATIVE,
            );
        });
        combined.push_str(&text);
        combined.push('\n');
    }
    combined
}

// ===========================================================================
// Extract-and-execute helpers (same shape as PR #8024's doc_lint_support)
// ===========================================================================

/// Returns the `n`-th (0-based) fenced block tagged ```` ```<lang> ```` that
/// starts **after** `anchor` in `content`, with the fence delimiters stripped.
///
/// This is the "locate" half of locate → extract → execute. It deliberately
/// anchors on a *heading* (structure) rather than on the block's contents, so
/// the block's own text is free to change.
fn nth_fenced_block_after(content: &str, anchor: &str, lang: &str, n: usize) -> String {
    let anchor_at = content.find(anchor).unwrap_or_else(|| {
        panic!(
            "sweep skill docs are missing the `{anchor}` anchor — the Stage -1 \
             doc-lint cannot locate the fenced block it extracts. If the section \
             was intentionally renamed, update the anchor in this test."
        )
    });
    let open = format!("```{lang}");
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in content[anchor_at..].lines() {
        match current.as_mut() {
            None => {
                if line.trim_end() == open {
                    current = Some(Vec::new());
                }
            }
            Some(buf) => {
                if line.trim_end() == "```" {
                    blocks.push(buf.join("\n"));
                    current = None;
                } else {
                    buf.push(line);
                }
            }
        }
        if blocks.len() > n {
            break;
        }
    }
    blocks.into_iter().nth(n).unwrap_or_else(|| {
        panic!(
            "sweep skill docs have no ```{lang} block #{n} after the `{anchor}` \
             anchor — #7994 requires this block stay extractable so its behaviour \
             (not its wording) can be asserted"
        )
    })
}

/// Convenience wrapper for the common "the first fenced block after this
/// heading" case. Named to match PR #8024's helper of the same shape.
fn first_fenced_block_after(content: &str, anchor: &str, lang: &str) -> String {
    nth_fenced_block_after(content, anchor, lang, 0)
}

/// Writes `body` as an executable script at `dir/name`. Used to stub the
/// OS-tool boundary (`git`, `df`) so an extracted block runs deterministically
/// against the **real** Loom shell libraries. Named to match PR #8024's helper.
fn write_executable_stub(dir: &Path, name: &str, body: &str) {
    fs::create_dir_all(dir).expect("create stub bin dir");
    let path = dir.join(name);
    fs::write(&path, body).expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
    }
}

/// The result of running an extracted shell block.
struct BashRun {
    stdout: String,
    stderr: String,
    status: i32,
}

impl BashRun {
    /// Reads back a `KEY=value` line the test harness appended to the extracted
    /// block. Last occurrence wins.
    fn captured(&self, key: &str) -> String {
        let prefix = format!("{key}=");
        self.stdout
            .lines()
            .filter_map(|l| l.strip_prefix(&prefix))
            .next_back()
            .map(str::to_string)
            .unwrap_or_else(|| {
                panic!(
                    "extracted block did not report `{key}` — stdout was:\n{}\nstderr:\n{}",
                    self.stdout, self.stderr
                )
            })
    }
}

/// Runs `script` with `bash`, `dir` as the working directory.
fn run_bash_in(dir: &Path, script: &str, env: &[(&str, String)], unset: &[&str]) -> BashRun {
    let script_path = dir.join("__extracted_block.sh");
    fs::write(&script_path, script).expect("write extracted block");
    let mut cmd = Command::new("bash");
    cmd.arg(&script_path).current_dir(dir);
    for key in unset {
        cmd.env_remove(key);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("spawn bash for the extracted block");
    BashRun {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(-1),
    }
}

/// Env vars that must never leak from the test runner's environment into an
/// extracted block — every one of them changes the block's decision.
const POOL_ENV_LEAKS: &[&str] = &[
    "LOOM_ACCOUNTS_ENV",
    "LOOM_CLAUDE_MONITOR_DIR",
    "LOOM_WORKTREE_ROOT",
    "LOOM_PER_WORKTREE_GB",
    "LOOM_DAEMON_WAVE_TARGET",
    "LOOM_SUBAGENT_WAVE_CAP",
];

// ===========================================================================
// Interpreter for the Stage -1 decision pseudocode (LLM-directed ```text)
// ===========================================================================

/// One `if` / `elif` / `else` branch lifted out of a Stage -1 pseudocode fence.
#[derive(Debug)]
struct Branch {
    /// `None` for the `else:` fallthrough.
    condition: Option<String>,
    action: String,
}

/// The five boolean inputs Stage -1's decision tree is a pure function of.
#[derive(Clone, Copy, Debug)]
struct Stage1Inputs {
    mode_c: bool,
    no_daemon: bool,
    claim_owned: bool,
    probe_daemon: bool,
    probe_pool: bool,
}

impl Stage1Inputs {
    /// Enumerates all 2^5 = 32 input states, so the interpreted tree is
    /// exercised exhaustively rather than on a hand-picked sample.
    fn all() -> Vec<Self> {
        (0u8..32)
            .map(|bits| Self {
                mode_c: bits & 1 != 0,
                no_daemon: bits & 2 != 0,
                claim_owned: bits & 4 != 0,
                probe_daemon: bits & 8 != 0,
                probe_pool: bits & 16 != 0,
            })
            .collect()
    }
}

/// Parses the indented `if` / `elif` / `else` lines that follow a `<LABEL>:`
/// line inside an extracted pseudocode fence.
///
/// Tolerant by construction: trailing `#` comments are stripped, blank lines
/// and re-indentation are ignored, and the *order* of the branches — the actual
/// contract — is what is preserved.
fn parse_branches(block: &str, label: &str) -> Vec<Branch> {
    let mut branches = Vec::new();
    let mut inside = false;
    for raw in block.lines() {
        let trimmed = raw.trim();
        if !inside {
            if trimmed == format!("{label}:") {
                inside = true;
            }
            continue;
        }
        // A blank line or a dedent to column 0 ends the labelled block.
        if trimmed.is_empty() || !(raw.starts_with(' ') || raw.starts_with('\t')) {
            break;
        }
        let body = trimmed.split('#').next().unwrap_or("").trim();
        if body.is_empty() {
            continue;
        }
        let Some((head, action)) = body.split_once(':') else {
            continue;
        };
        let head = head.trim();
        let action = action.trim().trim_end_matches("()").trim().to_string();
        if let Some(cond) = head
            .strip_prefix("if ")
            .or_else(|| head.strip_prefix("elif "))
        {
            branches.push(Branch {
                condition: Some(cond.trim().to_string()),
                action,
            });
        } else if head == "else" {
            branches.push(Branch {
                condition: None,
                action,
            });
        }
    }
    assert!(
        !branches.is_empty(),
        "could not parse any `{label}:` branch out of the extracted pseudocode \
         fence — #7994's interpreter needs the `if`/`elif`/`else <cond>: <action>` \
         shape. Extracted block was:\n{block}"
    );
    branches
}

fn is_or_separator(token: &str) -> bool {
    matches!(token.to_ascii_lowercase().as_str(), "or" | "||" | "∨")
}

fn is_and_separator(token: &str) -> bool {
    matches!(token.to_ascii_lowercase().as_str(), "and" | "&&" | "∧")
}

fn split_on<'a>(tokens: &[&'a str], is_sep: fn(&str) -> bool) -> Vec<Vec<&'a str>> {
    let mut groups: Vec<Vec<&'a str>> = vec![Vec::new()];
    for token in tokens {
        if is_sep(token) {
            groups.push(Vec::new());
        } else {
            groups
                .last_mut()
                .expect("groups is never empty")
                .push(token);
        }
    }
    groups.retain(|g| !g.is_empty());
    groups
}

/// Maps one atomic condition term to the input signal it tests.
///
/// An unrecognised term is a hard failure, on purpose: that is the drift
/// detector. If Stage -1 grows a new short-circuit signal, this panics with an
/// instruction to model it here — which is a *true* failure (the contract
/// changed), unlike the prose pin it replaced.
fn eval_atom(atom: &str, inputs: Stage1Inputs) -> bool {
    let upper = atom.to_ascii_uppercase();
    assert!(
        !upper.split_whitespace().any(|t| t == "NOT" || t == "!"),
        "the Stage -1 pseudocode interpreter does not model negation, but the \
         extracted condition term `{atom}` contains one — teach `eval_atom` \
         about it before relying on this test"
    );
    if upper.contains("MODE C") {
        return inputs.mode_c;
    }
    if upper.contains("NO-DAEMON") || upper.contains("NO_DAEMON") {
        return inputs.no_daemon;
    }
    if upper.contains("CLAIM_OWNED") || upper.contains("CLAIM-OWNED") {
        return inputs.claim_owned;
    }
    if upper.contains("PROBE_DAEMON") {
        return inputs.probe_daemon;
    }
    if upper.contains("PROBE_POOL") {
        return inputs.probe_pool;
    }
    panic!(
        "Stage -1 pseudocode names a decision signal this test does not model: \
         `{atom}`. Stage -1's routing contract is a pure function of \
         (Mode C, --no-daemon, CLAIM_OWNED, PROBE_DAEMON, PROBE_POOL); a new \
         signal is a real contract change — model it in `eval_atom` together \
         with `expected_decision`."
    );
}

/// Evaluates an extracted condition as an OR-of-ANDs over atomic terms.
fn eval_condition(condition: &str, inputs: Stage1Inputs) -> bool {
    let tokens: Vec<&str> = condition.split_whitespace().collect();
    split_on(&tokens, is_or_separator).into_iter().any(|disj| {
        split_on(&disj, is_and_separator)
            .into_iter()
            .all(|conj| eval_atom(&conj.join(" "), inputs))
    })
}

/// Runs the extracted branch list: the first matching branch's action wins.
fn run_branches(branches: &[Branch], inputs: Stage1Inputs) -> &str {
    for branch in branches {
        match &branch.condition {
            Some(cond) => {
                if eval_condition(cond, inputs) {
                    return &branch.action;
                }
            }
            None => return &branch.action,
        }
    }
    panic!(
        "the extracted Stage -1 decision tree fell off the end for {inputs:?} — \
         it must end in an `else:` universal fallthrough (#3454 AC #1)"
    );
}

/// The Stage -1 routing contract, independent of how the doc spells it.
fn expected_decision(inputs: Stage1Inputs) -> &'static str {
    // Precedence (#3454 AC #1, #3829/#4111): Mode C, then --no-daemon, then the
    // daemon-owned-child marker, all short-circuit to the subagent path before
    // any probe is consulted. Only a STRICT AND of both probes reaches the
    // daemon; everything else falls through.
    if inputs.mode_c || inputs.no_daemon || inputs.claim_owned {
        return "use_subagent";
    }
    if inputs.probe_daemon && inputs.probe_pool {
        return "use_daemon";
    }
    "use_subagent"
}

// ===========================================================================
// REVIEW-ONLY: structural presence of the stage and its probe sections
// ===========================================================================

/// REVIEW-ONLY (#7994): the stage is identified by its NUMBER (`-1`), which is
/// the stable anchor; the "Backend detection …" title text is prose that can be
/// reworded (cf. #3830→#3834 for a sweep-section rename that red-mained on a
/// pinned title). The needle lives in an `##` heading, outside any code fence,
/// so no fence rewrite can break it. There is no executable surface for "the
/// stage exists" — deleting the stage is the only thing this can catch, and
/// that is exactly what it is for.
#[test]
fn sweep_md_has_stage_minus_one_section() {
    let content = read_sweep_md();
    assert!(
        content.contains("## Stage -1:"),
        "expected a `## Stage -1:` section header in the sweep skill docs — the \
         Phase D backend-detection stage is required by #3454 AC #1 (asserted by \
         stage number, not by exact title wording)"
    );
}

/// REVIEW-ONLY (#7994): each probe must keep its own `####` section so the
/// contract stays documented. Anchored on the heading prefix (unfenced
/// structure), NOT on the pseudocode tokens inside the decision-tree fence —
/// the *behaviour* those tokens encode is asserted executably by
/// [`stage_minus_one_decision_tree_routes_by_contract`] below.
///
/// Replaces the pre-#7994 `sweep_md_decision_tree_documents_all_probes`, which
/// scanned the whole doc for the bare identifiers and would have been satisfied
/// by an incidental mention anywhere.
#[test]
fn sweep_md_documents_all_three_probe_sections() {
    let content = read_sweep_md();
    for probe in ["PROBE_MODE", "PROBE_DAEMON", "PROBE_POOL"] {
        let heading = format!("#### {probe}");
        assert!(
            content.contains(&heading),
            "the sweep skill docs are missing a `{heading}` section — #3454 AC #1 \
             requires each of the three Stage -1 probes be documented in its own \
             subsection (PROBE_MODE / PROBE_DAEMON / PROBE_POOL)"
        );
    }
}

// ===========================================================================
// EXECUTABLE: the Stage -1 decision tree, interpreted
// ===========================================================================

/// EXECUTABLE (#7994): extracts the `DECIDE:` pseudocode out of the "Decision
/// tree (the contract)" fence, **interprets it** over all 32 input states, and
/// asserts the resulting routing table matches Stage -1's contract.
///
/// This one test replaces four separate fence-literal pins that previously
/// asserted the spelling of individual pseudocode lines:
///
/// - `if Mode C: use_subagent()` (was `sweep_md_documents_mode_c_short_circuit`)
/// - `elif --no-daemon: use_subagent()` (was a branch of
///   `sweep_md_documents_no_daemon_flag`, whose alternates
///   `elif NO_DAEMON: use_subagent()` and `PROBE_DAEMON skipped` were *also*
///   fence-only literals)
/// - `CLAIM_OWNED is set` (was a branch of `sweep_md_documents_claim_owned_flag`)
/// - `PROBE_DAEMON AND PROBE_POOL` (was
///   `sweep_md_documents_strict_and_precedence`'s strict-AND marker)
///
/// All four were literals whose ONLY occurrence in the skill docs is inside a
/// ```` ```text ```` fence, i.e. exactly the #7979 failure mode. What actually
/// matters is the *routing behaviour* — precedence order, and that the daemon
/// path needs a strict AND of both probes — and that is what is asserted here.
/// Rewording the fence (renaming `NO_DAEMON`, re-ordering comments, switching
/// `AND` to `∧`, re-indenting) cannot fail this test; relaxing the AND to an
/// OR, dropping a short-circuit, or reordering precedence will.
#[test]
fn stage_minus_one_decision_tree_routes_by_contract() {
    let content = read_sweep_md();
    let block = first_fenced_block_after(&content, "### Decision tree (the contract)", "text");
    let branches = parse_branches(&block, "DECIDE");

    assert!(
        branches.iter().any(|b| b.condition.is_none()),
        "the extracted Stage -1 decision tree has no `else:` universal \
         fallthrough — #3454 AC #1 requires one. Extracted block:\n{block}"
    );

    for inputs in Stage1Inputs::all() {
        let actual = run_branches(&branches, inputs);
        let expected = expected_decision(inputs);
        assert_eq!(
            actual, expected,
            "the Stage -1 decision tree in the sweep skill docs routes {inputs:?} \
             to `{actual}`, but the #3454 contract requires `{expected}`.\n\
             Contract: Mode C / --no-daemon / CLAIM_OWNED each short-circuit to \
             the subagent path BEFORE any probe; only a STRICT AND of \
             PROBE_DAEMON and PROBE_POOL reaches the daemon; everything else \
             falls through to the subagent.\nExtracted block:\n{block}"
        );
    }

    // Guard against a degenerate tree that routes everything to the subagent
    // path and would therefore satisfy the strict-AND half vacuously.
    let daemon_state = Stage1Inputs {
        mode_c: false,
        no_daemon: false,
        claim_owned: false,
        probe_daemon: true,
        probe_pool: true,
    };
    assert_eq!(
        run_branches(&branches, daemon_state),
        "use_daemon",
        "the extracted Stage -1 decision tree never reaches `use_daemon` — the \
         daemon dispatch path documented by #3454 would be unreachable"
    );
}

/// EXECUTABLE (#7994): extracts the `PROBE_DAEMON pseudocode` fence and
/// interprets its short-circuit guard over all 32 input states, asserting that
/// the probe is skipped **exactly** when `--no-daemon` or the daemon-owned-child
/// marker is present — and, structurally, that the `mcp__loom__list_sweeps`
/// round-trip appears only on the non-short-circuited `else:` path.
///
/// That ordering is the whole #3829/#4111 fix: a daemon-dispatched child must
/// never re-probe the daemon that spawned it. Previously this was covered only
/// by the fence-only literal `PROBE_DAEMON skipped` (a fallback branch of
/// `sweep_md_documents_no_daemon_flag`), which a reworded comment would break
/// while a genuinely *reordered* probe would sail through.
#[test]
fn stage_minus_one_probe_daemon_short_circuits_before_any_mcp_call() {
    let content = read_sweep_md();
    let block = first_fenced_block_after(
        &content,
        "#### PROBE_DAEMON — is the loom-daemon reachable?",
        "text",
    );

    let guard = block
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("if ").and_then(|r| r.strip_suffix(':')))
        .unwrap_or_else(|| {
            panic!(
                "the PROBE_DAEMON pseudocode fence has no top-level `if <cond>:` \
                 short-circuit guard — #3829/#4111 require one. Extracted \
                 block:\n{block}"
            )
        })
        .to_string();

    for inputs in Stage1Inputs::all() {
        let skipped = eval_condition(&guard, inputs);
        let expected = inputs.no_daemon || inputs.claim_owned;
        assert_eq!(
            skipped, expected,
            "PROBE_DAEMON's short-circuit guard evaluates to {skipped} for \
             {inputs:?}, but #3829/#4111 require the probe be skipped exactly \
             when `--no-daemon` is passed OR this session is a daemon-owned \
             child (LOOM_SWEEP_CLAIM_OWNED / --claim-owned). Extracted \
             guard: `{guard}`"
        );
    }

    let else_at = block.find("else:").unwrap_or_else(|| {
        panic!(
            "the PROBE_DAEMON pseudocode fence has no `else:` branch — the \
             reachability probe would be unreachable. Extracted block:\n{block}"
        )
    });
    let call_at = block.find("mcp__loom__list_sweeps").unwrap_or_else(|| {
        panic!(
            "the PROBE_DAEMON pseudocode fence never issues \
             `mcp__loom__list_sweeps` — that MCP call IS the reachability probe \
             (#3454). Extracted block:\n{block}"
        )
    });
    assert!(
        call_at > else_at,
        "the PROBE_DAEMON pseudocode issues `mcp__loom__list_sweeps` at or \
         before its `else:` branch — the daemon-owned-child short-circuit would \
         still make the circular round-trip #3829 exists to remove. Extracted \
         block:\n{block}"
    );
}

/// EXECUTABLE-adjacent (#7994): extracts the numeric `timeout_ms` the
/// PROBE_DAEMON pseudocode actually passes and asserts it is 500, then
/// cross-checks that the surrounding (unfenced) prose quotes the same figure.
///
/// The pre-#7994 test asserted the string `500ms`, which occurs both inside and
/// outside the fence. The value is what matters, and a doc that says "500ms" in
/// prose while passing `timeout_ms=2000` in the pseudocode is now a failure —
/// the old test passed happily in that state.
#[test]
fn stage_minus_one_probe_daemon_timeout_is_500ms() {
    let content = read_sweep_md();
    let block = first_fenced_block_after(
        &content,
        "#### PROBE_DAEMON — is the loom-daemon reachable?",
        "text",
    );

    let timeout = block
        .split("timeout_ms")
        .nth(1)
        .and_then(|rest| {
            let digits: String = rest
                .trim_start()
                .trim_start_matches('=')
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse::<u32>().ok()
        })
        .unwrap_or_else(|| {
            panic!(
                "could not extract a numeric `timeout_ms` from the PROBE_DAEMON \
                 pseudocode fence — #3454 AC #2 requires the probe carry an \
                 explicit timeout. Extracted block:\n{block}"
            )
        });

    assert_eq!(
        timeout, 500,
        "the PROBE_DAEMON reachability probe passes timeout_ms={timeout}, but \
         #3454 AC #2 pins a 500ms ceiling (the implicit-fallback semantic: a \
         stale socket or hung daemon is treated as unavailable after 500ms, \
         with no retry)"
    );

    // REVIEW-ONLY half: the operator-facing prose must quote the same figure.
    // `500ms` appears in unfenced prose ("Use a **500ms timeout** on this
    // probe"), so this needle is not a fence pin.
    assert!(
        content.contains("500ms"),
        "the sweep skill docs no longer state the `500ms` daemon-probe timeout in \
         prose, even though the PROBE_DAEMON pseudocode still passes \
         timeout_ms={timeout} — #3454 AC #2 requires the implicit fallback be \
         documented for the operator, not just encoded in the pseudocode"
    );
}

// ===========================================================================
// EXECUTABLE: the PROBE_POOL shell block, actually run
// ===========================================================================

/// Which account sources a [`run_probe_pool`] case materialises.
#[derive(Default)]
struct PoolCase {
    /// Number of `*.token` files created under `.loom/tokens/`.
    token_files: usize,
    /// `Some(n)` creates `.loom/accounts.env` with `n` `ACCOUNT_KEY_*` lines
    /// (`Some(0)` = the existing-but-empty file the block's own comment calls
    /// out as a bash-3.2 arithmetic hazard). `None` = no such file.
    repo_accounts_keys: Option<usize>,
    /// Same, for the legacy `.env` fallback.
    legacy_env_keys: Option<usize>,
    /// Same, for the claude-monitor master.
    monitor_keys: Option<usize>,
    /// Reach the monitor master through `$HOME/.claude-monitor` instead of an
    /// explicit `LOOM_CLAUDE_MONITOR_DIR`.
    monitor_via_home_default: bool,
    /// Same, for the opt-in home master.
    home_master_keys: Option<usize>,
    /// Export `LOOM_ACCOUNTS_ENV` (the #3704 opt-in). When false the home
    /// master exists on disk but must NOT be consulted.
    export_home_master: bool,
}

fn write_account_keys(path: &Path, count: usize) {
    fs::create_dir_all(path.parent().expect("account file has a parent"))
        .expect("create account source dir");
    let body: String = (0..count)
        .map(|i| format!("ACCOUNT_KEY_{i}=sk-test-{i}\n"))
        .collect();
    fs::write(path, body).expect("write account source");
}

/// Extracts the PROBE_POOL fence and RUNS it with real `bash` against a
/// synthetic account layout, returning what it decided.
fn run_probe_pool(case: &PoolCase) -> BashRun {
    let content = read_sweep_md();
    let block = first_fenced_block_after(
        &content,
        "#### PROBE_POOL — does a multi-account token pool exist?",
        "bash",
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let home = root.join("home");
    fs::create_dir_all(&home).expect("create fake HOME");

    if case.token_files > 0 {
        let tokens = root.join(".loom/tokens");
        fs::create_dir_all(&tokens).expect("create .loom/tokens");
        for i in 0..case.token_files {
            fs::write(tokens.join(format!("acct{i}.token")), "tok\n").expect("write token file");
        }
    }
    if let Some(n) = case.repo_accounts_keys {
        write_account_keys(&root.join(".loom/accounts.env"), n);
    }
    if let Some(n) = case.legacy_env_keys {
        write_account_keys(&root.join(".env"), n);
    }
    let monitor_dir = if case.monitor_via_home_default {
        home.join(".claude-monitor")
    } else {
        root.join("monitor")
    };
    if let Some(n) = case.monitor_keys {
        write_account_keys(&monitor_dir.join("accounts.env"), n);
    }
    let home_master = root.join("home-master.env");
    if let Some(n) = case.home_master_keys {
        write_account_keys(&home_master, n);
    }

    let mut env: Vec<(&str, String)> = vec![("HOME", home.display().to_string())];
    if !case.monitor_via_home_default {
        env.push(("LOOM_CLAUDE_MONITOR_DIR", monitor_dir.display().to_string()));
    }
    if case.export_home_master {
        env.push(("LOOM_ACCOUNTS_ENV", home_master.display().to_string()));
    }

    let script = format!(
        "{block}\n\
         echo \"RESULT_PROBE_POOL=$PROBE_POOL\"\n\
         echo \"RESULT_TOKEN_FILE_COUNT=$TOKEN_FILE_COUNT\"\n\
         echo \"RESULT_ENV_KEY_COUNT=$ENV_KEY_COUNT\"\n"
    );
    let run = run_bash_in(root, &script, &env, POOL_ENV_LEAKS);
    // `tmp` must outlive the run; returning `run` drops it here on purpose.
    drop(tmp);
    run
}

/// EXECUTABLE (#7994): runs the extracted PROBE_POOL block and asserts the
/// `>= 2 accounts` gate behaviourally, across every documented account source.
///
/// This is the AC #4 contract ("no behaviour change for solo-token operators")
/// that the pre-#7994 `sweep_md_documents_smoke_test_recipes` asserted with the
/// fence-only literal `< 2 ACCOUNT_KEY_` — a smoke-test *comment*. A reworded
/// comment broke the build; a block that silently started accepting a pool of
/// one did not. Now the reverse is true.
#[test]
fn stage_minus_one_probe_pool_requires_at_least_two_accounts() {
    // (case, expected PROBE_POOL, what the case proves)
    let cases: Vec<(PoolCase, bool, &str)> = vec![
        (PoolCase::default(), false, "no accounts configured at all → not a pool"),
        (
            PoolCase {
                token_files: 1,
                ..Default::default()
            },
            false,
            "a single materialised token is NOT a pool (the `>= 2`, not `>= 1`, gate)",
        ),
        (
            PoolCase {
                token_files: 2,
                ..Default::default()
            },
            true,
            "two materialised tokens are a pool",
        ),
        (
            PoolCase {
                repo_accounts_keys: Some(1),
                ..Default::default()
            },
            false,
            "one configured ACCOUNT_KEY_ line is NOT a pool",
        ),
        (
            PoolCase {
                repo_accounts_keys: Some(2),
                ..Default::default()
            },
            true,
            "two ACCOUNT_KEY_ lines in .loom/accounts.env are a pool even with no \
             token files bootstrapped yet",
        ),
        (
            PoolCase {
                repo_accounts_keys: Some(1),
                monitor_keys: Some(1),
                ..Default::default()
            },
            true,
            "the configured count is SUMMED across merged sources (repo + \
             claude-monitor master)",
        ),
        (
            PoolCase {
                monitor_keys: Some(2),
                monitor_via_home_default: true,
                ..Default::default()
            },
            true,
            "the claude-monitor master is found at $HOME/.claude-monitor when \
             LOOM_CLAUDE_MONITOR_DIR is unset",
        ),
        (
            PoolCase {
                legacy_env_keys: Some(2),
                ..Default::default()
            },
            true,
            "the legacy .env fallback is consulted when .loom/accounts.env is absent",
        ),
        (
            PoolCase {
                repo_accounts_keys: Some(0),
                legacy_env_keys: Some(2),
                ..Default::default()
            },
            false,
            "an existing-but-EMPTY .loom/accounts.env wins over the legacy .env \
             (source precedence) and contributes exactly 0 — the `grep -c` \
             bash-3.2 hazard the block's own comment documents",
        ),
        (
            PoolCase {
                home_master_keys: Some(2),
                export_home_master: false,
                ..Default::default()
            },
            false,
            "the opt-in home master is NOT consulted unless LOOM_ACCOUNTS_ENV is \
             set (#3704)",
        ),
        (
            PoolCase {
                home_master_keys: Some(2),
                export_home_master: true,
                ..Default::default()
            },
            true,
            "the opt-in home master IS consulted once LOOM_ACCOUNTS_ENV is set (#3704)",
        ),
    ];

    for (case, expected, why) in cases {
        let run = run_probe_pool(&case);
        assert_eq!(
            run.status, 0,
            "the extracted PROBE_POOL block exited {} for the case `{why}` — the \
             probe must be cheap, local, and side-effect-free, never an abort.\n\
             stdout:\n{}\nstderr:\n{}",
            run.status, run.stdout, run.stderr
        );
        let actual = run.captured("RESULT_PROBE_POOL");
        assert_eq!(
            actual,
            expected.to_string(),
            "the extracted PROBE_POOL block decided PROBE_POOL={actual} but the \
             #3454 contract requires {expected}: {why}.\n\
             TOKEN_FILE_COUNT={}, ENV_KEY_COUNT={}\nstderr:\n{}",
            run.captured("RESULT_TOKEN_FILE_COUNT"),
            run.captured("RESULT_ENV_KEY_COUNT"),
            run.stderr
        );
    }
}

/// EXECUTABLE (#7994): the "configured but not bootstrapped" discoverability
/// signal fires exactly when the merged sources declare a pool while
/// `.loom/tokens/` has fewer than two token files — and NOT on every subagent
/// fallthrough. Previously unasserted (the pre-#7994 file had no coverage of
/// this branch at all).
#[test]
fn stage_minus_one_probe_pool_warns_only_when_configured_but_not_bootstrapped() {
    let hint = "loom-daemon tokens bootstrap";

    let not_bootstrapped = run_probe_pool(&PoolCase {
        repo_accounts_keys: Some(2),
        ..Default::default()
    });
    assert!(
        not_bootstrapped.stderr.contains(hint),
        "the extracted PROBE_POOL block did not emit the \
         configured-but-not-bootstrapped hint on stderr when 2 ACCOUNT_KEY_ \
         lines exist but .loom/tokens/ is empty.\nstderr:\n{}",
        not_bootstrapped.stderr
    );

    let already_bootstrapped = run_probe_pool(&PoolCase {
        token_files: 2,
        repo_accounts_keys: Some(2),
        ..Default::default()
    });
    assert!(
        !already_bootstrapped.stderr.contains(hint),
        "the extracted PROBE_POOL block emitted the \
         configured-but-not-bootstrapped hint even though .loom/tokens/ already \
         holds 2 token files — the signal must not fire on an already-\
         bootstrapped pool.\nstderr:\n{}",
        already_bootstrapped.stderr
    );

    let solo = run_probe_pool(&PoolCase {
        token_files: 1,
        repo_accounts_keys: Some(1),
        ..Default::default()
    });
    assert!(
        !solo.stderr.contains(hint),
        "the extracted PROBE_POOL block nagged a genuine single-token operator — \
         the hint must fire only when the merged sources DECLARE a pool \
         (ENV_KEY_COUNT >= 2), not on every subagent fallthrough.\nstderr:\n{}",
        solo.stderr
    );
}

// ===========================================================================
// EXECUTABLE: the wave-size shell blocks, run against the REAL disk lib
// ===========================================================================

/// Extracts both "Resolve auto wave size" shell fences and RUNS them against
/// the **real** `defaults/scripts/lib/disk-headroom.sh`, with only the OS-tool
/// boundary (`git`, `df`) stubbed so the outcome is deterministic.
///
/// `mapfile` / `readarray` are **disabled** (`enable -n`) for the whole run, so
/// the #3765 bash-3.2-portability regression guard is now *executable*: if the
/// documented capture pattern ever reaches for a bash-4.0+ array builtin, the
/// extracted block dies with `command not found` instead of being caught by a
/// whole-document text scan that a mere prose mention of "mapfile" could trip.
fn run_wave_size(decide: &str, cand: u32, df_mode: &str, avail_gb: u64) -> BashRun {
    let content = read_sweep_md();
    let anchor = "### Resolve auto wave size";
    let probe_block = nth_fenced_block_after(&content, anchor, "bash", 0);
    let resolve_block = nth_fenced_block_after(&content, anchor, "bash", 1);

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // Real Loom shell libs, at the path the documented snippet sources.
    let lib_dst = root.join(".loom/scripts/lib");
    fs::create_dir_all(&lib_dst).expect("create .loom/scripts/lib");
    let lib_src = repo_root().join("defaults/scripts/lib");
    for entry in fs::read_dir(&lib_src).expect("read defaults/scripts/lib") {
        let entry = entry.expect("dir entry");
        if entry.path().is_file() {
            fs::copy(entry.path(), lib_dst.join(entry.file_name())).expect("copy lib");
        }
    }

    // Stub only the OS-tool boundary.
    let stub_bin = root.join("stubbin");
    write_executable_stub(
        &stub_bin,
        "git",
        &format!(
            "#!/usr/bin/env bash\n\
             if [[ \"$1\" == rev-parse && \"$2\" == --show-toplevel ]]; then\n\
             \x20 printf '%s\\n' '{}'\n\
             \x20 exit 0\n\
             fi\n\
             exit 1\n",
            root.display()
        ),
    );
    write_executable_stub(
        &stub_bin,
        "df",
        "#!/usr/bin/env bash\n\
         if [[ \"${LOOM_TEST_DF_MODE:-ok}\" == fail ]]; then\n\
         \x20 echo 'df: stub failure' >&2\n\
         \x20 exit 1\n\
         fi\n\
         echo 'Filesystem 1024-blocks Used Available Capacity Mounted on'\n\
         echo \"/dev/stub 209715200 0 ${LOOM_TEST_DF_AVAIL_K:-0} 1% /\"\n",
    );

    let path = format!("{}:{}", stub_bin.display(), std::env::var("PATH").unwrap_or_default());

    let script = format!(
        "# #3765: prove the documented capture pattern is bash-3.2 portable by\n\
         # removing the bash-4.0+ array builtins entirely.\n\
         enable -n mapfile readarray 2>/dev/null || true\n\
         DECIDE={decide}\n\
         CAND={cand}\n\
         {probe_block}\n\
         {resolve_block}\n\
         echo \"RESULT_DISK_PROBE_OK=$DISK_PROBE_OK\"\n\
         echo \"RESULT_MECH=$MECH\"\n\
         echo \"RESULT_WAVE_SIZE=$WAVE_SIZE\"\n\
         echo \"RESULT_REASON=$REASON\"\n"
    );

    let env: Vec<(&str, String)> = vec![
        ("PATH", path),
        ("HOME", root.display().to_string()),
        ("LOOM_TEST_DF_MODE", df_mode.to_string()),
        ("LOOM_TEST_DF_AVAIL_K", (avail_gb * 1024 * 1024).to_string()),
        // Pin the operator-overridable knobs so the assertions are exact. The
        // doc's own `:=` contract (an operator-set cap always wins) is asserted
        // by observing that this value survives.
        ("LOOM_SUBAGENT_WAVE_CAP", "4".to_string()),
        ("LOOM_PER_WORKTREE_GB", "2".to_string()),
    ];
    let run = run_bash_in(root, &script, &env, POOL_ENV_LEAKS);
    drop(tmp);
    run
}

/// EXECUTABLE (#7994): the documented wave-size resolution, extracted and run.
///
/// Replaces `sweep_md_stage_minus_one_wave_size_snippet_is_bash_3_2_portable`,
/// which pinned the helper name `loom_wave_size_from_disk` as a literal and
/// scanned the *entire* skill text for the strings `mapfile -` / `readarray -`.
/// Both halves are now behavioural: the block is executed against the real
/// `disk-headroom.sh` (so a renamed or missing helper fails loudly on its own),
/// and the bash-4-only builtins are *disabled* for the run rather than
/// text-scanned (so a prose mention of "mapfile" is harmless while a real
/// reintroduction fails).
///
/// Also covers the #4164 "unknown != zero" contract, which the pre-#7994 file
/// asserted nowhere: a failing `df` must skip the disk clamp entirely rather
/// than feeding a fake `0` into the helper.
/// One wave-size scenario: the inputs to feed the extracted blocks, and the
/// behaviour the documented recipe must produce.
struct WaveCase {
    decide: &'static str,
    cand: u32,
    df_mode: &'static str,
    avail_gb: u64,
    mech: &'static str,
    size: u32,
    reason: &'static str,
    why: &'static str,
}

#[test]
fn stage_minus_one_wave_size_block_resolves_against_real_disk_lib() {
    let cases = [
        WaveCase {
            decide: "use_subagent",
            cand: 5,
            df_mode: "ok",
            avail_gb: 100,
            mech: "subagent",
            size: 4,
            reason: "target",
            why: "plentiful disk → the operator-set LOOM_SUBAGENT_WAVE_CAP wins \
                  (the `:=` only fills an unset value)",
        },
        WaveCase {
            decide: "use_subagent",
            cand: 2,
            df_mode: "ok",
            avail_gb: 100,
            mech: "subagent",
            size: 2,
            reason: "candidates",
            why: "fewer candidates than the target → clamped to the candidate count",
        },
        WaveCase {
            decide: "use_subagent",
            cand: 5,
            df_mode: "ok",
            avail_gb: 4,
            mech: "subagent",
            size: 2,
            reason: "disk",
            why: "a tight scratch volume clamps the wave and reports reason `disk` \
                  — proves the two-line helper stdout was split correctly without \
                  `mapfile`",
        },
        WaveCase {
            decide: "use_subagent",
            cand: 5,
            df_mode: "ok",
            avail_gb: 0,
            mech: "subagent",
            size: 1,
            reason: "floor",
            why: "a genuinely full disk floors the wave at 1 (never 0) with reason \
                  `floor`",
        },
        WaveCase {
            decide: "use_daemon",
            cand: 25,
            df_mode: "ok",
            avail_gb: 100,
            mech: "daemon",
            size: 10,
            reason: "target",
            why: "the daemon path scales to its own target of 10, not the subagent cap",
        },
        WaveCase {
            decide: "use_daemon",
            cand: 3,
            df_mode: "fail",
            avail_gb: 0,
            mech: "daemon",
            size: 3,
            reason: "unknown",
            why: "#4164: an unmeasurable df SKIPS the disk clamp (reason `unknown`, \
                  K = min(target, CAND)) instead of masquerading as a full disk",
        },
    ];

    for WaveCase {
        decide,
        cand,
        df_mode,
        avail_gb,
        mech,
        size,
        reason,
        why,
    } in cases
    {
        let run = run_wave_size(decide, cand, df_mode, avail_gb);
        assert_eq!(
            run.status, 0,
            "the extracted wave-size blocks exited {} for the case `{why}`.\n\
             stdout:\n{}\nstderr:\n{}",
            run.status, run.stdout, run.stderr
        );
        assert_eq!(
            run.captured("RESULT_DISK_PROBE_OK"),
            if df_mode == "fail" { "false" } else { "true" },
            "DISK_PROBE_OK is wrong for the case `{why}` — the snippet must \
             capture `loom_worktree_root_free_gb`'s EXIT STATUS, not assume a \
             printed value (#4164).\nstderr:\n{}",
            run.stderr
        );
        assert_eq!(run.captured("RESULT_MECH"), mech, "MECH is wrong for the case `{why}`");
        assert_eq!(
            run.captured("RESULT_WAVE_SIZE"),
            size.to_string(),
            "WAVE_SIZE is wrong for the case `{why}`.\nstdout:\n{}\nstderr:\n{}",
            run.stdout,
            run.stderr
        );
        assert_eq!(
            run.captured("RESULT_REASON"),
            reason,
            "the reason token is wrong for the case `{why}` — the operator-facing \
             one-line log maps off it.\nstdout:\n{}\nstderr:\n{}",
            run.stdout,
            run.stderr
        );
    }
}

// ===========================================================================
// REVIEW-ONLY + real-code cross-checks: the two daemon-path CLI flags
// ===========================================================================

/// REVIEW-ONLY (#7994): `--no-daemon` is a stable CLI flag name, and the `>= 3`
/// occurrence floor is a structural presence check (optional-flags list +
/// validation rules + Stage -1 section) that fails if the flag is dropped from
/// any of its documented sites. 22 of its 30 occurrences are in unfenced prose,
/// so this is not a fence pin.
///
/// The flag's *decision-tree semantics* — that it short-circuits to the
/// subagent path ahead of the daemon probe — moved to the executable
/// [`stage_minus_one_decision_tree_routes_by_contract`] and
/// [`stage_minus_one_probe_daemon_short_circuits_before_any_mcp_call`]. The
/// pre-#7994 version of this test asserted that semantics with an or-chain of
/// three needles (`elif --no-daemon: use_subagent()`,
/// `elif NO_DAEMON: use_subagent()`, `PROBE_DAEMON skipped`) **every branch of
/// which was a fence-only literal** — a reworded pseudocode comment took `main`
/// red while a genuinely reordered decision tree passed.
#[test]
fn sweep_md_documents_no_daemon_flag() {
    let content = read_sweep_md();
    let occurrences = content.matches("--no-daemon").count();
    assert!(
        occurrences >= 3,
        "the sweep skill docs mention `--no-daemon` only {occurrences} time(s); \
         #3454 AC #2 requires the flag be documented in the optional-flags \
         section, the validation rules, and the Stage -1 section (lower bound: 3 \
         occurrences)"
    );
}

/// REVIEW-ONLY + real-code cross-check (#7994): the `--claim-owned <N>`
/// occurrence floor and the backward-compatible `LOOM_SWEEP_CLAIM_OWNED` env
/// var are structural/unfenced checks; the flag *name* is additionally
/// cross-checked against the daemon code that actually emits it, so a rename on
/// either side is caught by the other.
///
/// The fence-only literal this replaces was `CLAIM_OWNED is set` (the
/// decision-tree pseudocode token); its behaviour is now asserted executably by
/// [`stage_minus_one_decision_tree_routes_by_contract`].
#[test]
fn sweep_md_documents_claim_owned_flag() {
    let content = read_sweep_md();

    let occurrences = content.matches("--claim-owned").count();
    assert!(
        occurrences >= 4,
        "the sweep skill docs mention `--claim-owned` only {occurrences} time(s); \
         #4111 requires the flag be documented in the optional-flags section, the \
         validation rules, the Stage -1 decision tree, and Stage -1 prose (lower \
         bound: 4 occurrences)"
    );

    assert!(
        content.contains("LOOM_SWEEP_CLAIM_OWNED"),
        "the sweep skill docs must still document `LOOM_SWEEP_CLAIM_OWNED` \
         alongside `--claim-owned` — #4111 keeps the env var exported for \
         backward compatibility, it does not replace it"
    );

    // Real-code cross-check: the flag the doc describes must be the flag the
    // daemon actually puts in a dispatched child's prompt.
    let dispatch_src = repo_root().join("loom-daemon/src/sweep_registry/dispatch.rs");
    let dispatch = fs::read_to_string(&dispatch_src).unwrap_or_else(|e| {
        panic!(
            "cannot read {} to cross-check the `--claim-owned` flag name: {e}",
            dispatch_src.display()
        )
    });
    assert!(
        dispatch.contains("--claim-owned"),
        "`loom-daemon`'s dispatch no longer emits `--claim-owned` in a child \
         sweep's prompt, but the sweep skill docs still document it as the \
         daemon-owned-child marker (#4111). One side was renamed without the \
         other: {}",
        dispatch_src.display()
    );
}

/// REVIEW-ONLY (#7994): `mcp__loom__dispatch_sweep` is the exact MCP tool name
/// the daemon path consumes (Phase A, #3452). 18 of its 26 occurrences are in
/// unfenced prose, so this is not a fence pin, and there is no executable
/// surface from a Rust integration test for "the skill calls this MCP tool" —
/// the call is made by the LLM running the skill, not by any binary in this
/// repo.
#[test]
fn sweep_md_references_dispatch_sweep_mcp_tool() {
    let content = read_sweep_md();
    assert!(
        content.contains("mcp__loom__dispatch_sweep"),
        "the sweep skill docs are missing the `mcp__loom__dispatch_sweep` MCP \
         tool reference — #3454 requires the daemon dispatch path consume Phase \
         A's tool (#3452)"
    );
}

/// REVIEW-ONLY (#7994): the sub-2-second daemon-path exit expectation is
/// LLM-directed orchestration prose about the *session's* behaviour — there is
/// no binary in this repo whose exit latency a Rust test could measure, so no
/// executable surface exists. Kept as a tolerant any-of presence check (the
/// `sub-2-second` spelling occurs twice in unfenced prose, so a rewrite of the
/// smoke-test fence cannot break it) rather than deleted, so the invariant
/// stays visible to a reviewer.
///
/// The other half of the pre-#7994 test — the `< 2 ACCOUNT_KEY_` / single-token
/// fallthrough contract (AC #4) — DID have an executable surface and moved to
/// [`stage_minus_one_probe_pool_requires_at_least_two_accounts`], which runs the
/// real PROBE_POOL block instead of pinning a smoke-test comment.
#[test]
fn sweep_md_documents_daemon_path_fast_exit_expectation() {
    let content = read_sweep_md();
    assert!(
        content.contains("< 2 seconds")
            || content.contains("sub-2-second")
            || content.contains("Sub-2-second"),
        "the sweep skill docs no longer state the sub-2-second exit expectation \
         for the daemon dispatch path — #3454 AC #3 requires it be documented"
    );
}

/// REVIEW-ONLY (#7994, AC #2 downgrade): the Limitations table records the
/// Stage -1 row as Implemented (#3454) — the operator-visible status flip that
/// signals Phase D shipped. This is a **table/structure** assertion with no
/// executable surface: the table is a human-readable status ledger, not a
/// machine-read manifest, so nothing can be located-extracted-executed from it.
/// Rather than delete the invariant silently, it stays here as a review-only
/// check, prefix-tolerant so appended issue refs (e.g. `(#3454, #3829)`) do not
/// break it (#3833/#3837). Both needles live in unfenced table rows.
#[test]
fn sweep_md_limitations_table_records_stage_minus_one_implemented() {
    let content = read_sweep_md();
    assert!(
        content.contains("Daemon backend detection") && content.contains("Implemented (#3454"),
        "the sweep skill docs' Limitations table is missing the `Daemon backend \
         detection | Implemented (#3454...` row — #3454 AC #1 requires the \
         operator-visible status flip (tolerates appended issue refs, e.g. #3829)"
    );
}
