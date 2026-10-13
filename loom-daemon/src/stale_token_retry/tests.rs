//! #9247: the write-side stale-token retry. Every refresh here is an
//! injected closure or a counting [`GithubAppMinter`] double, and every `gh`
//! is a stub script: no test mints a token or reaches a forge.

use std::cell::Cell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use log::Level;

use super::*;
use crate::credential_preflight::{
    force_refresh_owner_credential_with, GithubAppMinter, GithubAppOutcome,
};
use crate::test_log_capture::capture_logs;

/// What `gh issue|pr` prints for the refusal (GraphQL: no HTTP marker).
pub(crate) const REFUSED: &str = "GraphQL: Resource not accessible by integration (addComment)";

/// A token-SHAPED string, assembled here so no fixture carries one.
pub(crate) fn token_shaped() -> String {
    format!("ghs_{}", "A".repeat(36))
}

/// A stub `gh` logging its argv to `<dir>/gh.log`. The FIRST call of each
/// `<arg1> <arg2>` pair (`pr comment`, `pr edit`, `issue edit`) is refused
/// with [`REFUSED`] plus a token-shaped string on stderr; later calls of that
/// pair succeed. With `always`, every call is refused.
pub(crate) fn refusing_gh(dir: &Path, always: bool) -> (PathBuf, PathBuf) {
    let gh = dir.join("stale-token-gh.sh");
    let log = dir.join("gh.log");
    let script = format!(
        "#!/bin/sh\nD='{d}'\necho \"$*\" >> \"$D/gh.log\"\nseen=\"$D/seen-$1-$2\"\n\
         if [ {always} = 1 ] || [ ! -f \"$seen\" ]; then\n  : > \"$seen\"\n  \
         echo '{REFUSED} {token}' >&2\n  exit 1\nfi\nexit 0\n",
        d = dir.display(),
        always = u8::from(always),
        token = token_shaped(),
    );
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    (gh, log)
}

/// How many logged `gh` calls contain `needle`.
pub(crate) fn calls(log: &Path, needle: &str) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(needle))
        .count()
}

/// Install a counting refresh that answers `fresh` for this test thread; the
/// returned counter is how many times a write site asked.
pub(crate) fn counting_refresh(fresh: bool) -> std::rc::Rc<Cell<usize>> {
    let count = std::rc::Rc::new(Cell::new(0));
    let seen = std::rc::Rc::clone(&count);
    set_test_refresh(Some(Box::new(move |_| {
        seen.set(seen.get() + 1);
        fresh
    })));
    count
}

fn git_repo_with_remote(owner_repo: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "-q"],
        vec![
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{owner_repo}.git"),
        ],
    ] {
        assert!(Command::new("git")
            .args(&args)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
    }
    dir
}

/// Run `retry_once` over a write that fails `failures` times with `text`,
/// returning `(result, writes, refreshes)`.
fn drive(text: &str, failures: usize, fresh: bool) -> (Result<&'static str>, usize, usize) {
    let (writes, refreshes) = (Cell::new(0), Cell::new(0));
    let refresh = |_: &Path| {
        refreshes.set(refreshes.get() + 1);
        fresh
    };
    let out = retry_once("test.op", Path::new("/ws/root"), &refresh, || {
        writes.set(writes.get() + 1);
        if writes.get() <= failures {
            Err(anyhow!(
                "gh pr comment failed for #1 in /ws/root: {text} (attempt {})",
                writes.get()
            ))
        } else {
            Ok("landed")
        }
    });
    (out, writes.get(), refreshes.get())
}

// ---- the predicate ----

#[test]
fn the_signature_is_the_integration_refusal_in_any_case() {
    for text in [
        REFUSED,
        "gh: Resource not accessible by integration (HTTP 403)",
        "GraphQL: RESOURCE NOT ACCESSIBLE BY INTEGRATION (addLabelsToLabelable)",
        "gh issue edit failed for #7: GraphQL: Resource not accessible by integration",
    ] {
        assert!(is_stale_token_signature(text), "{text}");
    }
}

#[test]
fn a_rate_limit_is_never_the_signature() {
    for text in [
        "gh: API rate limit exceeded for installation ID 1 (HTTP 403)",
        "GraphQL: API rate limit already exceeded for installation ID 151241294",
        "gh: You have exceeded a secondary rate limit (HTTP 403)",
        // Both in one message: the rate limit wins.
        "Resource not accessible by integration; API rate limit exceeded (HTTP 403)",
        "Resource not accessible by integration; you have exceeded a secondary rate limit",
    ] {
        assert!(!is_stale_token_signature(text), "{text}");
    }
}

