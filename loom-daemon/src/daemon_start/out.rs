//! The script's three output helpers, reproduced exactly.
//!
//! ```sh
//! if [[ -t 1 ]]; then RED=…; GREEN=…; YELLOW=…; BOLD=…; NC=…; else …=''; fi
//! err()  { echo -e "${RED}$*${NC}" >&2; }
//! warn() { echo -e "${YELLOW}$*${NC}" >&2; }
//! ok()   { echo -e "${GREEN}$*${NC}"; }
//! ```
//!
//! Two details are load-bearing and easy to lose:
//!
//! * **The colour test is on STDOUT (`-t 1`), but `err`/`warn` write to
//!   STDERR.** So a run with a terminal stdout and a redirected stderr still
//!   colours the stderr lines. Testing `isatty(2)` for the stderr writers would
//!   read as the obvious fix and would change what a captured log contains.
//! * **`err` and `warn` go to stderr; `ok` goes to stdout.** The retained suite
//!   captures them separately (`2>&1` in some cases, `2>/dev/null` in others),
//!   so a line on the wrong stream is invisible to one case and noise in
//!   another.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// ANSI codes, or empty strings when stdout is not a terminal.
pub struct Palette {
    pub red: &'static str,
    pub green: &'static str,
    pub yellow: &'static str,
    pub bold: &'static str,
    pub nc: &'static str,
}

static PALETTE: OnceLock<Palette> = OnceLock::new();

/// Resolve once per process, exactly as the script resolved once per run.
pub fn palette() -> &'static Palette {
    PALETTE.get_or_init(|| {
        if std::io::stdout().is_terminal() {
            Palette {
                red: "\x1b[0;31m",
                green: "\x1b[0;32m",
                yellow: "\x1b[1;33m",
                bold: "\x1b[1m",
                nc: "\x1b[0m",
            }
        } else {
            Palette {
                red: "",
                green: "",
                yellow: "",
                bold: "",
                nc: "",
            }
        }
    })
}

/// `err()` — red, stderr.
pub fn err(msg: &str) {
    let p = palette();
    eprintln!("{}{}{}", p.red, msg, p.nc);
}

/// `warn()` — yellow, stderr.
pub fn warn(msg: &str) {
    let p = palette();
    eprintln!("{}{}{}", p.yellow, msg, p.nc);
}

/// `ok()` — green, stdout.
pub fn ok(msg: &str) {
    let p = palette();
    println!("{}{}{}", p.green, msg, p.nc);
}

/// A plain `echo` to stdout (no colour, no `-e`).
pub fn say(msg: &str) {
    println!("{msg}");
}

/// A plain `echo … >&2` to stderr (no colour, no `-e`).
pub fn say_err(msg: &str) {
    eprintln!("{msg}");
}
