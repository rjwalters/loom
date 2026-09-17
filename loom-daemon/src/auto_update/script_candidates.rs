//! Ordered `loom-daemon-update.sh` candidate resolution (Issue #7964), split
//! out of `auto_update.rs` to respect the file-size ratchet
//! (`.loom/docs/file-size-policy.md`).
//!
//! # Why a third candidate
//!
//! Before this module the artifact path had exactly two places to find the
//! update script: the build-time source checkout, else this daemon's own
//! workspace root (the two-branch `script_root` this replaced). #7818's
//! incident showed the failure mode that leaves: a host running
//! `LOOM_WORKSPACE=~/GitHub/anvil` invoked **that workspace's** copy of
//! `loom-daemon-update.sh`, which was old enough not to understand
//! `--resolve-json` at all. It printed no JSON, every tick silently degraded
//! to the source path, and the host sat on 0.19.38 while the fleet moved on —
//! even though a current copy of the very same script was sitting on the same
//! machine under `~/.local/share/loom-daemon/defaults`, the per-machine mirror
//! `scripts/install-loom.sh` / `loom update` maintain.
//!
//! #7818 fixed the *diagnostics* (the log now names the script and its stderr,
//! see [`super::resolve_json`]); this module fixes the *behaviour*: when a
//! candidate's script produces no usable JSON, try the next candidate, ending
//! with the machine-level mirror, before giving up and degrading to the source
//! path.
//!
//! # What the mirror candidate actually substitutes
//!
//! Only the **script file** — not the checkout it operates on. The mirrored
//! payload is a bare `defaults/` tree (no `.git`, no `loom-daemon/Cargo.toml`),
//! and `loom-daemon-update.sh` resolves its `REPO_ROOT` by walking up from
//! `$PWD`, so running it *inside* the mirror would fail its own
//! "Not in a Loom workspace" gate. The mirror candidate therefore runs the
//! mirror's fresh script with the same working directory the earlier
//! candidates use ([`ScriptCandidate::cwd`]): fresh code, unchanged target.

use std::path::{Path, PathBuf};

use super::ArtifactResolution;

/// Where a candidate's copy of `loom-daemon-update.sh` came from. Carried
/// purely so the exhausted-fallback diagnostic can say which copy said what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScriptOrigin {
    /// The checkout this binary was built from (`CARGO_MANIFEST_DIR`).
    SourceCheckout,
    /// This daemon's own workspace root — the `LOOM_WORKSPACE` fallback.
    WorkspaceRoot,
    /// The per-machine mirrored `defaults/` payload (#7964).
    MachineMirror,
}

impl ScriptOrigin {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::SourceCheckout => "build-time source checkout",
            Self::WorkspaceRoot => "daemon workspace root",
            Self::MachineMirror => "machine-level mirror",
        }
    }
}

/// One candidate invocation of `loom-daemon-update.sh`: which copy to run and
/// which directory to run it in. The two are deliberately separable — see the
/// module docs on why the mirror candidate keeps the earlier candidates' cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScriptCandidate {
    pub(super) origin: ScriptOrigin,
    pub(super) script: PathBuf,
    pub(super) cwd: PathBuf,
}

/// The outcome of asking one candidate for `--resolve-json`.
#[derive(Debug)]
pub(super) enum CandidateOutcome {
    /// The script spoke the protocol — this is the authoritative answer and
    /// the walk stops here. It may itself be
    /// [`ArtifactResolution::Unresolved`]: "no release published yet" is a
    /// real answer (`{"ok":false,"reason":…}`), not a broken script, and must
    /// NOT send the walk on to another copy that would only say the same
    /// thing after another `gh` round-trip.
    Answered(ArtifactResolution),
    /// No usable JSON came back — a stale/incompatible copy that doesn't
    /// understand the flag, a spawn failure, or a timeout. Try the next
    /// candidate; the reason is retained for the exhausted-fallback log.
    Unusable(String),
}

