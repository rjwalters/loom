//! **Which agent is calling** (#10116): bind an attended start to the one
//! pending `Bash` call that is running it.
//!
//! A Claude Code session can have several agents running at once: the main
//! agent and its subagents, each with its own transcript. All of them have the
//! same `$CLAUDE_CODE_SESSION_ID`. To publish the right transcript, the
//! starter needs proof of which agent's tool call it is running in. Text
//! matching is not proof. A sibling agent's running command can contain the
//! issue number by chance (`sleep 60`, `--limit 100`). A Doctor's claim step,
//! `worktree.sh "$ISSUE_NUM"`, never contains the number at all.
//!
//! The proof is the process tree. Claude Code runs each `Bash` call as
//! `<shell> -c '… eval '"'"'<command>'"'"' …'`, and `lease ensure` or
//! `live-output-attend` runs as a descendant of that shell. So the caller's
//! own call is the pending call whose command is **exactly** the script one of
//! this process's ancestor shells runs. That is either the shell's whole `-c`
//! argument, or the word its `eval` unquotes. A command that merely appears
//! inside a longer script does not count.
//!
//! **It fails closed.** An argv that cannot be read stops the walk: an
//! unsupported platform, or a process that has exited (a zombie's
//! `/proc/<pid>/cmdline` is empty, and `KERN_PROCARGS2` refuses one). So does
//! an empty argv. Nothing is ever inferred from what is missing. A missing
//! script can only mean "no match", and no match publishes nothing. A
//! process caught between `fork` and `exec` shows its parent's argv, which is
//! a copy of this same ancestor chain. So it too can only match the caller's
//! own command, or nothing. The command text read here is used only for this
//! comparison. It is never placed on a record.

/// How far up the process tree to look. Claude Code's shell is normally two
/// or three levels above `lease ensure` (`zsh -c` → `bash worktree.sh` →
/// `loom-daemon`). The bound only stops a runaway walk.
const MAX_DEPTH: usize = 32;

/// The scripts this process's ancestor shells are running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caller {
    scripts: Vec<String>,
}

impl Caller {
    /// Build from the argv of each ancestor process, nearest first.
    #[must_use]
    pub fn from_argvs(argvs: &[Vec<String>]) -> Self {
        let mut scripts = Vec::new();
        for argv in argvs {
            for script in shell_scripts(argv) {
                scripts.extend(eval_words(script));
                scripts.push(script.trim().to_string());
            }
        }
        scripts.retain(|script| !script.is_empty());
        scripts.dedup();
        Caller { scripts }
    }

    /// Read the real ancestors of this process.
    ///
    /// # Errors
    ///
    /// Why the ancestors could not be read. The caller then publishes
    /// nothing.
    pub fn from_process() -> Result<Self, String> {
        #[cfg(unix)]
        let argvs = ancestor_argvs(std::os::unix::process::parent_id(), parent_of, argv_of);
        #[cfg(not(unix))]
        let argvs: Vec<Vec<String>> = Vec::new();
        Self::from_ancestors(&argvs)
    }

    /// [`Self::from_argvs`], refusing when no ancestor could be read at all.
    fn from_ancestors(argvs: &[Vec<String>]) -> Result<Self, String> {
        if argvs.is_empty() {
            return Err(
                "this process's parent shells could not be read on this host, so the calling \
                 agent cannot be confirmed"
                    .to_string(),
            );
        }
        Ok(Self::from_argvs(argvs))
    }

    /// Whether `command`, a pending `Bash` call's command text, is exactly
    /// what one of this process's ancestor shells is running.
    #[must_use]
    pub fn runs(&self, command: &str) -> bool {
        let command = command.trim();
        !command.is_empty() && self.scripts.iter().any(|script| script == command)
    }
}

/// Each script a shell in `argv` was handed with `-c` (or a cluster such as
/// `-lc`): the argument right after the flag.
fn shell_scripts(argv: &[String]) -> impl Iterator<Item = &str> {
    argv.windows(2).filter_map(|pair| {
        let flag = pair[0].as_str();
        let is_c = flag.len() > 1
            && flag.starts_with('-')
            && !flag.starts_with("--")
            && flag[1..].chars().all(|c| c.is_ascii_alphabetic())
            && flag.contains('c');
        is_c.then_some(pair[1].as_str())
    })
}

