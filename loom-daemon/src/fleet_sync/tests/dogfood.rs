//! The workspace resync over the Loom source repo's gitignored dogfood
//! symlinks (#11681): healed by a write pass, never by a pass that may not
//! write.

use super::*;

#[test]
fn the_source_repo_gets_its_missing_dogfood_symlinks_and_stays_clean() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let source = fx.clone_as("loom-source");
    write(&source.join(".loom-source"), "");
    write(&source.join("defaults/.claude/commands/loom/builder.md"), "x");
    write(&source.join("defaults/.claude/agents/a.md"), "x");
    // In the real source repo the dogfood paths are untracked (gitignored).
    let _ = fs::remove_dir_all(source.join(".claude"));
    for args in [
        vec!["add", "-A", ".claude"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "untrack",
        ],
    ] {
        let st = Command::new("git")
            .args(&args)
            .current_dir(&source)
            .status()
            .unwrap();
        assert!(st.success());
    }
    let exclude = source.join(".git/info/exclude");
    let mut ex = fs::read_to_string(&exclude).unwrap_or_default();
    ex.push_str(".loom-source\ndefaults/\n.claude/\n");
    fs::write(&exclude, ex).unwrap();
    let roots = [source.clone()];
    for _ in 0..2 {
        let pass = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
        let w = &pass.workspaces[0];
        assert_eq!(w.state, WState::Skipped);
        assert_eq!(reason(w), "the Loom source repo installs from its own tree");
        for link in [".claude/commands/loom", ".claude/agents"] {
            let m = fs::symlink_metadata(source.join(link)).unwrap();
            assert!(m.file_type().is_symlink(), "{link}");
            assert!(source.join(link).is_dir(), "{link} resolves");
        }
    }
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&source)
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&status.stdout);
    assert!(out.trim().is_empty(), "dirty: {out}");
}

#[test]
fn a_check_mode_pass_does_not_create_the_source_repos_dogfood_symlinks() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let source = fx.clone_as("loom-source");
    write(&source.join(".loom-source"), "");
    write(&source.join("defaults/.claude/commands/loom/builder.md"), "x");
    write(&source.join("defaults/.claude/agents/a.md"), "x");
    let _ = fs::remove_dir_all(source.join(".claude/commands/loom"));
    let _ = fs::remove_dir_all(source.join(".claude/agents"));
    let roots = [source.clone()];
    let pass = host.pass_over(&roots, Mode::Check, &|| Ok(()), None);
    assert_eq!(pass.workspaces[0].state, WState::Skipped);
    for link in [".claude/commands/loom", ".claude/agents"] {
        assert!(fs::symlink_metadata(source.join(link)).is_err(), "{link} written");
    }
}

#[test]
fn the_source_repo_keeps_a_real_commands_dir_it_cannot_safely_replace() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let source = fx.clone_as("loom-source");
    write(&source.join(".loom-source"), "");
    write(&source.join("defaults/.claude/commands/loom/builder.md"), "x");
    write(&source.join(".claude/commands/loom/local-only.md"), "mine");
    let roots = [source.clone()];
    host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
    let dir = source.join(".claude/commands/loom");
    assert!(!fs::symlink_metadata(&dir).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(dir.join("local-only.md")).unwrap(), "mine");
}
