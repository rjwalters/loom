//! Dispatch **argv/prompt shape** tests — what `spawn_child` actually hands
//! to `spawn-claude.sh`, asserted positionally.
//!
//! Home for every test whose subject is the shape of the child argv rather
//! than dispatch *behavior*: the `-p` payload is a bare slash-command
//! reference, and the flags that follow it sit in a fixed order.
//! `dispatch_appends_dangerously_skip_permissions` (#3824/#4111) moved here
//! from `dispatch/tests.rs` because it is the same subject as the #8065
//! tests below and reads better beside them.
//!
//! Split out as a sibling file rather than appended to `dispatch/tests.rs`:
//! that module is over the 1000-line ratchet threshold and frozen at its
//! current size (see `.loom/docs/file-size-policy.md`). Registered from its
//! foot via `#[path]`, the shape `guards.rs` already uses for
//! `guards_union_tests.rs` / `guards_preflip_tests.rs`.
//!
//! The #8065 finding these guard is written up in
//! `defaults/docs/prompt-prefix-loading.md`.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use tempfile::tempdir;

/// Extract the value of the `-p` argv token from a `fixture_registry`
/// record log, using the per-token `arg: <tok>` lines (never the flattened
/// `argv:` line, which cannot distinguish a space inside the prompt from an
/// argument boundary — exactly the #4111 failure mode).
fn recorded_prompt_arg(recorded: &str) -> String {
    let mut toks = recorded
        .lines()
        .filter_map(|l| l.strip_prefix("arg: "))
        .skip_while(|t| *t != "-p");
    toks.next().expect("record log has no `arg: -p` token");
    toks.next()
        .expect("`arg: -p` is the last token — no prompt value followed it")
        .to_string()
}

/// Issue #8065: the `-p` payload is a bare **slash-command reference**, never
/// an expansion of one.
///
/// `sweep.md` documents a per-sibling "Load when" table (`sweep-examples.md`
/// "never required", `sweep-mode-c-lifecycle.md` "Mode C only", …) and relies
/// on the runtime fetching each sibling on demand via the `Skill` tool. That
/// contract is only real if nothing upstream pre-concatenates the family into
/// the initial prompt. #8065 traced the whole chain and confirmed it does not
/// (see `defaults/docs/prompt-prefix-loading.md` for the trace and the
/// transcript measurements); this test is the Loom-side regression guard for
/// that finding.
///
/// Asserted structurally, not by substring: the `-p` token must be *exactly*
/// the short reference string, so any future "helpful" inlining — of a
/// sibling body, of `CLAUDE.md`, of the issue body — fails here rather than
/// silently re-inflating every sweep's turn-1 context (the pre-#7726 monolith
/// cost ~150k tokens per session by exactly this shape).
#[test]
#[serial]
fn dispatch_prompt_is_a_bare_slash_command_reference() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(8065), None, None, None, None)
        .expect("dispatch should succeed");
    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);

    let prompt = recorded_prompt_arg(&recorded);
    assert_eq!(
        prompt, "/loom:sweep 8065 --claim-owned 8065",
        "the Issue-dispatch `-p` payload must be the bare slash-command \
         reference and nothing else; got: {prompt}"
    );
    // A reference, by construction, cannot carry a file body. Both bounds are
    // deliberately generous — this catches wholesale inlining, not a future
    // extra flag.
    assert!(
        prompt.len() < 200,
        "the `-p` payload grew to {} bytes — a dispatch prompt this large is \
         carrying file content, not a slash-command reference (#8065); got: {prompt}",
        prompt.len()
    );
    assert!(
        !prompt.contains("Load when") && !prompt.contains("sweep-wave-lifecycle"),
        "the `-p` payload contains `sweep.md`/sibling body text — progressive \
         disclosure (#7726/#8065) is defeated by inlining; got: {prompt}"
    );
}

/// Issue #8065, Mode C half: the PR-set dispatch prompt is a bare reference
/// too. Kept as its own test because `SweepKind::PrSet` takes a different
/// `match` arm in `spawn_child` — a regression could land on one arm only.
#[test]
#[serial]
fn dispatch_prset_prompt_is_a_bare_slash_command_reference() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::PrSet(vec![8065, 8073]), None, None, None, None)
        .expect("dispatch should succeed");
    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);

    let prompt = recorded_prompt_arg(&recorded);
    assert_eq!(
        prompt, "/loom:sweep --prs 8065 8073",
        "the PrSet-dispatch `-p` payload must be the bare slash-command \
         reference and nothing else; got: {prompt}"
    );
    assert!(
        !prompt.contains("Load when") && !prompt.contains("sweep-mode-c-lifecycle"),
        "the Mode C `-p` payload contains sibling body text — progressive \
         disclosure (#7726/#8065) is defeated by inlining; got: {prompt}"
    );
}
/// Issue #3824: `spawn_child` unconditionally appends
/// `--dangerously-skip-permissions` to the child argv so a detached,
/// non-interactive `claude -p` sweep never stalls on a permission prompt.
/// With no model/effort/depends-on the flag directly follows the
/// `--claim-owned <N>` marker (#4111, always emitted for a daemon
/// dispatch), appended AFTER it (verified by the exact positional form).
#[test]
#[serial]
fn dispatch_appends_dangerously_skip_permissions() {
    let dir = tempdir().unwrap();
    let (mut registry, record_log) = fixture_registry(dir.path());

    let outcome = registry
        .dispatch(&SweepKind::Issue(4242), None, None, None, None)
        .expect("dispatch should succeed");

    let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record_log, &needle);
    assert!(
        recorded.contains(
            "argv: -p /loom:sweep 4242 --claim-owned 4242 --dangerously-skip-permissions"
        ),
        "expected --claim-owned then --dangerously-skip-permissions appended after the \
             prompt; got: {recorded}"
    );
}
