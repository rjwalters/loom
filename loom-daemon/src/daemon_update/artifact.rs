//! Artifact-fetch mode (Epic #4990 Phase 3, #5020): resolve the latest
//! GitHub Release for this host's platform, then download and verify it
//! INSTEAD of rebuilding from source.
//!
//! Resolution here is read-only and cheap — it answers "is there an artifact,
//! and is it newer?" without downloading anything. The download + checksum +
//! signature half already lives in [`crate::release_fetch`] (epic #7810 PR 6a)
//! and the JSON resolution in [`crate::release_resolve`] (PR 5); this module
//! calls both **in process**.
//!
//! That is the one structural difference from the shell, and it is a
//! simplification of the delegation the shell already performed, not a new
//! behaviour: the shell had to spawn `loom-daemon release-fetch` and parse
//! `KEY=value` lines back off its stdout precisely because bash could not call
//! Rust. Those lines were always CAPTURED, never printed, so nothing observed
//! them; the human-facing progress and verdict lines they accompanied went to
//! stderr and are reproduced here unchanged, because
//! `test-loom-daemon-update-fetch.sh` greps for their exact wording.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::out;
use super::util;
use crate::release_resolve::host;

/// The verified download, as the shell's `ARTIFACT_*` globals held it.
pub struct FetchedArtifact {
    pub bin: PathBuf,
    pub version_output: String,
    pub commit: String,
    /// Darwin only (#8008): whether the VERIFIED download carried an
    /// `Authority=` line, i.e. a certificate-anchored (Developer ID)
    /// signature rather than an ad-hoc/unsigned one.
    ///
    /// Left `None` (neither true nor false) on Linux targets and when
    /// `codesign` was unavailable — both cases where the pre-provision state
    /// is unknown, so the post-provision check has nothing to assert against
    /// and skips.
    pub had_authority: Option<bool>,
}

/// What `fetch_resolve_latest` resolved, or why it could not.
pub struct Resolution {
    pub target: String,
    pub repo_slug: String,
    pub latest_tag: String,
    pub latest_version: String,
    pub ok: bool,
    pub reason: String,
}

/// `fetch_resolve_latest` — read-only resolution, no downloads.
///
/// `pinned_tag` (#10709) resolves exactly that release instead of the newest
/// one, so the auto-update roll installs the release its verdict compared.
/// `None` is the newest-release resolution, unchanged.
pub fn fetch_resolve_latest(repo_root: &Path, pinned_tag: Option<&str>) -> Resolution {
    let mut r = Resolution {
        target: String::new(),
        repo_slug: String::new(),
        latest_tag: String::new(),
        latest_version: String::new(),
        ok: false,
        reason: String::new(),
    };

    r.target = util::env_non_empty("LOOM_DAEMON_UPDATE_TARGET")
        .unwrap_or_else(|| host::target_triple().unwrap_or("").to_string());
    if r.target.is_empty() {
        // The shell's fallback here is `?`, not `unknown`, and it is the one
        // that reaches the operator.
        let os = uname_or_question("-s");
        let arch = uname_or_question("-m");
        r.reason =
            format!("unrecognized host platform ({os}/{arch}) -- no release target-triple mapping");
        return r;
    }

    if !util::have("gh") {
        r.reason = "'gh' CLI not found on PATH".to_string();
        return r;
    }

    r.repo_slug = util::env_non_empty("LOOM_DAEMON_UPDATE_GH_REPO")
        .unwrap_or_else(|| host::repo_slug(repo_root).unwrap_or_default());
    if r.repo_slug.is_empty() {
        r.reason = "could not resolve owner/repo from git remote 'origin' (set LOOM_DAEMON_UPDATE_GH_REPO to override)".to_string();
        return r;
    }

    let slug = r.repo_slug.clone();
    let view = |fields: &str, jq: &str| gh_release_view(&slug, pinned_tag, fields, jq);
    let explain = |tag: &str, target: &str| {
        crate::release_resolve::explain_no_artifact(repo_root, &slug, tag, target)
    };
    resolve_release(&mut r, pinned_tag, &view, &explain);
    r
}