/// Every command `script` hands to `eval`, with its shell quoting undone.
fn eval_words(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = script[from..].find("eval ") {
        let at = from + found;
        let starts_word = script[..at]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || ";&|({".contains(c));
        if starts_word {
            if let Some(word) = shell_word(script[at + "eval ".len()..].trim_start()) {
                out.push(word.trim().to_string());
            }
        }
        from = at + "eval ".len();
    }
    out
}

/// Decode the shell word at the head of `text`: single-quoted, double-quoted
/// and bare segments joined, up to the first unquoted blank or operator.
/// `None` for an unterminated quote or an empty word.
fn shell_word(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    let mut any = false;
    while let Some(&c) = chars.peek() {
        match c {
            '\'' => {
                chars.next();
                loop {
                    match chars.next()? {
                        '\'' => break,
                        ch => out.push(ch),
                    }
                }
            }
            '"' => {
                chars.next();
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            next @ ('"' | '\\' | '$' | '`') => out.push(next),
                            '\n' => {}
                            next => {
                                out.push('\\');
                                out.push(next);
                            }
                        },
                        ch => out.push(ch),
                    }
                }
            }
            '\\' => {
                chars.next();
                match chars.next()? {
                    '\n' => {}
                    next => out.push(next),
                }
            }
            c if c.is_whitespace() || ";&|<>()".contains(c) => break,
            c => {
                chars.next();
                out.push(c);
            }
        }
        any = true;
    }
    any.then_some(out)
}

/// The argv of each process from `start` upward, nearest first. The walk
/// stops at the first argv that cannot be read or is empty, and never skips
/// past one: an unknown link in the chain ends what counts as an ancestor.
/// The readers are injected so the fail-closed cases are testable.
fn ancestor_argvs(
    start: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
    argv_of: impl Fn(u32) -> Option<Vec<String>>,
) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut pid = start;
    for _ in 0..MAX_DEPTH {
        if pid <= 1 {
            break;
        }
        let Some(argv) = argv_of(pid).filter(|argv| !argv.is_empty()) else {
            break;
        };
        out.push(argv);
        match parent_of(pid) {
            Some(parent) if parent != pid => pid = parent,
            _ => break,
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid …`; `comm` may itself hold spaces or `)`.
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn argv_of(pid: u32) -> Option<Vec<String>> {
    parse_cmdline(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?)
}

/// `/proc/<pid>/cmdline`: NUL-separated arguments with a trailing NUL. It is
/// empty for a process that has exited (a zombie) or a kernel thread. That is
/// `None`, never an empty argv that could be mistaken for a readable one.
#[cfg(any(target_os = "linux", test))]
fn parse_cmdline(raw: &[u8]) -> Option<Vec<String>> {
    let raw = raw.strip_suffix(&[0]).unwrap_or(raw);
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.split(|byte| *byte == 0)
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect(),
    )
}

#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<u32> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: `proc_bsdinfo` is plain old data, so all-zero is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is a writable `proc_bsdinfo` of exactly `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    (written == size).then_some(info.pbi_ppid)
}

#[cfg(target_os = "macos")]
fn argv_of(pid: u32) -> Option<Vec<String>> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        libc::c_int::try_from(pid).ok()?,
    ];
    let mut size: libc::size_t = 0;
    // SAFETY: a size query: no output buffer, `size` receives the length.
    let rc = unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0)
    };
    if rc != 0 || size == 0 {
        return None;
    }
    let mut buffer = vec![0_u8; size];
    // SAFETY: `buffer` is `size` writable bytes, and `size` says so.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buffer.truncate(size);
    parse_procargs2(&buffer)
}

