//! `security -i` keychain write for the per-config-dir credential clone (#11850).
//!
//! Split out of `terminal.rs` (a file-size-ratchet file) so the credential
//! never rides in argv and the parent file does not grow.

/// Upper bound on one `security -i` command line.
///
/// `security -i` reads commands into a fixed buffer of about 4 KiB.  A
/// longer line is split: the first part runs as a truncated command and
/// the remainder is parsed as a *second* command, whose text `security`
/// echoes to stderr as `unknown command "<text>"`.  For a line carrying
/// the credential that would put credential bytes in stderr, so a line
/// at or above this bound is never sent.  4000 leaves headroom under the
/// observed limit; it admits credentials of up to ~1950 bytes (hex
/// doubles them), several times the size of a Claude Code OAuth blob.
const SECURITY_INTERACTIVE_MAX_LINE: usize = 4000;

/// A keychain write to perform by spawning `program` with `args` and
/// feeding `stdin` to it.  The secret lives only in `stdin`.
#[derive(Debug)]
pub(super) struct KeychainWrite {
    pub(super) program: &'static str,
    pub(super) args: Vec<String>,
    pub(super) stdin: String,
}

/// Quote a word for the `security -i` command tokenizer: wrap it in
/// double quotes and backslash-escape `"` and `\`.
fn security_quote(word: &str) -> String {
    let mut out = String::with_capacity(word.len() + 2);
    out.push('"');
    for c in word.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Build the command that writes `cred` to the keychain item
/// (`account`, `service`) without putting the credential in any argv.
///
/// The process is `security -i` (interactive mode), which reads its
/// commands from stdin; the `add-generic-password ... -X <hex>` command,
/// and so the credential, travels only on that pipe (#11850).  The item
/// is still created by `/usr/bin/security`, so its default ACL trusts
/// the same binary Claude Code reads it back with, exactly as before.
///
/// Rejected alternatives:
/// - `add-generic-password ... -w` with no value prompts on the terminal
///   (or stdin when there is none), asks twice, and silently truncates
///   the password to 128 bytes, which corrupts a Claude Code credential.
/// - Writing in-process through the Security framework makes the daemon
///   binary the item's trusted application, so Claude Code's own
///   `security find-generic-password` read would raise a keychain access
///   dialog on an unattended host.
///
/// Returns `None` when the command line would exceed
/// [`SECURITY_INTERACTIVE_MAX_LINE`].
pub(super) fn keychain_write_command(
    account: &str,
    service: &str,
    cred: &str,
) -> Option<KeychainWrite> {
    let line = format!(
        "add-generic-password -U -a {} -s {} -X {}\n",
        security_quote(account),
        security_quote(service),
        hex::encode(cred.as_bytes()),
    );
    if line.len() >= SECURITY_INTERACTIVE_MAX_LINE {
        return None;
    }
    Some(KeychainWrite {
        program: "security",
        args: vec!["-i".to_string()],
        stdin: line,
    })
}

/// Run a [`KeychainWrite`], returning whether it succeeded.
///
/// `security -i` exits 0 even when the command it ran failed; it reports
/// the failure on stderr as `<command>: returned <code>`.  So success is
/// a zero exit *and* an empty stderr.  stderr is never logged: on a
/// malformed line it can echo the command text.
pub(super) fn run_keychain_write(write: &KeychainWrite) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let child = Command::new(write.program)
        .args(&write.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            log::debug!("Failed to spawn `security -i`: {e}");
            return false;
        }
    };
    let fed = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(write.stdin.as_bytes()).is_ok());
    // stdin is dropped (closed) above, so `security -i` sees EOF and exits.
    match child.wait_with_output() {
        Ok(output) => fed && output.status.success() && output.stderr.is_empty(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A credential-shaped fake: JSON with quotes and spaces, as the
    /// real Claude Code keychain blob is.
    const FAKE_CRED: &str = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-FAKEFAKEFAKE","refreshToken":"sk-ant-ort01-NOTREAL","scopes":["user:inference"]}}"#;

    #[test]
    fn test_keychain_write_argv_never_contains_credential() {
        // #11850: the credential, in any encoding, must not be in argv.
        let write =
            keychain_write_command("alice", "Claude Code-credentials-abcd1234", FAKE_CRED).unwrap();
        assert_eq!(write.program, "security");
        assert_eq!(write.args, vec!["-i".to_string()]);

        let cred_hex = hex::encode(FAKE_CRED.as_bytes());
        let cred_hex_upper = cred_hex.to_uppercase();
        for arg in std::iter::once(write.program.to_string()).chain(write.args.clone()) {
            // No 8-byte window of the credential or of its hex encoding.
            for needle in [FAKE_CRED, cred_hex.as_str(), cred_hex_upper.as_str()] {
                for window in needle.as_bytes().windows(8) {
                    let window = std::str::from_utf8(window).unwrap();
                    assert!(!arg.contains(window), "argv element {arg:?} leaks credential");
                }
            }
        }
    }

    #[test]
    fn test_keychain_write_stdin_carries_the_credential() {
        let write =
            keychain_write_command("alice", "Claude Code-credentials-abcd1234", FAKE_CRED).unwrap();
        let cred_hex = hex::encode(FAKE_CRED.as_bytes());
        assert_eq!(
                write.stdin,
                format!(
                    "add-generic-password -U -a \"alice\" -s \"Claude Code-credentials-abcd1234\" -X {cred_hex}\n"
                )
            );
        let hex_field = write.stdin.trim_end().rsplit(' ').next().unwrap();
        assert_eq!(hex::decode(hex_field).unwrap(), FAKE_CRED.as_bytes());
    }

    #[test]
    fn test_keychain_write_quotes_account_and_service() {
        let write = keychain_write_command(r#"we"ird\acct x"#, "svc name", "pw").unwrap();
        assert!(write
            .stdin
            .starts_with(r#"add-generic-password -U -a "we\"ird\\acct x" -s "svc name" -X "#));
    }

    #[test]
    fn test_keychain_write_rejects_line_over_security_buffer() {
        // Over the `security -i` line buffer the tail of the line would be
        // re-parsed as a command and echoed to stderr, so refuse it.
        let big = "q".repeat(SECURITY_INTERACTIVE_MAX_LINE / 2);
        assert!(keychain_write_command("alice", "svc", &big).is_none());
        let ok = "q".repeat(1900);
        assert!(keychain_write_command("alice", "Claude Code-credentials-abcd1234", &ok).is_some());
    }
}