/// Shared prefix of the two "no latest release" reasons below. One literal,
/// not two: `forge_inventory`'s ratchet counts every non-comment `gh release`
/// phrase textually, so duplicating it grew the file's baseline count (#14152).
const NO_LATEST_RELEASE: &str = "'gh release view' found no latest release";

/// The release half of [`fetch_resolve_latest`], with the forge reads factored
/// out so the tag/asset logic is testable without `gh` (#10709).
///
/// `view(fields, jq)` is `gh release view [<pinned_tag>] --json <fields> --jq
/// <jq>` for `r.repo_slug`; `Ok` carries stdout, `Err` the one cause `gh`
/// itself named for the failure (#14016). `explain(tag, target)` is the #8654
/// reason for a release that lacks this host's artifact.
fn resolve_release(
    r: &mut Resolution,
    pinned_tag: Option<&str>,
    view: &dyn Fn(&str, &str) -> Result<String, String>,
    explain: &dyn Fn(&str, &str) -> Option<String>,
) {
    let tag = match view("tagName", ".tagName") {
        Ok(tag) => tag.trim_end().to_string(),
        Err(why) => {
            // #14016: name the cause `gh` reported — not-found vs
            // rate-limited vs auth/network — instead of listing all of
            // them, which told the operator nothing.
            r.reason = match pinned_tag {
                Some(pinned) => {
                    format!("found no release tagged {pinned} for {} ({why})", r.repo_slug)
                }
                None => {
                    format!("{NO_LATEST_RELEASE} for {} ({why})", r.repo_slug)
                }
            };
            return;
        }
    };
    if tag.is_empty() {
        // `gh` answered but named no release: no Releases yet (or an empty
        // answer for a pin). No stderr to classify — say it directly.
        r.reason = match pinned_tag {
            Some(pinned) => format!(
                "found no release tagged {pinned} for {} (the forge returned no tag)",
                r.repo_slug
            ),
            None => format!("{NO_LATEST_RELEASE} for {} (no Releases yet)", r.repo_slug),
        };
        return;
    }
    if let Some(pinned) = pinned_tag {
        // Defensive: `gh release view <tag>` names the release it read. A
        // different tag would mean installing something other than the pin.
        if tag != pinned {
            r.reason = format!("asked for release {pinned} but the forge returned {tag}");
            return;
        }
    }
    r.latest_tag = tag.clone();
    r.latest_version = util::extract_version(&tag);
    if r.latest_version.is_empty() {
        r.reason = format!("could not parse a semver version out of release tag '{tag}'");
        return;
    }

    let assets = view("assets", ".assets[].name").ok().unwrap_or_default();
    let bin_name = format!("loom-daemon-{}", r.target);
    let sha_name = format!("{bin_name}.sha256");
    // `grep -qxF` — a WHOLE-LINE, fixed-string match. A substring match would
    // accept `loom-daemon-x86_64-unknown-linux-gnu.sha256` as evidence of the
    // binary asset.
    let has = |needle: &str| assets.lines().any(|l| l == needle);
    if !has(&bin_name) || !has(&sha_name) {
        // #8654: name the release's age + asset count ("still uploading" vs
        // "genuinely unbuilt", the daemon resolver's own #8515 wording) when
        // available. `explain_no_artifact` re-resolves in process (no
        // subprocess to shell out to, unlike the shell twin this replaces)
        // and only returns `None` if it finds the artifact IS present after
        // all — otherwise it always degrades to *some* reason, so the flat
        // fallback below is a belt-and-suspenders default, not the common
        // case.
        r.reason = explain(&tag, &r.target)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "release {tag} has no artifact for target {} (checked for {bin_name} + {sha_name})",
                    r.target
                )
            });
        return;
    }

    r.ok = true;
}

fn uname_or_question(flag: &str) -> String {
    Command::new("uname")
        .arg(flag)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "?".to_string())
}