/// The script inside the mirrored `defaults/` payload. The mirror *is* a
/// `defaults/` tree, so the script lives at `scripts/cli/` — one level
/// shallower than either layout [`super::ScriptAutoUpdateProbe::resolve_script`]
/// knows. `resolve_script` is still tried afterwards so a
/// `LOOM_DAEMON_DEFAULTS_DIR` pointed at a whole checkout also resolves.
fn mirror_script(
    mirror: &Path,
    resolve_script: &impl Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let payload = mirror.join("scripts/cli/loom-daemon-update.sh");
    if payload.exists() {
        return Some(payload);
    }
    resolve_script(mirror)
}

/// Build the ordered candidate list for the artifact path: source checkout,
/// then this daemon's workspace root, then the machine-level mirror.
///
/// A root with no script at all contributes no candidate (there is nothing to
/// run), and duplicate `(script, cwd)` pairs are collapsed — a self-hosted
/// Loom checkout has `source_root == fallback_root`, and running the identical
/// command twice would only double the `gh` round-trips.
pub(super) fn candidates(
    source_root: Option<&Path>,
    fallback_root: &Path,
    mirror_root: Option<&Path>,
    resolve_script: impl Fn(&Path) -> Option<PathBuf>,
) -> Vec<ScriptCandidate> {
    let mut out: Vec<ScriptCandidate> = Vec::new();
    let mut push = |origin, script, cwd: &Path| {
        let candidate = ScriptCandidate {
            origin,
            script,
            cwd: cwd.to_path_buf(),
        };
        if !out
            .iter()
            .any(|c| c.script == candidate.script && c.cwd == candidate.cwd)
        {
            out.push(candidate);
        }
    };
    if let Some(root) = source_root {
        if let Some(script) = resolve_script(root) {
            push(ScriptOrigin::SourceCheckout, script, root);
        }
    }
    if let Some(script) = resolve_script(fallback_root) {
        push(ScriptOrigin::WorkspaceRoot, script, fallback_root);
    }
    if let Some(mirror) = mirror_root {
        if let Some(script) = mirror_script(mirror, &resolve_script) {
            // The mirror supplies the SCRIPT; the cwd stays a real checkout
            // (the source checkout when there is one, else the workspace root)
            // because that is what the script resolves its REPO_ROOT from.
            push(ScriptOrigin::MachineMirror, script, source_root.unwrap_or(fallback_root));
        }
    }
    out
}

/// Ask each candidate in turn and return the first authoritative answer,
/// together with the candidate that gave it (so a subsequent `--fetch` runs
/// the same copy that resolved, not one already known to be unusable).
///
/// When every candidate is unusable the returned reason names each script that
/// was tried, its origin, and what it said — #7818's diagnostics applied to
/// the whole chain rather than only to the first link.
pub(super) fn first_answering(
    candidates: &[ScriptCandidate],
    mut run: impl FnMut(&ScriptCandidate) -> CandidateOutcome,
) -> (ArtifactResolution, Option<ScriptCandidate>) {
    let mut tried: Vec<String> = Vec::new();
    for candidate in candidates {
        match run(candidate) {
            CandidateOutcome::Answered(resolution) => return (resolution, Some(candidate.clone())),
            CandidateOutcome::Unusable(reason) => tried.push(format!(
                "{} [{}]: {reason}",
                candidate.script.display(),
                candidate.origin.label()
            )),
        }
    }
    (ArtifactResolution::Unresolved(exhausted_reason(&tried)), None)
}

