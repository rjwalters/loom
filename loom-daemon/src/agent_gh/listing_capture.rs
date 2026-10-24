//! Journal the `issue|pr list --json …` calls the front passes through
//! (#11858).
//!
//! The front serves, and journals for the tick's `pick.decision`
//! ([`crate::observability::pick_journal::record_listing`]), only the listing
//! shapes whose projection is `gh`-identical ([`super::classify`]). A listing
//! that asks for `labels` or uses `--search` passes through to `gh`, and
//! Curator's queue queries are exactly those, so its ticks read
//! `candidate_source=none` even when they found and finished work.
//!
//! Inside a role tick (a pick journal is attached), off a TTY, such a listing
//! now runs captured instead of `exec`'d: its stdout, stderr and exit status
//! are replayed unchanged and its rows are journaled. Every other call, and
//! every call outside a role tick, is untouched. This adds no forge call: it
//! records the rows `gh` already printed.

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

/// The stage name to journal a passed-through `args` listing under, or
/// `None` when its stdout is not the JSON rows the journal reads: not an
/// `issue|pr list`, no `--json` naming `number`, or a flag that reshapes the
/// output (`--jq`, `--template`, `--web`). The stage is the `--label`
/// value(s), else `search` for a `--search` query, else `listing`.
#[must_use]
pub fn stage(args: &[String]) -> Option<String> {
    let (Some(command), Some(verb)) = (args.first(), args.get(1)) else {
        return None;
    };
    if !matches!(command.as_str(), "issue" | "pr") || verb != "list" {
        return None;
    }
    let rest = &args[2..];
    let mut labels = Vec::new();
    let (mut fields, mut search) = (None, false);
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f, Some(v)),
            _ => (a, None),
        };
        let takes_value = matches!(
            flag,
            "--json" | "--label" | "-l" | "--search" | "-S" | "--jq" | "-q" | "--template" | "-t"
        );
        let value = if takes_value && inline.is_none() {
            i += 1;
            rest.get(i).map(String::as_str)
        } else {
            inline
        };
        match flag {
            "--jq" | "-q" | "--template" | "-t" | "--web" | "-w" => return None,
            "--json" => fields = value,
            "--label" | "-l" => labels.extend(value.map(str::to_string)),
            "--search" | "-S" => search = true,
            _ => {}
        }
        i += 1;
    }
    fields?.split(',').any(|f| f.trim() == "number").then(|| {
        if !labels.is_empty() {
            labels.join(",")
        } else if search {
            "search".to_string()
        } else {
            "listing".to_string()
        }
    })
}

/// Run a recordable passthrough listing captured, replay its output and
/// journal its rows. `None` when the call is not one (see [`stage`]), no
/// journal is attached, stdout is a terminal, or the child could not be
/// spawned: the caller then `exec`s it as before.
pub fn run(next: &Path, raw: &[OsString], depth: u32) -> Option<i32> {
    use std::io::IsTerminal;
    if !crate::observability::pick_journal::active() || std::io::stdout().is_terminal() {
        return None;
    }
    let args = raw
        .iter()
        .map(|a| a.to_str().map(str::to_string))
        .collect::<Option<Vec<String>>>()?;
    let stage = stage(&args)?;
    let out = crate::gh_invocation::transparent::run_captured(
        next,
        raw,
        &[(super::SENTINEL_ENV, OsString::from((depth + 1).to_string()))],
        &[],
    )
    .ok()?;
    let _ = std::io::stdout().lock().write_all(&out.stdout);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().lock().write_all(&out.stderr);
    if out.status.success() {
        record(&stage, &out.stdout);
    }
    Some(super::exit_code(out.status))
}

/// Journal the rows of a listing's captured stdout under `stage`.
pub fn record(stage: &str, stdout: &[u8]) {
    crate::observability::pick_journal::record_gh_listing(stage, &String::from_utf8_lossy(stdout));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::stage;

    fn argv(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn the_curator_queue_listings_are_recordable() {
        assert_eq!(
            stage(&argv("issue list --label loom:triage --json number,title,labels")).as_deref(),
            Some("loom:triage")
        );
        assert_eq!(
            stage(&argv("issue list --search label:loom:curated --json=number,labels")).as_deref(),
            Some("search")
        );
        assert_eq!(
            stage(&argv("pr list -l loom:pr -l loom:review-requested --json number")).as_deref(),
            Some("loom:pr,loom:review-requested")
        );
        assert_eq!(stage(&argv("issue list --json number,labels")).as_deref(), Some("listing"));
    }

    #[test]
    fn output_that_is_not_json_rows_is_not_recordable() {
        for call in [
            "issue list --label loom:triage",
            "issue list --json title,labels",
            "issue list --json number --jq .[].number",
            "issue list --json number -q .[]",
            "issue list --json number --template {{.}}",
            "issue list --json number --web",
            "issue view 7 --json number",
            "api repos/o/r/issues",
        ] {
            assert_eq!(stage(&argv(call)), None, "{call}");
        }
    }
}