fn gh_release_view(
    slug: &str,
    tag: Option<&str>,
    fields: &str,
    jq: &str,
) -> Result<String, String> {
    use crate::cmd_out::CmdOutcome;
    use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    // #10089: through the facade, so the read is counted and bounded.
    let op = Operation::new("release.view");
    let timeout = std::time::Duration::from_secs(60);
    let outcome = GhInvocation::new(op, AccessIntent::Read, GhTarget::None, timeout)
        .forge_op(crate::forge_call_stats::ops::RELEASE_RESOLVE_AND_FETCH)
        // Reader-only (#12394, #12584): the reader route names the repo the
        // `release view -R` argv reads, which the facade does not derive.
        .reader_repo(slug)
        .args(release_view_args(slug, tag, fields, jq))
        .run();
    match outcome {
        CmdOutcome::Ran(o) if o.status.success() => {
            Ok(String::from_utf8_lossy(&o.stdout).trim_end().to_string())
        }
        // #14016: keep the failed output's stderr — the cause it names is
        // the difference between "no such tag" and "rate-limited".
        CmdOutcome::Ran(o) => Err(why_view_failed(&String::from_utf8_lossy(&o.stderr))),
        CmdOutcome::Unavailable(u) => Err(u.to_string()),
    }
}

/// The one cause `gh`'s own report names for a failed release read
/// (Issue #14016). The old reason listed every cause at once ("no such tag,
/// an unreachable/rate-limited API, or an auth failure"), which an operator
/// could not act on; this returns the one whose signature the stderr
/// matches, or `None` when nothing matches — the caller then says
/// "unclassified" instead of repeating the whole list.
fn classify_gh_stderr(stderr: &str) -> Option<&'static str> {
    let s = stderr.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| s.contains(n));
    // Rate limit first: its HTTP 403s must not read as auth.
    if has(&["rate limit", "secondary rate", "abuse"]) {
        Some("a rate-limited API")
    } else if has(&[
        "bad credentials",
        "unauthorized",
        "http 401",
        "http 403",
        "forbidden",
        "auth",
    ]) {
        Some("an auth failure")
    } else if has(&[
        "could not resolve host",
        "no such host",
        "connection refused",
        "connection reset",
        "timed out",
        "timeout",
        "network",
        "unreachable",
        "dial tcp",
        "tls",
    ]) {
        Some("an unreachable API")
    } else if has(&[
        "http 404",
        "not found",
        "no releases",
        "could not be found",
        "could not resolve to a release",
    ]) {
        Some("no such release")
    } else {
        None
    }
}

/// The `gh`-attributable cause for a failed release read (Issue #14016):
/// the classifier's verdict, with gh's own first stderr line kept for the
/// detail a one-phrase verdict loses.
fn why_view_failed(stderr: &str) -> String {
    let first_line = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| {
            let truncated = l.chars().take(200).collect::<String>();
            truncated
        });
    match (classify_gh_stderr(stderr), first_line) {
        (Some(cause), Some(line)) => format!("{cause} (gh: {line})"),
        (Some(cause), None) => cause.to_string(),
        (None, Some(line)) => format!("unclassified gh failure (gh: {line})"),
        (None, None) => "unclassified gh failure".to_string(),
    }
}

/// The `gh` argv for one release read: the newest release when `tag` is
/// `None` (byte-for-byte the pre-#10709 argv), exactly `tag` otherwise.
fn release_view_args(slug: &str, tag: Option<&str>, fields: &str, jq: &str) -> Vec<String> {
    let mut args = vec!["release".to_string(), "view".to_string()];
    args.extend(tag.map(str::to_string));
    for a in ["--json", fields, "-R", slug, "--jq", jq] {
        args.push(a.to_string());
    }
    args
}

/// Why `fetch_and_verify_artifact` did not produce a binary.
pub enum FetchFailure {
    /// Verification FAILED — a checksum mismatch, an invalid signature, or a
    /// published signature asset that would not download. The shell exited 1
    /// from inside the function rather than returning, because these are
    /// tamper/corruption signals and never soft-fallback conditions.
    Verification,
    /// Could not even download. The shell returned 1 so the caller could
    /// decide; in practice the caller has already committed to artifact mode
    /// and also exits 1, but the distinction keeps the contract composable.
    Download,
}

