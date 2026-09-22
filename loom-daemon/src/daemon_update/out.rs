//! The script's four output helpers, reproduced exactly — plus the one
//! stream-routing rule `--resolve-json` imposes on all of them.
//!
//! ```sh
//! if [[ -t 1 ]]; then RED=…; GREEN=…; YELLOW=…; NC=…; else …=''; fi
//! err()  { echo -e "${RED}$*${NC}" >&2; }
//! warn() { echo -e "${YELLOW}$*${NC}" >&2; }
//! ok()   { echo -e "${GREEN}$*${NC}"; }
//! ```
//!
//! Three details are load-bearing and easy to lose:
//!
//! * **The colour test is on STDOUT (`-t 1`), but `err`/`warn` write to
//!   STDERR.** So a run with a terminal stdout and a redirected stderr still
//!   colours the stderr lines. Testing `isatty(2)` for the stderr writers
//!   would read as the obvious fix and would change what a captured log
//!   contains.
//! * **`err` and `warn` go to stderr; `ok` and a bare `echo` go to stdout.**
//!   The retained suite captures them separately (`2>&1` in some cases,
//!   `2>/dev/null` in others), so a line on the wrong stream is invisible to
//!   one case and noise in another.
//! * **`--resolve-json` moves the stdout writers to stderr** (#7609). The
//!   shell did it with one `exec 3>&1 1>&2` at the top of the run and then
//!   `exec … >&3` for the JSON itself; here [`divert_stdout_to_stderr`] sets
//!   a process-global flag that [`ok`] and [`say`] consult. The effect is the
//!   same — stdout carries the single JSON object and nothing else — without
//!   needing a spare file descriptor to survive into the exec'd child, which
//!   inherits the real stdout untouched.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// ANSI codes, or empty strings when stdout is not a terminal.
pub struct Palette {
    pub red: &'static str,
    pub green: &'static str,
    pub yellow: &'static str,
    pub nc: &'static str,
}

static PALETTE: OnceLock<Palette> = OnceLock::new();

/// `--resolve-json`: every informational line moves to stderr so stdout holds
/// exactly one JSON object.
static DIVERTED: AtomicBool = AtomicBool::new(false);

/// Resolve once per process, exactly as the script resolved once per run.
pub fn palette() -> &'static Palette {
    PALETTE.get_or_init(|| {
        if std::io::stdout().is_terminal() {
            Palette {
                red: "\x1b[0;31m",
                green: "\x1b[0;32m",
                yellow: "\x1b[1;33m",
                nc: "\x1b[0m",
            }
        } else {
            Palette {
                red: "",
                green: "",
                yellow: "",
                nc: "",
            }
        }
    })
}

/// `exec 3>&1 1>&2` — route the stdout writers to stderr for the rest of the
/// run. Idempotent; never reversed (the shell never restored fd 1 either).
pub fn divert_stdout_to_stderr() {
    DIVERTED.store(true, Ordering::SeqCst);
}

fn diverted() -> bool {
    DIVERTED.load(Ordering::SeqCst)
}

/// `err()` — red, stderr. Unaffected by the `--resolve-json` diversion
/// (already on stderr).
pub fn err(msg: &str) {
    let p = palette();
    eprintln!("{}{}{}", p.red, msg, p.nc);
}

/// `warn()` — yellow, stderr. Unaffected by the diversion.
pub fn warn(msg: &str) {
    let p = palette();
    eprintln!("{}{}{}", p.yellow, msg, p.nc);
}

/// `ok()` — green, stdout (stderr under `--resolve-json`).
pub fn ok(msg: &str) {
    let p = palette();
    if diverted() {
        eprintln!("{}{}{}", p.green, msg, p.nc);
    } else {
        println!("{}{}{}", p.green, msg, p.nc);
    }
}

/// A plain `echo` to stdout (stderr under `--resolve-json`), no colour.
pub fn say(msg: &str) {
    if diverted() {
        eprintln!("{msg}");
    } else {
        println!("{msg}");
    }
}

/// A plain `echo … >&2` — always stderr, diversion or not.
pub fn say_err(msg: &str) {
    eprintln!("{msg}");
}