#[test]
fn other_failures_are_never_the_signature() {
    for text in [
        "gh: Validation Failed (HTTP 422)",
        "gh: Not Found (HTTP 404)",
        "gh: Bad credentials (HTTP 401)",
        "gh: Must have admin rights to Repository. (HTTP 403)",
        "gh pr comment exceeded 120s and was killed",
        "failed to invoke gh (verdict.anchor_comment): No such file or directory",
        "",
    ] {
        assert!(!is_stale_token_signature(text), "{text}");
    }
}

/// The classifier this predicate leans on is untouched: the text is still a
/// permission denial, and the #6171 predicate still knows nothing of it.
#[test]
fn the_denial_classifier_and_the_404_predicate_are_unchanged() {
    let text = "gh: Resource not accessible by integration (HTTP 403)";
    assert_eq!(classify(None, text, None), Some(Denial::Permission));
    assert!(!crate::forge_listing::is_404_error(text));
    assert!(crate::forge_listing::is_404_error("gh: Not Found (HTTP 404)"));
}

// ---- the helper ----

#[test]
fn a_refused_write_is_refreshed_once_and_rerun_once() {
    let (out, writes, refreshes) = drive(REFUSED, 1, true);
    assert_eq!(out.unwrap(), "landed");
    assert_eq!((writes, refreshes), (2, 1));
}

#[test]
fn a_failed_retry_surfaces_the_original_error_and_stops() {
    let (out, writes, refreshes) = drive(REFUSED, usize::MAX, true);
    let err = out.unwrap_err().to_string();
    assert_eq!(
        err,
        format!("gh pr comment failed for #1 in /ws/root: {REFUSED} (attempt 1)"),
        "the ORIGINAL error, unchanged"
    );
    assert_eq!((writes, refreshes), (2, 1), "no third attempt, no second refresh");
}

#[test]
fn a_refresh_that_did_nothing_means_no_rerun() {
    let (out, writes, refreshes) = drive(REFUSED, usize::MAX, false);
    assert!(out.unwrap_err().to_string().ends_with("(attempt 1)"));
    assert_eq!((writes, refreshes), (1, 1));
}

#[test]
fn a_rate_limit_403_neither_refreshes_nor_reruns() {
    for text in [
        "gh: API rate limit exceeded for installation ID 1 (HTTP 403)",
        "gh: You have exceeded a secondary rate limit (HTTP 403)",
    ] {
        let (out, writes, refreshes) = drive(text, usize::MAX, true);
        assert!(out.unwrap_err().to_string().contains(text));
        assert_eq!((writes, refreshes), (1, 0), "{text}");
    }
}

#[test]
fn any_other_failure_neither_refreshes_nor_reruns() {
    for text in [
        "gh: Validation Failed (HTTP 422)",
        "gh: Not Found (HTTP 404)",
        "gh pr comment exceeded 120s and was killed",
        "failed to invoke gh (verdict.anchor_comment): No such file or directory",
    ] {
        let (out, writes, refreshes) = drive(text, usize::MAX, true);
        assert!(out.is_err());
        assert_eq!((writes, refreshes), (1, 0), "{text}");
    }
}

#[test]
fn a_write_that_succeeds_first_time_never_refreshes() {
    assert_eq!(drive(REFUSED, 0, true).1, 1);
    assert_eq!(drive(REFUSED, 0, true).2, 0);
}

// ---- logging ----

/// The retry is one `info` line in the #6171 shape, and nothing the forge
/// said (here: a token-shaped string in the refusal) reaches any log line.
#[test]
fn the_retry_logs_one_info_line_and_never_the_forge_response() {
    let secret = token_shaped();
    let refused = format!("{REFUSED} {secret}");
    for failures in [1, usize::MAX] {
        let logs = capture_logs(|| {
            let _ = drive(&refused, failures, true);
        });
        let info: Vec<_> = logs.iter().filter(|(l, _)| *l == Level::Info).collect();
        assert_eq!(info.len(), 1, "{logs:?}");
        assert_eq!(
            info[0].1,
            "stale_token_retry: retrying test.op in /ws/root after a forced per-owner \
             credential refresh (#9247)"
        );
        let warns = logs.iter().filter(|(l, _)| *l == Level::Warn).count();
        assert_eq!(warns, usize::from(failures > 1), "one warn only when the retry failed");
        for (_, line) in &logs {
            assert!(!line.contains(&secret) && !line.contains("ghs_"), "{line}");
            assert!(!line.to_ascii_lowercase().contains(SIGNATURE), "no stderr: {line}");
        }
    }
}

// ---- the debounce ----

/// Counts forced mints; never configured, so nothing is minted or published.
#[derive(Default)]
struct CountingMinter {
    forced: Cell<usize>,
}

