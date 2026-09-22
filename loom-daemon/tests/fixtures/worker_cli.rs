//! Native fake harness: tests argv, streams, exit status and exec identity.
//! Std-only: the test compiles this file with bare `rustc`.

/// Classify fd 0 by `fstat` rather than by path, so a pipe, a TTY, a regular
/// file and `/dev/null` are all told apart (issue #8506: the native harnesses
/// now receive the prompt as a regular, already-fully-written file handed to
/// stdin, never `/dev/null` and never the parent's own pipe).
#[cfg(unix)]
fn stdin_kind() -> &'static str {
    use std::os::fd::AsFd;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(meta) = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .and_then(|fd| std::fs::File::from(fd).metadata())
    else {
        return "unknown";
    };
    let file_type = meta.file_type();
    if file_type.is_char_device() {
        if std::fs::metadata("/dev/null").is_ok_and(|null| meta.rdev() == null.rdev()) {
            "null"
        } else {
            "char_device"
        }
    } else if file_type.is_fifo() {
        "pipe"
    } else if file_type.is_file() {
        "regular"
    } else {
        "other"
    }
}
#[cfg(not(unix))]
fn stdin_kind() -> &'static str {
    "unknown"
}

fn main() {
    // A versioned CLI: answer the adapter's probe and nothing else. This must
    // stay ahead of the FIXTURE_SIGNAL self-kill, which would otherwise take
    // the probe down instead of the launched harness.
    if std::env::args_os()
        .skip(1)
        .eq([std::ffi::OsString::from("--version")])
    {
        println!("{}", std::env::var("FIXTURE_VERSION").unwrap_or_default());
        std::process::exit(
            std::env::var("FIXTURE_VERSION_EXIT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        );
    }
    if std::env::var("FIXTURE_SIGNAL").is_ok() {
        std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .unwrap();
    }
    println!("pid={}", std::process::id());
    println!("cwd={:?}", std::env::current_dir().unwrap_or_default());
    let kind = stdin_kind();
    println!("stdin_kind={kind}");
    // Legacy compatibility with pre-#8506 assertions.
    println!("stdin_is_dev_null={}", kind == "null");
    for arg in std::env::args_os().skip(1) {
        println!("arg={arg:?}");
    }
    for key in [
        "LOOM_RUNTIME",
        "LOOM_MODEL",
        "LOOM_TEST_ALLOW_SYSTEMD",
        "LOOM_DAEMON_LOG",
        "CARGO_INCREMENTAL",
    ] {
        println!("{key}={}", std::env::var(key).unwrap_or_default());
    }
    for key in std::env::var("FIXTURE_PRINT_ENV")
        .unwrap_or_default()
        .split(',')
    {
        if !key.is_empty() {
            println!("child_env {key}={}", std::env::var(key).unwrap_or_default());
        }
    }
    if let Ok(source) = std::env::var("LOOM_TEST_PROVIDER_SECRET") {
        println!(
            "credential_alias_matches={}",
            std::env::var("LOOM_TEST_HARNESS_SECRET").ok().as_deref() == Some(source.as_str())
        );
    }
    // Pool-sourced credential: the source variable is deliberately absent from
    // the environment, so the expected value is passed under its own name.
    if let Ok(expected) = std::env::var("LOOM_TEST_EXPECTED_SECRET") {
        println!(
            "credential_pool_matches={}",
            std::env::var("LOOM_TEST_HARNESS_SECRET").ok().as_deref() == Some(expected.as_str())
        );
    }
    if std::env::var("FIXTURE_NATIVE_CONFIG").is_ok() {
        println!("native_config={}", std::env::var("OPENCODE_CONFIG_CONTENT").unwrap());
        println!(
            "native_worker_pid_matches={}",
            std::env::var("LOOM_NATIVE_WORKER_PID").unwrap() == std::process::id().to_string()
        );
    }
    // Simulates a native `--format json` event stream (#8448), so a test can
    // capture the guarded launch's full output through the real `spawn-worker`
    // seam and feed it to `worker_spawn::launch_outcome::classify_native_stream`
    // — "toolless" reproduces #8448's live OpenCode 2.0.10 receipt exactly
    // (one step_start, one text event, no tool_use, no step_finish); "used"
    // is an ordinary completion that actually calls a loom_* tool.
    match std::env::var("FIXTURE_NATIVE_STREAM").as_deref() {
        Ok("toolless") => {
            println!("{{\"type\":\"step_start\"}}");
            println!(
                "{{\"type\":\"text\",\"text\":\"I do not have tools named loom_write, loom_bash, or loom_edit\"}}"
            );
        }
        Ok("used") => {
            println!("{{\"type\":\"step_start\"}}");
            println!("{{\"type\":\"tool_use\",\"tool\":\"loom_write\",\"input\":{{\"path\":\"a.txt\"}}}}");
            println!(
                "{{\"type\":\"tool_result\",\"tool\":\"loom_write\",\"output\":\"Wrote 3 bytes\"}}"
            );
            println!("{{\"type\":\"step_finish\"}}");
        }
        _ => {}
    }
    // #8506: the harness now delivers the prompt on stdin instead of argv.
    // Reading is safe here ONLY because it is gated on the fd kind: a
    // "regular" file (our unlinked prompt-transfer tempfile) or "null" both
    // hit EOF immediately with no writer to wait on. A "pipe" is
    // deliberately left unread — one test holds a live, unwritten,
    // never-closed pipe open across the whole run specifically to prove a
    // launch never blocks on it, and reading here would hang that case
    // forever.
    if kind != "pipe" {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf).unwrap();
        println!("stdin_len={}", buf.len());
        // Printed last, on its own marker line, with the raw bytes following
        // verbatim (not `Debug`-escaped) so a test can assert byte-for-byte
        // delivery of an arbitrarily large prompt via a single split.
        print!("STDIN_BEGIN\n");
        std::io::Write::write_all(&mut std::io::stdout(), &buf).unwrap();
    }
    eprintln!("fixture stderr");
    std::process::exit(
        std::env::var("FIXTURE_EXIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    );
}