/// `KERN_PROCARGS2`'s layout: `argc` (a native `i32`), the executable path,
/// NUL padding, then `argc` NUL-terminated arguments (the environment
/// follows and is not read).
#[cfg(any(target_os = "macos", test))]
fn parse_procargs2(buffer: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok().filter(|argc| *argc > 0)?;
    let rest = &buffer[4..];
    let rest = &rest[rest.iter().position(|byte| *byte == 0)?..];
    let rest = &rest[rest.iter().position(|byte| *byte != 0)?..];
    let argv: Vec<String> = rest
        .split(|byte| *byte == 0)
        .take(argc)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    (argv.len() == argc).then_some(argv)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn parent_of(_pid: u32) -> Option<u32> {
    None
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn argv_of(_pid: u32) -> Option<Vec<String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper Claude Code 2.1.288 runs a `Bash` call in, as `ps` showed
    /// from inside a tool call on macOS: the command is single-quoted, with
    /// each `'` in it spelled `'"'"'`.
    fn claude_wrapper(command: &str) -> Vec<String> {
        let quoted = command.replace('\'', r#"'"'"'"#);
        vec![
            "/bin/zsh".to_string(),
            "-c".to_string(),
            format!(
                "source /Users/op/.claude/shell-snapshots/snapshot-zsh-1.sh 2>/dev/null || true \
                 && setopt NO_EXTENDED_GLOB 2>/dev/null || true && eval '{quoted}' < /dev/null \
                 && pwd -P >| /tmp/claude-e0c2-cwd"
            ),
        ]
    }

    #[test]
    fn the_command_a_claude_shell_evaluates_is_recovered_exactly() {
        let command = "PR_BRANCH=$(gh pr view 10121 --json headRefName --jq '.headRefName')\n\
                       if [[ \"$PR_BRANCH\" =~ ^feature/issue-([0-9]+)$ ]]; then\n  \
                       ISSUE_NUM=\"${BASH_REMATCH[1]}\"\n  \
                       ./.loom/scripts/worktree.sh \"$ISSUE_NUM\"\nfi";
        let caller = Caller::from_argvs(&[
            vec![
                "/bin/bash".to_string(),
                "./.loom/scripts/worktree.sh".to_string(),
                "10116".to_string(),
            ],
            claude_wrapper(command),
        ]);
        assert!(caller.runs(command));
        assert!(caller.runs(&format!("{command}\n")), "surrounding blanks do not matter");
    }

    #[test]
    fn a_command_merely_contained_in_the_script_does_not_count() {
        let caller = Caller::from_argvs(&[claude_wrapper("worktree.sh 60 && sleep 60")]);
        assert!(caller.runs("worktree.sh 60 && sleep 60"));
        assert!(!caller.runs("sleep 60"));
        assert!(!caller.runs("60"));
        assert!(!caller.runs(""));
    }

    #[test]
    fn a_plain_sh_dash_c_script_counts_as_a_whole() {
        let caller = Caller::from_argvs(&[vec![
            "sh".to_string(),
            "-lc".to_string(),
            "loom-daemon live-output-attend --issue 7".to_string(),
        ]]);
        assert!(caller.runs("loom-daemon live-output-attend --issue 7"));
        // A non-shell argv contributes nothing.
        let caller = Caller::from_argvs(&[vec!["claude".to_string(), "sleep 60".to_string()]]);
        assert!(!caller.runs("sleep 60"));
    }

    #[test]
    fn shell_words_unquote_like_a_shell() {
        assert_eq!(shell_word(r#"'a'"'"'b' rest"#).as_deref(), Some("a'b"));
        assert_eq!(shell_word(r#""x \"y\" \$z \q" rest"#).as_deref(), Some(r#"x "y" $z \q"#));
        assert_eq!(shell_word(r"a\ b;c").as_deref(), Some("a b"));
        assert_eq!(shell_word("'unterminated"), None);
        assert_eq!(shell_word(""), None);
        assert_eq!(eval_words("x=1; eval 'one' && reeval 'no' && eval two"), ["one", "two"]);
    }

    #[test]
    fn procargs2_is_parsed_up_to_argc() {
        let mut buffer = 2_i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/bin/zsh\0\0\0\0zsh\0-c\0ENV=leak\0");
        assert_eq!(parse_procargs2(&buffer), Some(vec!["zsh".to_string(), "-c".to_string()]));
        assert_eq!(parse_procargs2(b"\0\0"), None);
    }

    /// The platform readers against a real process tree: a shell that
    /// `eval`s a command is this test's child, and its argv names it.
    ///
    /// No timing is involved. The eval'd command prints a line and then
    /// blocks in the `read` builtin: no fork and no exec, so the shell stays
    /// alive with the argv it was spawned with until its stdin closes. The
    /// line can only appear once the shell has parsed and run the command, so
    /// reading it is the readiness signal. (The previous version eval'd
    /// `sleep 30; echo it's`, which is not valid shell. dash rejects the whole
    /// line before running `sleep`, so the child exited at once, and the test
    /// passed only when it read the argv in the first few milliseconds.)
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_real_child_shell_is_read_and_bound() {
        use std::io::BufRead as _;
        let command = r#"echo "it's ready"; read -r line"#;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"eval 'echo "it'"'"'s ready"; read -r line'"#)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        // Read while the shell is provably alive and blocked in `read`.
        let argv = argv_of(pid);
        let parent = parent_of(pid);
        drop(child.stdin.take());
        let _ = child.wait();
        assert_eq!(ready, "it's ready\n", "the shell did not run the eval'd command");
        let caller = Caller::from_argvs(&[argv.expect("a live shell's argv is readable")]);
        assert!(caller.runs(command), "{caller:?}");
        assert_eq!(parent, Some(std::process::id()));
    }

    /// The case CI hit: a child that has exited but is not yet reaped (a
    /// zombie). Its argv reads as `None` on both platforms, so it can never
    /// bind. `waitid(WNOWAIT)` waits for the exit without reaping, so the
    /// zombie state is reached exactly, not by sleeping.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn an_exited_shells_argv_is_unreadable_and_binds_nothing() {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("eval 'exit 0'")
            .spawn()
            .unwrap();
        let pid = child.id();
        // SAFETY: `info` is a writable `siginfo_t`; `WNOWAIT` leaves the child
        // unreaped, so `pid` stays this exited child until `wait` below.
        let rc = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT)
        };
        assert_eq!(rc, 0);
        let argv = argv_of(pid);
        let _ = child.wait();
        assert_eq!(argv, None, "an exited process has no readable argv");
        // So the walk stops there, and the start publishes nothing.
        let argvs = ancestor_argvs(pid, |_| Some(1), |_| argv.clone());
        assert!(Caller::from_ancestors(&argvs).is_err());
    }

    #[test]
    fn an_empty_cmdline_is_unreadable_not_an_empty_argv() {
        assert_eq!(parse_cmdline(b""), None);
        assert_eq!(parse_cmdline(b"\0"), None);
        assert_eq!(
            parse_cmdline(b"/bin/sh\0-c\0eval 'x'\0"),
            Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "eval 'x'".to_string()
            ])
        );
    }

    /// The walk never skips an unreadable link to reach a matching shell
    /// beyond it, and an empty argv or script never matches anything.
    #[test]
    fn an_unreadable_or_empty_ancestor_fails_closed() {
        let shell = |command: &str| {
            vec![
                "/bin/zsh".to_string(),
                "-c".to_string(),
                format!("eval '{command}'"),
            ]
        };
        let tree = |unreadable: u32| {
            move |pid: u32| match pid {
                _ if pid == unreadable => None,
                10 => Some(vec!["loom-daemon".to_string(), "lease".to_string()]),
                11 => Some(Vec::new()),
                12 => Some(shell("worktree.sh 42")),
                _ => None,
            }
        };
        let parent = |pid: u32| Some(pid + 1);
        // The first ancestor is gone: nothing is read, and the start refuses.
        assert!(Caller::from_ancestors(&ancestor_argvs(10, parent, tree(10))).is_err());
        // An empty argv (11) ends the walk before the matching shell (12).
        let argvs = ancestor_argvs(10, parent, tree(0));
        assert_eq!(argvs.len(), 1);
        assert!(!Caller::from_ancestors(&argvs)
            .unwrap()
            .runs("worktree.sh 42"));
        // With every link readable, the same shell is found.
        let argvs = ancestor_argvs(12, parent, tree(0));
        assert!(Caller::from_ancestors(&argvs)
            .unwrap()
            .runs("worktree.sh 42"));
        // Empty argvs, a `-c` with no script, and an empty script bind nothing.
        let caller = Caller::from_argvs(&[
            Vec::new(),
            vec!["sh".to_string(), "-c".to_string()],
            vec!["sh".to_string(), "-c".to_string(), String::new()],
        ]);
        assert_eq!(caller, Caller::default());
        assert!(!caller.runs("") && !caller.runs("sh") && !caller.runs("-c"));
        // An empty eval'd word is dropped; only the whole script remains.
        let caller = Caller::from_argvs(&[vec![
            "sh".to_string(),
            "-c".to_string(),
            "eval ''".to_string(),
        ]]);
        assert!(!caller.runs("") && caller.runs("eval ''"));
    }
}