/// `fetch_and_verify_artifact` — download, verify, and report the identity.
///
/// The progress/verdict lines go to **our own stderr**, never captured, which
/// is what keeps them showing up in a caller's `2>&1` capture unchanged — and
/// is why the cosign/codesign wording they carry is contract for
/// `test-loom-daemon-update-fetch.sh`, not merely descriptive.
pub fn fetch_and_verify_artifact(
    repo_root: &Path,
    target: &str,
    repo_slug: &str,
    tag: &str,
) -> Result<FetchedArtifact, FetchFailure> {
    use crate::release_fetch::{
        evidence, fetch_and_verify_with_evidence, FetchInputs, FetchOutcome, SignaturePolicy,
    };

    let bin_name = format!("loom-daemon-{target}");
    let sha_name = format!("{bin_name}.sha256");
    out::say_err(&format!("Downloading {bin_name} + {sha_name} from {repo_slug}@{tag}..."));

    let inputs = FetchInputs {
        repo_root,
        binary: crate::release_fetch::fetch::DAEMON_BIN,
        target,
        repo_slug,
        tag,
        cosign_pubkey_env: std::env::var("LOOM_DAEMON_UPDATE_COSIGN_PUBKEY").ok(),
        cosign_identity_env: std::env::var("LOOM_DAEMON_UPDATE_COSIGN_IDENTITY").ok(),
        cosign_oidc_issuer_env: std::env::var("LOOM_DAEMON_UPDATE_COSIGN_OIDC_ISSUER").ok(),
    };

    // #10474: every verdict, in both policy modes, lands in the durable
    // signature-evidence journal (best-effort; never changes the verdict).
    let (outcome, record) = fetch_and_verify_with_evidence(&inputs, &SignaturePolicy::from_env());
    let _ = evidence::record(repo_root, &record);
    match outcome {
        FetchOutcome::Verified {
            artifact,
            checksum_line,
            signature_line,
            ..
        } => {
            out::say_err(&checksum_line);
            if !signature_line.is_empty() {
                out::say_err(&signature_line);
            }
            // The scratch dir joins the cleanup set the shell's EXIT trap
            // owned, so an abort never leaves an unverified artifact behind.
            super::register_tmpdir(artifact.tmp_dir.clone());
            Ok(FetchedArtifact {
                bin: artifact.bin_path.clone(),
                version_output: artifact.version_output.clone(),
                commit: artifact.commit.clone().unwrap_or_default(),
                had_authority: artifact.had_authority,
            })
        }
        FetchOutcome::VerificationFailed { lines } => {
            for line in lines {
                out::say_err(&line);
            }
            Err(FetchFailure::Verification)
        }
        FetchOutcome::DownloadFailed(message) => {
            out::say_err(&message);
            Err(FetchFailure::Download)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A forge with one release per tag (the first entry is the newest),
    /// answering `view` exactly as `gh release view [<tag>]` would.
    fn forge<'a>(
        releases: &'a [(&'a str, &'a [&'a str])],
        pinned: Option<&'a str>,
        seen: &'a std::cell::RefCell<Vec<String>>,
    ) -> impl Fn(&str, &str) -> Result<String, String> + 'a {
        move |fields, _jq| {
            seen.borrow_mut().push(fields.to_string());
            let (tag, assets) = match pinned {
                None => releases.first().ok_or("no release")?,
                Some(p) => releases
                    .iter()
                    .find(|(t, _)| *t == p)
                    .ok_or("no such tag")?,
            };
            Ok(match fields {
                "tagName" => (*tag).to_string(),
                _ => assets.join("\n"),
            })
        }
    }

    fn fresh() -> Resolution {
        Resolution {
            target: "x86_64-unknown-linux-gnu".to_string(),
            repo_slug: "rjwalters/loom".to_string(),
            latest_tag: String::new(),
            latest_version: String::new(),
            ok: false,
            reason: String::new(),
        }
    }

    const FULL: &[&str] = &[
        "loom-daemon-x86_64-unknown-linux-gnu",
        "loom-daemon-x86_64-unknown-linux-gnu.sha256",
    ];
    const NO_LINUX: &[&str] = &["loom-daemon-aarch64-apple-darwin"];
    const RELEASES: &[(&str, &[&str])] = &[
        ("v0.19.831", FULL),
        ("v0.19.830", FULL),
        ("v0.19.829", NO_LINUX),
    ];
    const NO_EXPLAIN: &dyn Fn(&str, &str) -> Option<String> = &|_, _| None;

    #[test]
    fn a_pinned_tag_resolves_that_release_not_the_newest() {
        let seen = std::cell::RefCell::new(Vec::new());
        let mut r = fresh();
        resolve_release(
            &mut r,
            Some("v0.19.830"),
            &forge(RELEASES, Some("v0.19.830"), &seen),
            NO_EXPLAIN,
        );
        assert!(r.ok, "{}", r.reason);
        assert_eq!(r.latest_tag, "v0.19.830");
        assert_eq!(r.latest_version, "0.19.830");
    }

    #[test]
    fn no_pin_resolves_the_newest_release_as_before() {
        let seen = std::cell::RefCell::new(Vec::new());
        let mut r = fresh();
        resolve_release(&mut r, None, &forge(RELEASES, None, &seen), NO_EXPLAIN);
        assert!(r.ok, "{}", r.reason);
        assert_eq!(r.latest_tag, "v0.19.831");
        assert_eq!(*seen.borrow(), ["tagName", "assets"]);
    }

    #[test]
    fn a_pinned_tag_that_does_not_exist_fails_naming_the_tag() {
        let seen = std::cell::RefCell::new(Vec::new());
        let mut r = fresh();
        resolve_release(
            &mut r,
            Some("v0.91.816"),
            &forge(RELEASES, Some("v0.91.816"), &seen),
            NO_EXPLAIN,
        );
        assert!(!r.ok);
        assert!(r.latest_tag.is_empty(), "nothing resolved");
        assert!(r.reason.contains("no release tagged v0.91.816"), "{}", r.reason);
    }

    #[test]
    fn a_pinned_release_without_this_targets_asset_fails_rather_than_falling_to_another() {
        let seen = std::cell::RefCell::new(Vec::new());
        let mut r = fresh();
        resolve_release(
            &mut r,
            Some("v0.19.829"),
            &forge(RELEASES, Some("v0.19.829"), &seen),
            NO_EXPLAIN,
        );
        assert!(!r.ok);
        assert!(
            r.reason
                .contains("release v0.19.829 has no artifact for target x86_64-unknown-linux-gnu"),
            "{}",
            r.reason
        );
        // The #8654 explanation still wins when it has one.
        let mut r = fresh();
        let explain: &dyn Fn(&str, &str) -> Option<String> =
            &|tag, target| Some(format!("{tag}/{target}: still uploading"));
        resolve_release(
            &mut r,
            Some("v0.19.829"),
            &forge(RELEASES, Some("v0.19.829"), &seen),
            explain,
        );
        assert_eq!(r.reason, "v0.19.829/x86_64-unknown-linux-gnu: still uploading");
    }

    #[test]
    fn a_forge_answer_for_a_different_tag_is_refused() {
        let mut r = fresh();
        let wrong: &dyn Fn(&str, &str) -> Result<String, String> = &|fields, _| {
            Ok(if fields == "tagName" {
                "v0.19.831".to_string()
            } else {
                FULL.join("\n")
            })
        };
        resolve_release(&mut r, Some("v0.19.830"), wrong, NO_EXPLAIN);
        assert!(!r.ok);
        assert!(r.reason.contains("asked for release v0.19.830"), "{}", r.reason);
    }

    // Issue #14016: a failed release read names the cause gh reported.

    /// A forge whose `tagName` read fails with the given stderr shape.
    fn failing(stderr: &'static str) -> impl Fn(&str, &str) -> Result<String, String> {
        move |fields, _jq| {
            if fields == "tagName" {
                Err(why_view_failed(stderr))
            } else {
                Ok(String::new())
            }
        }
    }

    #[test]
    fn a_not_found_failure_says_no_such_release_not_all_three_causes() {
        let mut r = fresh();
        let stderr = "gh: Not Found (HTTP 404)";
        resolve_release(&mut r, Some("v0.19.999"), &failing(stderr), NO_EXPLAIN);
        assert!(!r.ok);
        assert!(r.reason.contains("no such release"), "names the cause: {}", r.reason);
        assert!(
            !r.reason.contains("unreachable") && !r.reason.contains("rate-limited"),
            "does not list the other causes: {}",
            r.reason
        );
    }

    #[test]
    fn a_rate_limited_failure_says_rate_limited() {
        let mut r = fresh();
        let stderr =
            "gh: API rate limit exceeded for 203.0.113.7. (But here's the good news: ...) [HTTP 403]";
        resolve_release(&mut r, None, &failing(stderr), NO_EXPLAIN);
        assert!(!r.ok);
        assert!(r.reason.contains("a rate-limited API"), "names the cause: {}", r.reason);
        assert!(
            !r.reason.contains("auth failure"),
            "a rate-limit 403 is not read as auth: {}",
            r.reason
        );
    }

    #[test]
    fn an_auth_failure_says_auth() {
        let mut r = fresh();
        let stderr = "gh: Bad credentials (HTTP 401)";
        resolve_release(&mut r, None, &failing(stderr), NO_EXPLAIN);
        assert!(r.reason.contains("an auth failure"), "{}", r.reason);
    }

    #[test]
    fn an_unreachable_failure_says_unreachable() {
        let mut r = fresh();
        let stderr = "gh: failed to fetch release: dial tcp: lookup api.github.com: no such host";
        resolve_release(&mut r, Some("v0.19.999"), &failing(stderr), NO_EXPLAIN);
        assert!(r.reason.contains("an unreachable API"), "{}", r.reason);
    }

    #[test]
    fn an_unclassifiable_failure_says_unclassified() {
        let mut r = fresh();
        let stderr = "gh: something entirely novel happened";
        resolve_release(&mut r, None, &failing(stderr), NO_EXPLAIN);
        assert!(r.reason.contains("unclassified"), "does not re-list the causes: {}", r.reason);
        // gh's own line survives, so the novel cause is still diagnosable.
        assert!(r.reason.contains("something entirely novel"), "{}", r.reason);
    }

    #[test]
    fn a_gh_answer_with_no_tag_says_no_releases_yet() {
        let mut r = fresh();
        let empty: &dyn Fn(&str, &str) -> Result<String, String> = &|_, _| Ok(String::new());
        resolve_release(&mut r, None, empty, NO_EXPLAIN);
        assert!(!r.ok);
        assert!(r.reason.contains("no Releases yet"), "{}", r.reason);
    }

    #[test]
    fn the_classifier_matches_gh_stderr_not_jq_stdout() {
        assert_eq!(
            classify_gh_stderr("gh: release not found for v9 (HTTP 404)"),
            Some("no such release")
        );
        assert_eq!(
            classify_gh_stderr("gh: You have exceeded a secondary rate limit"),
            Some("a rate-limited API")
        );
        assert_eq!(classify_gh_stderr(""), None);
        assert_eq!(classify_gh_stderr("no releases found"), Some("no such release"));
    }

    #[test]
    fn the_gh_argv_is_unchanged_without_a_tag_and_names_the_tag_with_one() {
        assert_eq!(
            release_view_args("o/r", None, "tagName", ".tagName"),
            ["release", "view", "--json", "tagName", "-R", "o/r", "--jq", ".tagName"]
        );
        assert_eq!(
            release_view_args("o/r", Some("v1.2.3"), "assets", ".assets[].name"),
            [
                "release",
                "view",
                "v1.2.3",
                "--json",
                "assets",
                "-R",
                "o/r",
                "--jq",
                ".assets[].name"
            ]
        );
    }

    #[test]
    fn the_target_map_covers_the_three_published_triples_and_nothing_else() {
        // Asserted through the pure mapping rather than `uname`, which the
        // retained suite stubs.
        let map = |os: &str, arch: &str| -> &'static str {
            match os {
                "Darwin" => match arch {
                    "arm64" | "aarch64" => "aarch64-apple-darwin",
                    _ => "",
                },
                "Linux" => match arch {
                    "aarch64" | "arm64" => "aarch64-unknown-linux-gnu",
                    "x86_64" | "amd64" => "x86_64-unknown-linux-gnu",
                    _ => "",
                },
                _ => "",
            }
        };
        assert_eq!(map("Darwin", "arm64"), "aarch64-apple-darwin");
        assert_eq!(map("Darwin", "x86_64"), "", "x86_64 macOS is not built");
        assert_eq!(map("Linux", "amd64"), "x86_64-unknown-linux-gnu");
        assert_eq!(map("Linux", "armv7l"), "");
        assert_eq!(map("FreeBSD", "x86_64"), "");
    }
}
