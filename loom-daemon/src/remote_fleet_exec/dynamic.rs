//! Run-time-built payloads and fan-out wrappers (#11568, Judge cycle 1).
//!
//! A remote command whose text is only known at run time — `ssh h "$(cat
//! fix.sh)"`, `ssh h "$CMD"`, `bash -s <<< "$(cat fix.sh)"`, an unquoted
//! heredoc whose body is `$(cat fix.sh)`, `printf '%s\n' "$CMD" | ssh h bash`
//! — cannot be checked, so it is undeterminable and asks (fail closed), the
//! same as an unreadable `< fix.sh`.
//!
//! The rule is deliberately narrower than "any `$` asks", so a read-only
//! `ssh h "ls $DIR"` still passes:
//!
//! - a **local command substitution** (`$( … )` / backticks expanded on this
//!   side) anywhere in the payload is undeterminable — its output becomes
//!   remote shell code;
//! - once the payload is lexed as the executing shell will lex it, a
//!   **command word that is an expansion** (`$CMD`, `"$(cat f)"`), a
//!   `bash -c` / `sh -c` script that is an expansion, `eval` of an expansion,
//!   or `source` / `.` of an expansion is undeterminable.
//!
//! A plain variable in an argument of a known command (`ls $DIR`) is not.
//!
//! Fan-out: `xargs [opts] cmd …` and `parallel [opts] cmd … ::: args` run
//! `cmd` once per input line; [`fan_out`] returns that command with every
//! replacement placeholder (`{}`, `-I R`, appended input) turned into an
//! expansion, so a host taken from stdin is a dynamic (treated-as-fleet)
//! target and a command taken from stdin is undeterminable.

use super::lex::{self, Word};
use super::{base, command_index, shell_c_script, SHELLS};

const MAX_DEPTH: usize = 4;
/// The text a placeholder becomes: an expansion the lexer marks dynamic.
const ARG: &str = "$FANOUT_ARG";

fn has_substitution(text: &str) -> bool {
    text.contains("$(") || text.contains('`')
}

/// Why `script` — text a shell will execute, local or remote — is built at
/// run time and so cannot be checked; `None` when it can be. `local` are the
/// words the script was assembled from on this side (quotes already removed),
/// so a local `$( … )` is told apart from a single-quoted remote one.
#[must_use]
pub fn undeterminable(script: &str, local: &[&Word]) -> Option<String> {
    if let Some(w) = local
        .iter()
        .find(|w| w.dynamic && has_substitution(&w.text))
    {
        return Some(format!("`{}` is built at run time", w.text));
    }
    dynamic_command(script, 0)
}

fn dynamic_command(text: &str, depth: usize) -> Option<String> {
    if depth > MAX_DEPTH {
        return None;
    }
    for simple in lex::pipelines(text).iter().flatten() {
        let Some(ci) = command_index(&simple.words) else {
            continue;
        };
        let cmd = &simple.words[ci];
        let rest = &simple.words[ci + 1..];
        if cmd.dynamic {
            return Some(format!("`{}` is a command chosen at run time", cmd.text));
        }
        let name = base(&cmd.text);
        if name == "eval" || name == "source" || name == "." {
            if let Some(w) = rest.iter().find(|w| w.dynamic) {
                return Some(format!("`{name} {}` runs text chosen at run time", w.text));
            }
        }
        if SHELLS.contains(&name) {
            if let Some(script) = shell_c_script(rest) {
                if script.dynamic {
                    return Some(format!(
                        "`{name} -c {}` runs text chosen at run time",
                        script.text
                    ));
                }
                if let Some(why) = dynamic_command(&script.text, depth + 1) {
                    return Some(why);
                }
            }
        }
    }
    None
}

/// The command `xargs` / `parallel` runs per input.
pub struct FanOut {
    /// Its words, placeholders made dynamic.
    pub words: Vec<Word>,
    /// `xargs -a FILE` reads its inputs from FILE and hands its own stdin to
    /// each command — so a `< fix.sh` on the `xargs` feeds the remote shell.
    pub passes_stdin: bool,
}

/// [`FanOut`] for an `xargs` / `parallel` invocation; `None` for any other
/// command word (or no command).
#[must_use]
pub fn fan_out(name: &str, rest: &[Word]) -> Option<FanOut> {
    match name {
        "xargs" => xargs(rest),
        "parallel" => parallel(rest).map(|words| FanOut {
            words,
            passes_stdin: false,
        }),
        _ => None,
    }
}

