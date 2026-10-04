//! Unit tests for the removal gate. The differential
//! (`tests/merge_pr_remove_gate_differential.rs`) compares against the frozen
//! retired shell; these name the individual properties.

use super::*;

const PORCELAIN: &str = "worktree /repo/main\nHEAD aaaa\nbranch refs/heads/main\n\nworktree /repo/wt\nHEAD bbbb\nbranch refs/heads/feature/issue-1\n\n";

fn ctx(real: &str, allow: bool, sentinel: bool) -> Context<'_> {
    Context {
        porcelain: PORCELAIN,
        path: real,
        real,
        allow_unmanaged: allow,
        sentinel_present: sentinel,
    }
}

#[test]
fn the_primary_checkout_is_refused_even_with_sentinel_and_opt_in() {
    let (v, lines) = decide(&ctx("/repo/main", true, true));
    assert_eq!(v, Verdict::Refuse);
    assert!(lines[0]
        .1
        .starts_with("Refusing to remove the primary/main worktree at /repo/main"));
}

#[test]
fn a_linked_managed_worktree_proceeds_silently() {
    let (v, lines) = decide(&ctx("/repo/wt", false, true));
    assert_eq!(v, Verdict::Proceed);
    assert!(lines.is_empty());
}

#[test]
fn an_unmarked_worktree_is_refused_unless_opted_in() {
    let (v, lines) = decide(&ctx("/repo/wt", false, false));
    assert_eq!(v, Verdict::Refuse);
    assert!(lines[0].1.contains("lacks .loom-managed sentinel"));
    let (v, lines) = decide(&ctx("/repo/wt", true, false));
    assert_eq!(v, Verdict::Proceed);
    assert_eq!(lines[0].0, Level::Info);
    assert!(lines[0].1.starts_with("Bypassing sentinel guard"));
}

#[test]
fn empty_porcelain_means_not_primary() {
    let c = Context {
        porcelain: "",
        path: "/x",
        real: "/x",
        allow_unmanaged: false,
        sentinel_present: true,
    };
    assert_eq!(decide(&c).0, Verdict::Proceed);
}

#[test]
fn render_is_token_then_level_tab_message() {
    let (v, lines) = decide(&ctx("/repo/wt", true, false));
    assert_eq!(
        render(v, &lines),
        "LOOM-REMOVE-GATE PROCEED\nINFO\tBypassing sentinel guard (--worktree-path explicit opt-in for /repo/wt)\n"
    );
}
