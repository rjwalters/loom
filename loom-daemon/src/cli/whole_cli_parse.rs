//! Test-only: parse the whole top-level `crate::Cli` the way the binary does
//! (#10616).
//!
//! In a **debug** build, clap's derive gives every
//! `#[derive(Subcommand)]::augment_subcommands` one stack frame sized by all
//! the args and subcommands it declares, and nested subcommand enums nest
//! those frames. `Commands` alone is about 1.2 MB and `ForgeAction` about
//! 0.7 MB, so building the whole tree takes between 1984 and 2048 KiB.
//! libtest's worker threads get 2 MiB, which leaves almost nothing, and one
//! new verb was enough to abort unrelated suites with `stack overflow`.
//!
//! The binary does not have that limit. It parses `Cli` on the process main
//! thread (`daemon_service::run_daemon`), which has 8 MiB. So every test that
//! parses the whole `Cli` calls [`try_parse_cli`], which does the parse on an
//! 8 MiB thread. The guards below keep that true and pin the headroom.

/// The binary's main-thread stack: the 8 MiB default `ulimit -s` on Linux and
/// the main-thread size on macOS.
pub(crate) const MAIN_THREAD_STACK: usize = 8 << 20;

/// Run `f` on a named thread with a `stack` byte stack and return its result.
///
/// A stack overflow aborts the whole test process, but the abort message
/// names the thread: `thread '<name>' has overflowed its stack`.
pub(crate) fn on_stack<T: Send + 'static>(
    name: &str,
    stack: usize,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(stack)
        .spawn(f)
        .expect("spawn sized parse thread")
        .join()
        .expect("sized parse thread panicked")
}

/// `crate::Cli::try_parse_from(args)` on an 8 MiB thread, so the test sees
/// the same `Command` tree the binary builds without depending on the test
/// harness's stack size (`RUST_MIN_STACK`).
pub(crate) fn try_parse_cli(args: &[&str]) -> Result<crate::Cli, clap::Error> {
    let argv: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    on_stack("whole-cli-parse-8MiB", MAIN_THREAD_STACK, move || {
        <crate::Cli as clap::Parser>::try_parse_from(argv)
    })
}

#[cfg(test)]
mod tests {
    use super::{on_stack, try_parse_cli, MAIN_THREAD_STACK};

    /// The deepest parse path: `Commands` -> `ForgeAction` ->
    /// `MergeQueueAction`. Every nested `augment_subcommands` frame is live.
    const DEEP_ARGV: &[&str] = &["loom-daemon", "forge", "merge-queue", "mode"];

    fn parses_forge(args: &[&str]) -> bool {
        matches!(
            try_parse_cli(args).map(|cli| cli.command),
            Ok(Some(crate::Commands::Forge { .. }))
        )
    }

    /// The helper must not depend on the caller's stack. A 1 MiB caller is the
    /// in-test version of running the suite with `RUST_MIN_STACK=1048576`, far
    /// below the ~2 MiB the debug-build parse needs on its own.
    #[test]
    fn whole_cli_parse_does_not_depend_on_the_callers_stack() {
        assert!(on_stack("cli-parse-caller-1MiB", 1 << 20, || parses_forge(DEEP_ARGV)));
    }

    /// Pins the headroom (#10616). The whole-`Cli` parse must fit in half of
    /// the binary's main-thread stack. If debug-build clap frames grow past
    /// that, this test aborts with `thread 'cli-parse-headroom-guard-issue-10616'
    /// has overflowed its stack`, before production's 8 MiB is in reach. The
    /// fix is to defer the largest subcommand trees (`clap::Command::defer`),
    /// not to raise this number.
    #[test]
    fn whole_cli_parse_fits_in_half_the_main_thread_stack() {
        let parsed =
            on_stack("cli-parse-headroom-guard-issue-10616", MAIN_THREAD_STACK / 2, || {
                <crate::Cli as clap::Parser>::try_parse_from(DEEP_ARGV)
                    .map(|cli| matches!(cli.command, Some(crate::Commands::Forge { .. })))
            });
        assert!(matches!(parsed, Ok(true)), "deep parse failed: {:?}", parsed.err());
    }
}