impl GithubAppMinter for CountingMinter {
    fn mint(&self, _owner_repo: &str) -> GithubAppOutcome {
        panic!("this path only ever force-mints");
    }
    fn mint_forced(&self, _owner_repo: &str) -> GithubAppOutcome {
        self.forced.set(self.forced.get() + 1);
        GithubAppOutcome::NotConfigured
    }
}

#[test]
fn an_owner_gets_one_forced_mint_per_window_and_a_second_owner_its_own() {
    let workspace = tempfile::tempdir().unwrap();
    let debounce = Debounce::default();
    let minter = CountingMinter::default();
    let invalidated = Cell::new(0);
    let start = Instant::now();
    let writes = Cell::new(0);
    // One failing write for `root` at `at`, through the real seam.
    let failing_write = |root: &Path, at: Instant| {
        let mint = |r: &Path| force_refresh_owner_credential_with(workspace.path(), r, &minter);
        let invalidate = |_: &Path| invalidated.set(invalidated.get() + 1);
        let refresh = |r: &Path| refresh_using(&debounce, at, r, &mint, &invalidate);
        retry_once("test.op", root, &refresh, || {
            writes.set(writes.get() + 1);
            Err::<(), _>(anyhow!("{REFUSED}"))
        })
        .unwrap_err()
        .to_string()
    };

    let (a1, a2) = (git_repo_with_remote("acme/one"), git_repo_with_remote("ACME/two"));
    let other = git_repo_with_remote("globex/site");
    let logs = capture_logs(|| {
        for (i, root) in [a1.path(), a2.path(), a1.path(), a2.path()]
            .into_iter()
            .enumerate()
        {
            let at = start + Duration::from_secs(60 * i as u64);
            assert_eq!(failing_write(root, at), REFUSED, "the original error, no retry");
        }
    });
    assert_eq!(minter.forced.get(), 1, "four failing writes, one owner, one mint");
    assert_eq!(writes.get(), 4, "no write was re-run");
    let warns = logs.iter().filter(|(l, _)| *l == Level::Warn).count();
    assert!(warns <= 1, "at most one warn per owner per window: {logs:?}");

    failing_write(other.path(), start + Duration::from_secs(61));
    assert_eq!(minter.forced.get(), 2, "a second owner still gets its own mint");

    failing_write(a1.path(), start + DEBOUNCE_WINDOW - Duration::from_secs(1));
    assert_eq!(minter.forced.get(), 2, "still inside the first owner's window");
    failing_write(a1.path(), start + DEBOUNCE_WINDOW);
    assert_eq!(minter.forced.get(), 3, "the window closed");
    assert_eq!(invalidated.get(), 0, "nothing is invalidated without a fresh token");
}

/// A mint that SUCCEEDED also opens the window: when the retry still fails
/// it was a real permission gap, and the owner's next refusal is not minted
/// for or retried.
#[test]
fn a_successful_mint_also_debounces_the_owner() {
    let root = git_repo_with_remote("acme/app");
    let debounce = Debounce::default();
    let (mints, invalidated, writes) = (Cell::new(0), Cell::new(0), Cell::new(0));
    let now = Instant::now();
    let mint = |_: &Path| {
        mints.set(mints.get() + 1);
        true
    };
    let invalidate = |_: &Path| invalidated.set(invalidated.get() + 1);
    let refresh = |r: &Path| refresh_using(&debounce, now, r, &mint, &invalidate);
    for _ in 0..3 {
        let err = retry_once("test.op", root.path(), &refresh, || {
            writes.set(writes.get() + 1);
            Err::<(), _>(anyhow!("{REFUSED}"))
        });
        assert_eq!(err.unwrap_err().to_string(), REFUSED);
    }
    assert_eq!(mints.get(), 1);
    assert_eq!(invalidated.get(), 1, "caches dropped once, after the one fresh token");
    assert_eq!(writes.get(), 4, "one retry for the first write; none inside the window");
}

#[test]
fn a_root_with_no_remote_has_nothing_to_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let mint = |_: &Path| panic!("no owner, no mint");
    let invalidate = |_: &Path| panic!("no mint, no invalidation");
    assert!(!refresh_using(
        &Debounce::default(),
        Instant::now(),
        dir.path(),
        &mint,
        &invalidate
    ));
}

/// A write site in a test never reaches the real minter: with nothing
/// installed on this thread the refresh answers "no retry".
#[test]
fn the_test_build_refresh_is_inert_until_a_test_installs_one() {
    set_test_refresh(None);
    assert!(!refresh(Path::new("/ws/root")));
    let asked = counting_refresh(true);
    assert!(refresh(Path::new("/ws/root")));
    assert_eq!(asked.get(), 1);
    set_test_refresh(None);
}

// ---- cache invalidation ----

mod invalidation;
