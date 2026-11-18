//! `ForgeAction::is_read` against the clap tree and the capability list
//! (#14395): a verb is served by `loom-tools` exactly when
//! [`TOOLS_COMMANDS`] names it, and every verb `ForgeAction` declares is
//! classified one way or the other.

use clap::{Args, Command, FromArgMatches};

use super::{ForgeReadCli, EX_WRITE_VERB_NOT_MIGRATED};
use crate::cli::whole_cli_parse::{on_stack, MAIN_THREAD_STACK};
use crate::install_compat::TOOLS_COMMANDS;

/// The `forge` command exactly as `loom-tools` mounts it, fully built (the
/// `merge-queue` verbs are deferred until `build`).
fn forge_command() -> Command {
    let mut cmd = ForgeReadCli::augment_args(Command::new("forge"));
    cmd.build();
    cmd
}

/// An argv that satisfies every required argument of `cmd` with a value that
/// parses as a number, a string or a path. Only classification matters here;
/// nothing runs.
fn argv_for(path: &[&str], cmd: &Command) -> Vec<String> {
    let mut argv: Vec<String> = vec!["forge".into()];
    argv.extend(path.iter().map(|p| (*p).to_string()));
    for arg in cmd.get_arguments().filter(|a| a.is_required_set()) {
        if arg.is_positional() {
            argv.push("1".into());
        } else if let Some(long) = arg.get_long() {
            argv.push(format!("--{long}"));
            if arg.get_action().takes_values() {
                argv.push("1".into());
            }
        }
    }
    // `required_unless_present` is conditional, so `is_required_set` misses it.
    if path == ["rerun"] {
        argv.push("1".into());
    }
    argv
}

fn parse(argv: &[String]) -> ForgeReadCli {
    let argv = argv.to_vec();
    on_stack("forge-read-classify-8MiB", MAIN_THREAD_STACK, move || {
        let matches = forge_command()
            .try_get_matches_from(&argv)
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        ForgeReadCli::from_arg_matches(&matches).expect("from_arg_matches")
    })
}

/// Every `(verb path, is_read_expected)` the clap tree declares: leaf commands
/// only, so `merge-queue` and `parent` contribute their sub-verbs.
fn declared_verbs() -> Vec<(Vec<String>, Command)> {
    on_stack("forge-read-verbs-8MiB", MAIN_THREAD_STACK, || {
        let root = forge_command();
        let mut out = Vec::new();
        for sub in root.get_subcommands().filter(|c| c.get_name() != "help") {
            if sub.has_subcommands() {
                for leaf in sub.get_subcommands().filter(|c| c.get_name() != "help") {
                    out.push((
                        vec![sub.get_name().to_string(), leaf.get_name().to_string()],
                        leaf.clone(),
                    ));
                }
            } else {
                out.push((vec![sub.get_name().to_string()], sub.clone()));
            }
        }
        out
    })
}

#[test]
fn is_read_matches_the_capability_list_for_every_declared_verb() {
    let verbs = declared_verbs();
    assert!(verbs.len() > 40, "the forge tree has lost verbs: {}", verbs.len());
    for (path, cmd) in verbs {
        let names: Vec<&str> = path.iter().map(String::as_str).collect();
        let listed = TOOLS_COMMANDS.contains(&format!("forge {}", names.join(" ")).as_str());
        let cli = parse(&argv_for(&names, &cmd));
        assert_eq!(
            cli.is_read(),
            listed,
            "`forge {}`: is_read() and TOOLS_COMMANDS disagree",
            names.join(" ")
        );
        if names.len() == 1 {
            assert_eq!(cli.verb(), names[0], "verb() drifted from the clap name");
        }
    }
}

#[test]
fn every_listed_forge_command_is_a_declared_verb() {
    let declared: Vec<String> = declared_verbs()
        .into_iter()
        .map(|(path, _)| format!("forge {}", path.join(" ")))
        .collect();
    for listed in TOOLS_COMMANDS.iter().filter(|c| c.starts_with("forge ")) {
        assert!(declared.iter().any(|d| d == listed), "{listed} is not a forge verb");
    }
}

#[test]
fn the_write_verbs_are_refused_and_the_split_verbs_are_split() {
    for (argv, read) in [
        (&["comment", "1", "--body", "x"][..], false),
        (&["create-issue", "--title", "t", "--body", "b"], false),
        (&["auto-merge", "1"], false),
        (&["merge-queue", "enqueue", "1", "--approved-sha", "a"], false),
        (&["merge-queue", "status", "1"], true),
        (&["parent", "link", "--parent", "1", "--child", "2"], false),
        (&["parent", "split-state", "--parent", "1"], true),
        (&["check-claim", "1"], true),
    ] {
        let mut full: Vec<String> = vec!["forge".into()];
        full.extend(argv.iter().map(|a| (*a).to_string()));
        assert_eq!(parse(&full).is_read(), read, "{argv:?}");
    }
}

#[test]
fn the_refusal_status_is_not_a_data_answer() {
    // `check-claim` and `check-open-pr` use 0 and 1 as data, 5 for "unanswered";
    // a refusal must not alias any of them.
    assert!(![0, 1, 2, 3, 5, 75].contains(&EX_WRITE_VERB_NOT_MIGRATED));
}

#[test]
fn only_verbs_without_a_forge_call_skip_the_mode_gate() {
    let mut local: Vec<String> = declared_verbs()
        .into_iter()
        .map(|(path, cmd)| {
            let names: Vec<&str> = path.iter().map(String::as_str).collect();
            (names.join(" "), parse(&argv_for(&names, &cmd)))
        })
        .filter(|(_, cli)| cli.is_read() && !cli.reaches_forge())
        .map(|(name, _)| name)
        .collect();
    local.sort();
    assert_eq!(
        local,
        [
            "calls",
            "dashboard-link",
            "rebase-guard",
            "verdict-body-check"
        ]
    );
}