/// The "nothing answered" reason: either "no copy exists anywhere" or the full
/// per-candidate failure list.
fn exhausted_reason(tried: &[String]) -> String {
    if tried.is_empty() {
        return "no loom-daemon-update.sh could be resolved (neither the build-time source \
                checkout, this daemon's workspace root, nor the machine-level mirror)"
            .to_string();
    }
    format!(
        "no candidate loom-daemon-update.sh produced usable `--resolve-json` output — tried {}: {}",
        tried.len(),
        tried.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::{AutoUpdateProbe, ScriptAutoUpdateProbe};
    use super::*;
    use crate::event_bus::EventBus;
    use crate::workspace_pool::WorkspacePool;

    /// A `resolve_script` stub: a root "has" the script iff it is in the list.
    /// Keeps every test below off the real filesystem — in particular off any
    /// actual `~/.local/share/loom-daemon/defaults` the test host may have.
    fn stub_roots(roots: &'static [&'static str]) -> impl Fn(&Path) -> Option<PathBuf> {
        move |root: &Path| {
            roots
                .iter()
                .any(|r| Path::new(r) == root)
                .then(|| root.join(".loom/scripts/cli/loom-daemon-update.sh"))
        }
    }

    fn candidate(origin: ScriptOrigin, script: &str, cwd: &str) -> ScriptCandidate {
        ScriptCandidate {
            origin,
            script: PathBuf::from(script),
            cwd: PathBuf::from(cwd),
        }
    }

    fn resolved() -> ArtifactResolution {
        ArtifactResolution::Resolved(super::super::ArtifactInfo {
            version: "0.19.55".to_string(),
            tag: "v0.19.55".to_string(),
            ..Default::default()
        })
    }

    // ---- candidate ordering ------------------------------------------------

    #[test]
    fn test_candidates_are_ordered_source_workspace_mirror() {
        let found = candidates(
            Some(Path::new("/src")),
            Path::new("/ws"),
            Some(Path::new("/mirror")),
            stub_roots(&["/src", "/ws", "/mirror"]),
        );
        let origins: Vec<_> = found.iter().map(|c| c.origin).collect();
        assert_eq!(
            origins,
            vec![
                ScriptOrigin::SourceCheckout,
                ScriptOrigin::WorkspaceRoot,
                ScriptOrigin::MachineMirror
            ]
        );
    }

    #[test]
    fn test_mirror_candidate_keeps_a_real_checkout_as_its_cwd() {
        // The mirror supplies the script; the cwd must stay the source
        // checkout, since that is what the script derives REPO_ROOT from.
        let found = candidates(
            Some(Path::new("/src")),
            Path::new("/ws"),
            Some(Path::new("/mirror")),
            stub_roots(&["/src", "/ws", "/mirror"]),
        );
        let mirror = found
            .iter()
            .find(|c| c.origin == ScriptOrigin::MachineMirror)
            .unwrap();
        assert_eq!(mirror.cwd, PathBuf::from("/src"));
        assert!(mirror.script.starts_with("/mirror"), "script: {}", mirror.script.display());

        // …and the workspace root when there is no source checkout at all —
        // the shape of the #7818 host.
        let found = candidates(
            None,
            Path::new("/ws"),
            Some(Path::new("/mirror")),
            stub_roots(&["/ws", "/mirror"]),
        );
        let mirror = found
            .iter()
            .find(|c| c.origin == ScriptOrigin::MachineMirror)
            .unwrap();
        assert_eq!(mirror.cwd, PathBuf::from("/ws"));
    }

    #[test]
    fn test_mirror_candidate_survives_a_workspace_with_no_script() {
        // A workspace root with no copy at all contributes no candidate, but
        // must not suppress the mirror.
        let found = candidates(
            None,
            Path::new("/ws"),
            Some(Path::new("/mirror")),
            stub_roots(&["/mirror"]),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].origin, ScriptOrigin::MachineMirror);
        assert_eq!(found[0].cwd, PathBuf::from("/ws"));
    }

    #[test]
    fn test_no_mirror_root_means_no_mirror_candidate() {
        // `LOOM_DAEMON_DEFAULTS_DIR=""` (strategy disabled) reaches us as
        // `None` — the pre-#7964 two-candidate behaviour, unchanged.
        let found = candidates(
            Some(Path::new("/src")),
            Path::new("/ws"),
            None,
            stub_roots(&["/src", "/ws"]),
        );
        assert_eq!(found.len(), 2);
        assert!(found
            .iter()
            .all(|c| c.origin != ScriptOrigin::MachineMirror));
    }

    #[test]
    fn test_self_hosted_checkout_is_not_probed_twice() {
        // source_root == fallback_root: one candidate, not two identical ones.
        let found =
            candidates(Some(Path::new("/src")), Path::new("/src"), None, stub_roots(&["/src"]));
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn test_no_candidates_when_nothing_has_a_script() {
        assert!(candidates(
            Some(Path::new("/src")),
            Path::new("/ws"),
            Some(Path::new("/mirror")),
            stub_roots(&[])
        )
        .is_empty());
    }

    // ---- the walk ----------------------------------------------------------

    #[test]
    fn test_first_answering_stops_at_the_first_answer() {
        let all = vec![
            candidate(ScriptOrigin::SourceCheckout, "/src/u.sh", "/src"),
            candidate(ScriptOrigin::MachineMirror, "/mirror/u.sh", "/src"),
        ];
        let mut seen: Vec<PathBuf> = Vec::new();
        let (resolution, winner) = first_answering(&all, |c| {
            seen.push(c.script.clone());
            CandidateOutcome::Answered(resolved())
        });
        assert!(matches!(resolution, ArtifactResolution::Resolved(_)));
        assert_eq!(seen, vec![PathBuf::from("/src/u.sh")], "the mirror must not be run needlessly");
        assert_eq!(winner.unwrap().origin, ScriptOrigin::SourceCheckout);
    }

    #[test]
    fn test_first_answering_falls_through_to_the_mirror() {
        // The #7964 case: the workspace's copy doesn't understand the flag,
        // the mirrored copy does.
        let all = vec![
            candidate(ScriptOrigin::WorkspaceRoot, "/ws/u.sh", "/ws"),
            candidate(ScriptOrigin::MachineMirror, "/mirror/u.sh", "/ws"),
        ];
        let (resolution, winner) = first_answering(&all, |c| {
            if c.origin == ScriptOrigin::MachineMirror {
                CandidateOutcome::Answered(resolved())
            } else {
                CandidateOutcome::Unusable("printed no JSON object".to_string())
            }
        });
        match resolution {
            ArtifactResolution::Resolved(info) => assert_eq!(info.version, "0.19.55"),
            other => panic!("expected the mirror's answer, got {other:?}"),
        }
        let winner = winner.expect("the mirror candidate is the winner");
        assert_eq!(winner.origin, ScriptOrigin::MachineMirror);
        // The winner carries the cwd, so a follow-up --fetch runs the fresh
        // script against the same checkout.
        assert_eq!(winner.cwd, PathBuf::from("/ws"));
    }

    #[test]
    fn test_first_answering_does_not_second_guess_an_ok_false_answer() {
        // "no release published yet" is an ANSWER. Walking on to the mirror
        // would buy nothing and cost another `gh` round-trip every tick.
        let all = vec![
            candidate(ScriptOrigin::WorkspaceRoot, "/ws/u.sh", "/ws"),
            candidate(ScriptOrigin::MachineMirror, "/mirror/u.sh", "/ws"),
        ];
        let mut runs = 0;
        let (resolution, winner) = first_answering(&all, |_| {
            runs += 1;
            CandidateOutcome::Answered(ArtifactResolution::Unresolved(
                "'gh release view' found no latest release".to_string(),
            ))
        });
        assert_eq!(runs, 1);
        assert_eq!(winner.unwrap().origin, ScriptOrigin::WorkspaceRoot);
        match resolution {
            ArtifactResolution::Unresolved(reason) => {
                assert!(reason.contains("no latest release"), "reason: {reason}");
            }
            other => panic!("expected the script's own verdict, got {other:?}"),
        }
    }

    #[test]
    fn test_exhausted_reason_names_every_script_that_was_tried() {
        let all = vec![
            candidate(ScriptOrigin::WorkspaceRoot, "/ws/u.sh", "/ws"),
            candidate(ScriptOrigin::MachineMirror, "/mirror/u.sh", "/ws"),
        ];
        let (resolution, winner) = first_answering(&all, |c| {
            CandidateOutcome::Unusable(format!("{} said nothing useful", c.origin.label()))
        });
        assert!(winner.is_none());
        match resolution {
            ArtifactResolution::Unresolved(reason) => {
                assert!(reason.contains("/ws/u.sh"), "reason: {reason}");
                assert!(reason.contains("/mirror/u.sh"), "reason: {reason}");
                assert!(reason.contains("daemon workspace root"), "reason: {reason}");
                assert!(reason.contains("machine-level mirror"), "reason: {reason}");
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
    }

    #[test]
    fn test_no_candidates_at_all_is_unresolved_not_a_panic() {
        let (resolution, winner) = first_answering(&[], |_| {
            panic!("nothing to run");
        });
        assert!(winner.is_none());
        match resolution {
            ArtifactResolution::Unresolved(reason) => {
                assert!(reason.contains("machine-level mirror"), "reason: {reason}");
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
    }

    // ---- mirror script layout ---------------------------------------------

    #[test]
    fn test_mirror_script_prefers_the_defaults_payload_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path();
        std::fs::create_dir_all(mirror.join("scripts/cli")).unwrap();
        std::fs::write(mirror.join("scripts/cli/loom-daemon-update.sh"), "#!/bin/sh\n").unwrap();
        let resolved = mirror_script(mirror, &|_: &Path| None).unwrap();
        assert_eq!(resolved, mirror.join("scripts/cli/loom-daemon-update.sh"));
    }

    #[test]
    fn test_mirror_script_falls_back_to_the_checkout_layouts() {
        // LOOM_DAEMON_DEFAULTS_DIR pointed at a whole checkout rather than a
        // bare payload: the ordinary `.loom/` / `defaults/` layouts still win.
        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path().to_path_buf();
        let resolved =
            mirror_script(&mirror, &|root: &Path| Some(root.join("defaults/scripts/cli/x.sh")));
        assert_eq!(resolved, Some(mirror.join("defaults/scripts/cli/x.sh")));
    }

    // ---- the production probe, end to end (moved here from tests.rs by
    // #7964: that file is at its file-size ratchet baseline) --------------

    /// A probe with NO mirror candidate (#7964) — every test here pins
    /// `mirror_root` explicitly so none of them depends on whether the host
    /// running `cargo test` happens to have a real
    /// `~/.local/share/loom-daemon/defaults`.
    async fn probe_at(root: &Path, mirror_root: Option<PathBuf>) -> ScriptAutoUpdateProbe {
        let bus = Arc::new(EventBus::new());
        let pool = Arc::new(WorkspacePool::new(bus, tokio::runtime::Handle::current()));
        let mut probe = ScriptAutoUpdateProbe::new(pool, root.to_path_buf());
        probe.source_root = None;
        probe.mirror_root = mirror_root;
        probe
    }

    fn write_installed_script(root: &Path) {
        std::fs::create_dir_all(root.join(".loom/scripts/cli")).unwrap();
        std::fs::write(root.join(".loom/scripts/cli/loom-daemon-update.sh"), "#!/bin/sh\n")
            .unwrap();
    }

    #[tokio::test]
    async fn test_script_candidates_fall_back_to_the_workspace_root() {
        // The host shape #7609 exists for: no build-time source checkout
        // resolvable, but the daemon's own workspace root has the script.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_installed_script(&root);
        let probe = probe_at(&root, None).await;
        let candidates = probe.artifact_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].cwd, root);
        assert_eq!(candidates[0].script, root.join(".loom/scripts/cli/loom-daemon-update.sh"));
    }

    #[tokio::test]
    async fn test_script_candidates_are_empty_without_any_script() {
        let tmp = tempfile::tempdir().unwrap();
        let probe = probe_at(tmp.path(), None).await;
        assert!(probe.artifact_candidates().is_empty());
        // …and a probe with no script resolves no artifact rather than erroring.
        assert!(matches!(probe.resolve_artifact(), ArtifactResolution::Unresolved(_)));
    }

    #[tokio::test]
    async fn test_mirrored_copy_is_the_last_script_candidate() {
        // #7964: the workspace's own (stale) copy is still tried first, but the
        // mirrored payload under ~/.local/share/loom-daemon/defaults is now a
        // candidate behind it instead of the walk stopping there. The "mirror" is
        // a temp dir, never the host's real one.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        write_installed_script(&root);
        let mirror = tmp.path().join("mirror");
        std::fs::create_dir_all(mirror.join("scripts/cli")).unwrap();
        std::fs::write(mirror.join("scripts/cli/loom-daemon-update.sh"), "#!/bin/sh\n").unwrap();

        let probe = probe_at(&root, Some(mirror.clone())).await;
        let candidates = probe.artifact_candidates();
        assert_eq!(candidates.len(), 2, "workspace copy then mirrored copy: {candidates:?}");
        assert_eq!(candidates[1].script, mirror.join("scripts/cli/loom-daemon-update.sh"));
        // The mirror supplies the script only — the cwd stays the workspace, which
        // is what the script resolves its REPO_ROOT from.
        assert_eq!(candidates[1].cwd, root);
    }

    #[tokio::test]
    async fn test_mirrored_copy_is_a_candidate_even_with_no_workspace_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let mirror = tmp.path().join("mirror");
        std::fs::create_dir_all(mirror.join("scripts/cli")).unwrap();
        std::fs::write(mirror.join("scripts/cli/loom-daemon-update.sh"), "#!/bin/sh\n").unwrap();

        let probe = probe_at(&root, Some(mirror.clone())).await;
        let candidates = probe.artifact_candidates();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].script, mirror.join("scripts/cli/loom-daemon-update.sh"));
        assert_eq!(candidates[0].cwd, root);
    }

    #[tokio::test]
    async fn test_a_stale_workspace_copy_falls_back_to_the_mirrored_copy() {
        // End-to-end over the real subprocess machinery, with two stub scripts:
        // the workspace's copy predates --resolve-json (prints nothing on stdout,
        // complains on stderr, exits 2); the mirrored copy understands it. Before
        // #7964 this host resolved no artifact at all and silently degraded to the
        // source path forever.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(root.join(".loom/scripts/cli")).unwrap();
        let stale = root.join(".loom/scripts/cli/loom-daemon-update.sh");
        std::fs::write(
            &stale,
            "#!/bin/sh\necho 'loom-daemon-update.sh: unknown option --resolve-json' >&2\nexit 2\n",
        )
        .unwrap();
        let mirror = tmp.path().join("mirror");
        std::fs::create_dir_all(mirror.join("scripts/cli")).unwrap();
        let fresh = mirror.join("scripts/cli/loom-daemon-update.sh");
        std::fs::write(
            &fresh,
            "#!/bin/sh\nprintf '{\"ok\":true,\"tag\":\"v0.19.55\",\"version\":\"0.19.55\"}\\n'\n",
        )
        .unwrap();
        for script in [&stale, &fresh] {
            let mut perms = std::fs::metadata(script).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(script, perms).unwrap();
        }

        let probe = probe_at(&root, Some(mirror)).await;
        match probe.resolve_artifact() {
            ArtifactResolution::Resolved(info) => assert_eq!(info.version, "0.19.55"),
            other => panic!("expected the mirrored copy's answer, got {other:?}"),
        }
        // …and the follow-up --fetch runs the copy that answered, not the stale one.
        assert_eq!(probe.fetch_candidate().unwrap().script, fresh);
    }

    #[tokio::test]
    async fn test_every_tried_script_is_named_when_all_candidates_are_unusable() {
        // The exhausted-fallback diagnostic (#7818's naming, applied to the whole
        // chain): both copies are unusable, and the reason must name both.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(root.join(".loom/scripts/cli")).unwrap();
        let stale = root.join(".loom/scripts/cli/loom-daemon-update.sh");
        std::fs::write(&stale, "#!/bin/sh\necho 'unknown option' >&2\nexit 2\n").unwrap();
        let mirror = tmp.path().join("mirror");
        std::fs::create_dir_all(mirror.join("scripts/cli")).unwrap();
        let also_stale = mirror.join("scripts/cli/loom-daemon-update.sh");
        std::fs::write(&also_stale, "#!/bin/sh\necho 'also unknown' >&2\nexit 2\n").unwrap();
        for script in [&stale, &also_stale] {
            let mut perms = std::fs::metadata(script).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(script, perms).unwrap();
        }

        let probe = probe_at(&root, Some(mirror)).await;
        match probe.resolve_artifact() {
            ArtifactResolution::Unresolved(reason) => {
                assert!(reason.contains(&stale.display().to_string()), "reason: {reason}");
                assert!(reason.contains(&also_stale.display().to_string()), "reason: {reason}");
                assert!(reason.contains("unknown option"), "reason: {reason}");
                assert!(reason.contains("also unknown"), "reason: {reason}");
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
    }
}