fn xargs(rest: &[Word]) -> Option<FanOut> {
    const TAKES: &[&str] = &["-I", "-E", "-L", "-n", "-P", "-s", "-a", "-d"];
    const LONG_TAKES: &[&str] = &[
        "--arg-file",
        "--delimiter",
        "--max-args",
        "--max-procs",
        "--max-chars",
        "--max-lines",
        "--eof",
        "--process-slot-var",
    ];
    let mut replace: Option<String> = None;
    let mut passes_stdin = false;
    let mut i = 0;
    while let Some(w) = rest.get(i) {
        let t = w.text.as_str();
        if t == "--" {
            i += 1;
            break;
        }
        if t.len() < 2 || !t.starts_with('-') {
            break;
        }
        i += 1;
        if t.starts_with("-a") || t.starts_with("--arg-file") {
            passes_stdin = true;
        }
        if let Some(r) = t.strip_prefix("--replace") {
            replace = Some(r.strip_prefix('=').unwrap_or("{}").to_string());
        } else if t.starts_with("--") {
            if !t.contains('=') && LONG_TAKES.contains(&t) {
                i += 1;
            }
        } else if let Some(r) = t.strip_prefix("-I") {
            replace = Some(if r.is_empty() {
                i += 1;
                rest.get(i - 1)
                    .map_or_else(|| "{}".into(), |w| w.text.clone())
            } else {
                r.to_string()
            });
        } else if let Some(r) = t.strip_prefix("-i") {
            replace = Some(if r.is_empty() {
                "{}".into()
            } else {
                r.to_string()
            });
        } else if TAKES.contains(&t) {
            i += 1;
        }
    }
    let cmd = rest.get(i..).filter(|c| !c.is_empty())?;
    let words = with_placeholders(
        cmd,
        |text| match &replace {
            Some(r) if !r.is_empty() => text
                .contains(r.as_str())
                .then(|| text.replace(r.as_str(), ARG)),
            _ => None,
        },
        replace.is_none(),
    );
    Some(FanOut {
        words,
        passes_stdin,
    })
}

fn parallel(rest: &[Word]) -> Option<Vec<Word>> {
    const TAKES: &[&str] = &[
        "-j",
        "-P",
        "-S",
        "-I",
        "-E",
        "-n",
        "-N",
        "-L",
        "-a",
        "-d",
        "-C",
        "--jobs",
        "--sshlogin",
        "--joblog",
        "--results",
        "--timeout",
        "--delay",
        "--retries",
        "--tag-string",
        "--workdir",
        "--wd",
        "--arg-file",
        "--basefile",
        "--bf",
        "--tmpdir",
        "--colsep",
        "--delimiter",
        "--max-args",
        "--max-replace-args",
        "--sshloginfile",
        "--slf",
        "--env",
        "--profile",
        "--memfree",
        "--load",
        "--shebang-wrap",
    ];
    let mut i = 0;
    while let Some(w) = rest.get(i) {
        let t = w.text.as_str();
        if t == "--" {
            i += 1;
            break;
        }
        if t.len() < 2 || !t.starts_with('-') {
            break;
        }
        i += 1;
        if TAKES.contains(&t) {
            i += 1;
        }
    }
    let end = rest[i.min(rest.len())..]
        .iter()
        .position(|w| w.text.starts_with(":::"))
        .map_or(rest.len(), |p| i + p);
    let cmd = rest.get(i..end).filter(|c| !c.is_empty())?;
    let mut placeholder = |text: &str| {
        let mut out = String::new();
        let mut hit = false;
        let mut rest = text;
        while let Some(open) = rest.find('{') {
            match rest[open..].find('}') {
                Some(len) if !rest[open + 1..open + len].contains(char::is_whitespace) => {
                    out.push_str(&rest[..open]);
                    out.push_str(ARG);
                    rest = &rest[open + len + 1..];
                    hit = true;
                }
                _ => break,
            }
        }
        out.push_str(rest);
        hit.then_some(out)
    };
    // `parallel "ssh {} 'git reset --hard'" ::: h` — one word is a whole
    // shell command line (parallel hands its command to a shell).
    if cmd.len() == 1 && cmd[0].text.contains(char::is_whitespace) {
        let text = placeholder(&cmd[0].text).unwrap_or_else(|| format!("{} {ARG}", cmd[0].text));
        let simple = lex::pipelines(&text).into_iter().flatten().next()?;
        return Some(simple.words);
    }
    Some(with_placeholders(cmd, &mut placeholder, true))
}

/// `cmd` with `subst` applied to each word (a hit becomes dynamic); with no
/// hit at all and `append`, the input is appended as one dynamic word.
fn with_placeholders(
    cmd: &[Word],
    mut subst: impl FnMut(&str) -> Option<String>,
    append: bool,
) -> Vec<Word> {
    let mut hit = false;
    let mut out: Vec<Word> = cmd
        .iter()
        .map(|w| match subst(&w.text) {
            Some(text) => {
                hit = true;
                Word {
                    text,
                    dynamic: true,
                }
            }
            None => w.clone(),
        })
        .collect();
    if !hit && append {
        out.push(Word {
            text: ARG.into(),
            dynamic: true,
        });
    }
    out
}
