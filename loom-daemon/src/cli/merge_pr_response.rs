//! `loom-daemon merge-pr classify-response` (#8191 slice): the decision half of
//! `merge-pr.sh`'s merge-retry ladder — which route a failed merge's error text
//! sends the loop down.
//!
//! # Protocol
//!
//! The forge's response text arrives on **stdin** and exactly one line is
//! printed:
//!
//! ```text
//! LOOM-MERGE-RESPONSE <token>
//! ```
//!
//! with `<token>` one of `merge-in-progress`, `head-mismatch`,
//! `base-modified`, `other` — see [`MergeResponseKind::token`].
//!
//! **stdin, never argv.** The response is forge-controlled text captured with
//! `2>&1`: multi-line, arbitrary length, and on a bad day not valid UTF-8. An
//! argument vector is the wrong place for all three (and `bash`'s `echo`, which
//! the retired shell used, silently swallows an argument that is exactly `-n` /
//! `-e` / `-E`). The shell side pipes it with `printf '%s'` rather than `echo`
//! for the same reason.
//!
//! # Exit code
//!
//! **Always 0 when a classification was printed** — including `other`. This
//! verb is a classifier, not a gate: `other` is a real answer ("no marker
//! matched"), and spending a non-zero exit on it would make it
//! indistinguishable from "the classifier could not run". Exit 2 only when
//! stdin could not be read at all, which is likewise not a verdict.
//!
//! That distinction is the whole contract, because the two are answered
//! oppositely upstream. `merge-pr.sh` accepts **only** exit 0 plus the
//! `LOOM-MERGE-RESPONSE ` sentinel; anything else — a missing binary, one
//! predating this verb (clap exits 2), a clap usage error, silence — is
//! reported as a helper failure that refuses the merge *and says so*, naming
//! the roll. It is never folded into `other`, and never into a route.
//!
//! **Why fail CLOSED here, on a verb that only ever runs after a merge has
//! already failed.** The three real routes are not interchangeable: two of them
//! retry and one of those must never be taken for a head that moved past the
//! approved SHA (#5579). An unresolvable classifier therefore offers only a
//! choice between guessing a route and refusing; and of the two, only refusing
//! can *report* which happened. Nothing healthy is stopped by this — a merge
//! that succeeds never reaches the classifier at all — so the cost of the
//! strict reading falls exactly on a host whose daemon needs rolling, with a
//! message that names the roll.
//!
//! No `requires-daemon:` floor was raised for it, deliberately; see the
//! `requires-daemon:` block in `merge-pr.sh` for why the `merge-pr` group's
//! floor stays at the merge-gate version.

use anyhow::Result;
use loom_daemon::merge_pr::response::{classify, MergeResponseKind};
use std::io::{Read, Write};

#[derive(clap::Args)]
pub(crate) struct ClassifyResponseArgs {}

impl ClassifyResponseArgs {
    pub(crate) fn run(self) -> Result<()> {
        // `read_to_end`, not `read_to_string`: the response need not be valid
        // UTF-8 and `grep` did not require it to be. See
        // `merge_pr::response::ascii_icontains`.
        let mut body: Vec<u8> = Vec::new();
        if std::io::stdin().read_to_end(&mut body).is_err() {
            eprintln!("merge-pr classify-response: could not read the merge response from stdin");
            std::process::exit(2);
        }
        let kind: MergeResponseKind = classify(&body);
        let mut out = std::io::stdout().lock();
        writeln!(out, "LOOM-MERGE-RESPONSE {}", kind.token())?;
        out.flush()?;
        Ok(())
    }
}
