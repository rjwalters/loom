//! Decisions behind the Loom `PreToolUse` guard hooks that moved out of shell
//! (issue #10335): the `guards.enabled` master opt-out and the gh-body heredoc
//! mask. The hooks keep only a call-site to `loom-daemon guard-hook …`, so the
//! `shell-budget` gate's hook-entry pool does not grow (epic #7810).
//!
//! Both helpers are pure functions of their input (plus the effective config
//! for [`opted_out`]); the CLI wrapper is `cli/guard_hook_cmd.rs`.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

/// True only on an EXPLICIT guards opt-out for `root`: `LOOM_GUARDS_ENABLED`
/// `0|false|no`, or a boolean `guards.enabled: false` in the effective config.
///
/// An empty `root` (a hook that could not resolve its repo) consults only the
/// env var — resolving the config chain against `""` would read whatever
/// `.loom/config.json` happens to sit in the current directory.
#[must_use]
pub fn opted_out(root: &Path) -> bool {
    if root.as_os_str().is_empty() {
        return matches!(std::env::var("LOOM_GUARDS_ENABLED").as_deref(), Ok("0" | "false" | "no"));
    }
    crate::config_resolver::guards_master_disabled(root)
}

struct Patterns {
    direct: Regex,
    pipe: Regex,
    taint: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        // One argument: a bare word of metacharacter-free characters, a
        // double-quoted string with no `"`, `$`, backtick or backslash inside,
        // or a single-quoted string.
        let arg = r#"([ \t]+([A-Za-z0-9_./:@,+=-]+|"[^"$`\\]*"|'[^']*'))*"#;
        let ghcmd = format!(r"gh[ \t]+(issue|pr)[ \t]+(create|comment|edit){arg}");
        let dl = r#"<<-?[ \t]*('[A-Za-z_][A-Za-z0-9_]*'|"[A-Za-z_][A-Za-z0-9_]*")"#;
        let compile = |re: String| Regex::new(&re).expect("static guard-hook regex");
        Patterns {
            direct: compile(format!(r"^[ \t]*{ghcmd}[ \t]*{dl}[ \t]*$")),
            pipe: compile(format!(r"^[ \t]*cat[ \t]*{dl}[ \t]*[|][ \t]*{ghcmd}[ \t]*$")),
            taint: compile(r#"["'`\\]"#.to_string()),
        }
    })
}

/// The heredoc delimiter word an opener line declares: the text after its
/// LAST `<<` (and an optional `-`), up to a `|` or trailing blanks, with
/// quotes removed.
fn delimiter(line: &str) -> String {
    let after = line.rfind("<<").map_or(line, |at| &line[at + 2..]);
    let after = after.strip_prefix('-').unwrap_or(after);
    let after = after.trim_start_matches([' ', '\t']);
    let before_pipe = after.find('|').map_or(after, |at| &after[..at]);
    before_pipe
        .trim_end_matches([' ', '\t'])
        .replace(['\'', '"'], "")
}

/// Blank the BODY of a quoted-delimiter heredoc whose consumer is provably
/// `gh issue|pr create|comment|edit` — directly (`gh issue create --body-file -
/// <<'EOF'`) or via a single `cat <<'EOF' | gh issue create …` pipe.
///
/// `gh` reads its body from stdin and never executes or forwards it, and the
/// quoted delimiter means bash performs no expansion in the body, so the text
/// is inert data. Fail-safe: the opener must be a whole physical line made
/// only of the `gh` command and metacharacter-free / balanced-quote args;
/// every earlier line must be free of quotes, backslashes and heredoc openers
/// (so the line start is a real command boundary); and the closing delimiter
/// must be found. Anything else masks nothing, so `bash <<'EOF'`,
/// `gh …; bash <<'EOF'` and a real merge command still deny as before.
///
/// Line structure is preserved: a masked body line becomes empty, it is not
/// removed.
#[must_use]
pub fn mask_gh_body_heredocs(command: &str) -> String {
    let p = patterns();
    let mut lines: Vec<String> = command.split('\n').map(str::to_string).collect();
    // A trailing newline terminates the last line; it does not start a new one.
    if command.ends_with('\n') {
        lines.pop();
    }
    let n = lines.len();
    let mut tainted = false;
    let mut i = 0;
    while i < n {
        let l = lines[i].clone();
        if !tainted && (p.direct.is_match(&l) || p.pipe.is_match(&l)) {
            let d = delimiter(&l);
            let dash = l.contains("<<-");
            let close_at = (i + 1..n).find(|&j| {
                let c = if dash {
                    lines[j].trim_start_matches('\t')
                } else {
                    lines[j].as_str()
                };
                c == d
            });
            if let Some(close_at) = close_at {
                for line in &mut lines[i + 1..close_at] {
                    line.clear();
                }
                i = close_at + 1;
                continue;
            }
        }
        if p.taint.is_match(&l) || l.contains("<<") {
            tainted = true;
        }
        i += 1;
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests;
