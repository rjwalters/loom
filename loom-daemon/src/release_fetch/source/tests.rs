//! Tests for the source-revision / tag-movement gate (#10473).
//!
//! Two layers: pure unit tests drive [`gate`] with an in-memory API closure,
//! and end-to-end tests drive the real `fetch_and_verify_with_policy()`
//! against a FAKE `gh` (release download/view + `api` answered from fixture
//! files) and a fake `cosign`. No network, no live release. `PATH` and
//! `LOOM_GH_BIN` are process-global, so the end-to-end tests are `#[serial]`.

use super::*;
use crate::release_fetch::fetch::{fetch_and_verify_with_policy, FetchInputs, FetchOutcome};
use crate::release_fetch::SignaturePolicy;
use serial_test::serial;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write as _;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
const T: &str = "dddddddddddddddddddddddddddddddddddddddd";
const SLUG: &str = "test-owner/test-repo";
const TARGET: &str = "x86_64-unknown-linux-gnu";

fn ref_body(kind: &str, sha: &str) -> String {
    format!(r#"{{"ref":"refs/tags/x","object":{{"type":"{kind}","sha":"{sha}"}}}}"#)
}

fn compare_body(status: &str) -> String {
    format!(r#"{{"status":"{status}","ahead_by":1,"behind_by":0}}"#)
}

/// An in-memory API: path -> body; anything unlisted fails like a 404.
fn fake_api(map: HashMap<String, String>) -> impl Fn(&str) -> Result<String, String> {
    move |p: &str| {
        map.get(p)
            .cloned()
            .ok_or_else(|| "`gh api` exited 1".to_string())
    }
}

fn tag_map(tag: &str, commit: &str) -> HashMap<String, String> {
    HashMap::from([(format!("repos/{SLUG}/git/ref/tags/{tag}"), ref_body("commit", commit))])
}

fn inputs<'a>(anchor: &'a AnchorSetting, record: Option<&'a Path>, sha: &'a str) -> GateInputs<'a> {
    GateInputs {
        slug: SLUG,
        tag: "v1.0.0",
        target: TARGET,
        asset_sha256: sha,
        anchor,
        record_path: record,
    }
}

fn tempdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "loom-source-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[test]
fn source_anchor_rejects_short_or_non_hex() {
    assert_eq!(parse_anchor(None), AnchorSetting::NotConfigured);
    assert_eq!(parse_anchor(Some("  ")), AnchorSetting::NotConfigured);
    assert!(matches!(parse_anchor(Some("abc1234")), AnchorSetting::Invalid(_)));
    let non_hex = "g".repeat(40);
    match parse_anchor(Some(&non_hex)) {
        AnchorSetting::Invalid(why) => {
            assert!(why.contains("non-hex"), "{why}");
            assert!(!why.contains(&non_hex), "must not echo the value: {why}");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    assert!(matches!(parse_anchor(Some(&"a".repeat(41))), AnchorSetting::Invalid(_)));
    assert_eq!(
        parse_anchor(Some(&format!(" {} ", A.to_ascii_uppercase()))),
        AnchorSetting::Anchor(A.to_string())
    );

    // An invalid anchor is a config error in the gate, never a skip.
    let bad = parse_anchor(Some("abc1234"));
    let err = gate(&fake_api(HashMap::new()), &inputs(&bad, None, "x")).unwrap_err();
    assert_eq!(err.class, RefusalClass::ConfigError);
    assert!(err.lines[0].contains("configuration error"), "{:?}", err.lines);
}

#[test]
fn source_identical_and_ahead_accepted() {
    let anchor = AnchorSetting::Anchor(A.to_string());
    for (commit, status) in [(A, "identical"), (B, "ahead")] {
        let mut map = tag_map("v1.0.0", commit);
        map.insert(format!("repos/{SLUG}/compare/{A}...{commit}"), compare_body(status));
        let r = gate(&fake_api(map), &inputs(&anchor, None, "x")).unwrap();
        assert_eq!(r.source_check, status);
        assert_eq!(r.source_commit.as_deref(), Some(commit));
        assert_eq!(r.source_anchor.as_deref(), Some(A));
        assert_eq!(r.adoption, "not_recorded");
    }
}

#[test]
fn source_behind_and_diverged_refused() {
    let anchor = AnchorSetting::Anchor(A.to_string());
    for status in ["behind", "diverged"] {
        let mut map = tag_map("v1.0.0", B);
        map.insert(format!("repos/{SLUG}/compare/{A}...{B}"), compare_body(status));
        let err = gate(&fake_api(map), &inputs(&anchor, None, "x")).unwrap_err();
        assert_eq!(err.class, RefusalClass::NotDescendant);
        assert!(err.lines[0].contains("not a descendant of approved source"), "{:?}", err.lines);
        assert!(err.lines[0].contains(status), "{:?}", err.lines);
    }
}

#[test]
fn source_api_failure_is_unavailable_not_mismatch() {
    let anchor = AnchorSetting::Anchor(A.to_string());
    let assert_unavailable = |err: Refusal| {
        assert_eq!(err.class, RefusalClass::Unavailable);
        let all = err.lines.join("\n");
        assert!(all.contains("source assurance unavailable"), "{all}");
        assert!(all.contains("NOT evidence of tampering"), "{all}");
        assert!(!all.contains("FAILED") && !all.contains("detected"), "{all}");
    };
    // Tag ref lookup fails.
    assert_unavailable(gate(&fake_api(HashMap::new()), &inputs(&anchor, None, "x")).unwrap_err());
    // Tag ref unparsable.
    let map = HashMap::from([(format!("repos/{SLUG}/git/ref/tags/v1.0.0"), "<html>".to_string())]);
    assert_unavailable(gate(&fake_api(map), &inputs(&anchor, None, "x")).unwrap_err());
    // Compare call fails.
    assert_unavailable(
        gate(&fake_api(tag_map("v1.0.0", B)), &inputs(&anchor, None, "x")).unwrap_err(),
    );
    // Compare status unknown.
    let mut map = tag_map("v1.0.0", B);
    map.insert(format!("repos/{SLUG}/compare/{A}...{B}"), compare_body("weird"));
    assert_unavailable(gate(&fake_api(map), &inputs(&anchor, None, "x")).unwrap_err());
    // A corrupt adoption record is unavailable too, never "first seen".
    let dir = tempdir();
    let rec = dir.join("adoption.json");
    std::fs::write(&rec, "{not json").unwrap();
    let none = AnchorSetting::NotConfigured;
    assert_unavailable(
        gate(&fake_api(HashMap::new()), &inputs(&none, Some(&rec), "x")).unwrap_err(),
    );
}

#[test]
fn annotated_tag_is_peeled_to_commit() {
    let calls = RefCell::new(Vec::new());
    let map = HashMap::from([
        (format!("repos/{SLUG}/git/ref/tags/v1.0.0"), ref_body("tag", T)),
        (format!("repos/{SLUG}/git/tags/{T}"), ref_body("commit", B)),
    ]);
    let api = |p: &str| {
        calls.borrow_mut().push(p.to_string());
        map.get(p).cloned().ok_or_else(|| "404".to_string())
    };
    assert_eq!(resolve_tag_commit(&api, SLUG, "v1.0.0").unwrap(), B);
    assert_eq!(calls.borrow().len(), 2);
    // A tag pointing at a tree is not a commit: unavailable, not accepted.
    let tree = HashMap::from([(format!("repos/{SLUG}/git/ref/tags/v1.0.0"), ref_body("tree", B))]);
    assert!(resolve_tag_commit(&fake_api(tree), SLUG, "v1.0.0").is_err());
    // A path-unsafe tag is never spliced into an API path.
    assert!(resolve_tag_commit(&fake_api(HashMap::new()), SLUG, "../x").is_err());
}

#[test]
fn source_adoption_record_detects_movement_and_replacement() {
    let dir = tempdir();
    let rec = dir.join("sub").join("adoption.json");
    let anchor = AnchorSetting::Anchor(A.to_string());
    let with = |commit: &str| {
        let mut m = tag_map("v1.0.0", commit);
        m.insert(format!("repos/{SLUG}/compare/{A}...{commit}"), compare_body("ahead"));
        fake_api(m)
    };
    let mut r = gate(&with(B), &inputs(&anchor, Some(&rec), "sha1")).unwrap();
    assert_eq!(r.adoption, "first_seen");
    assert!(record_adoption(&mut r, &inputs(&anchor, Some(&rec), "sha1")).is_none());
    let r = gate(&with(B), &inputs(&anchor, Some(&rec), "sha1")).unwrap();
    assert_eq!(r.adoption, "matched");
    let e = gate(&with(C), &inputs(&anchor, Some(&rec), "sha1")).unwrap_err();
    assert_eq!(e.class, RefusalClass::TagMoved);
    let e = gate(&with(B), &inputs(&anchor, Some(&rec), "sha2")).unwrap_err();
    assert_eq!(e.class, RefusalClass::AssetReplaced);
    // Without an anchor the commit is unknown, so only the digest is compared,
    // and a later unanchored write keeps the commit already on record.
    let none = AnchorSetting::NotConfigured;
    let mut r = gate(&with(C), &inputs(&none, Some(&rec), "sha1")).unwrap();
    assert_eq!(r.source_check, "not_configured");
    assert!(record_adoption(&mut r, &inputs(&none, Some(&rec), "sha1")).is_none());
    let rec_json = read_record(&rec).unwrap();
    let entry = rec_json.entries.values().next().unwrap();
    assert_eq!(entry.commit.as_deref(), Some(B));
}

// ---------------------------------------------------------------------------
// End to end: real fetch, fake `gh` + `cosign`
// ---------------------------------------------------------------------------

fn write_script(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).unwrap();
    writeln!(f, "#!/usr/bin/env bash\n{body}").unwrap();
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The fixture-file name the fake `gh` reads for `gh api <path>`.
fn api_key(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const BIN: &str = "loom-daemon-x86_64-unknown-linux-gnu";

struct Fx {
    root: PathBuf,
    assets: PathBuf,
    api: PathBuf,
    fakebin: PathBuf,
    record: PathBuf,
    marker: PathBuf,
}

impl Fx {
    fn new() -> Self {
        let root = tempdir();
        let (assets, api, fakebin) = (root.join("assets"), root.join("api"), root.join("bin"));
        for d in [&assets, &api, &fakebin] {
            std::fs::create_dir_all(d).unwrap();
        }
        write_script(
            &fakebin,
            "gh",
            &format!(
                r#"ASSETS="{assets}"; API="{api}"
if [[ "$1" == api ]]; then
    key=$(printf '%s' "$2" | tr -c 'A-Za-z0-9' '_')
    [[ -f "$API/FAIL" ]] && {{ echo "gh: HTTP 502" >&2; exit 1; }}
    [[ -f "$API/$key" ]] || {{ echo "gh: HTTP 404" >&2; exit 1; }}
    cat "$API/$key"; exit 0
fi
if [[ "$1" == release && "$2" == view ]]; then
    out='{{"assets":['; first=1
    for n in $(ls "$ASSETS"); do [[ $first -eq 1 ]] || out+=','; out+="{{\"name\":\"$n\"}}"; first=0; done
    printf '%s]}}\n' "$out"; exit 0
fi
if [[ "$1" == release && "$2" == download ]]; then
    shift 3; dest=.; pats=()
    while [[ $# -gt 0 ]]; do case "$1" in -p) pats+=("$2"); shift 2;; -D) dest="$2"; shift 2;; -R) shift 2;; *) shift;; esac; done
    mkdir -p "$dest"; copied=0
    for p in "${{pats[@]}}"; do for f in "$ASSETS"/$p; do [[ -e "$f" ]] && cp "$f" "$dest/" && copied=1; done; done
    [[ $copied -eq 1 ]] && exit 0 || exit 1
fi
exit 1
"#,
                assets = assets.display(),
                api = api.display(),
            ),
        );
        // Keyless verification always passes: the source gate is under test.
        write_script(&fakebin, "cosign", "exit 0\n");
        let fx = Self {
            record: root.join("state").join("release-adoption.json"),
            marker: root.join("candidate-ran"),
            root,
            assets,
            api,
            fakebin,
        };
        fx.set_asset("v1");
        fx
    }

    /// Publish a signed candidate that drops `marker` if it is ever executed.
    fn set_asset(&self, flavour: &str) {
        let body = format!(
            "#!/bin/sh\ntouch {}\necho 'loom-daemon 0.0.1 ({flavour})'\n",
            self.marker.display()
        );
        std::fs::write(self.assets.join(BIN), body.as_bytes()).unwrap();
        std::fs::write(
            self.assets.join(format!("{BIN}.sha256")),
            format!("{}  {BIN}\n", sha256_hex(body.as_bytes())),
        )
        .unwrap();
        std::fs::write(self.assets.join(format!("{BIN}.sig")), b"sig").unwrap();
        std::fs::write(self.assets.join(format!("{BIN}.pem")), b"cert").unwrap();
    }

    fn api_fixture(&self, path: &str, body: &str) {
        std::fs::write(self.api.join(api_key(path)), body).unwrap();
    }

    /// Point `tag` at `commit`, with `status` as its compare answer vs `A`.
    fn set_tag(&self, tag: &str, commit: &str, status: &str) {
        self.api_fixture(&format!("repos/{SLUG}/git/ref/tags/{tag}"), &ref_body("commit", commit));
        self.api_fixture(&format!("repos/{SLUG}/compare/{A}...{commit}"), &compare_body(status));
    }

    fn policy(&self, anchor: Option<&str>) -> SignaturePolicy {
        SignaturePolicy {
            require_signature: true,
            approved_source_anchor: parse_anchor(anchor),
            adoption_record: Some(self.record.clone()),
            ..SignaturePolicy::default()
        }
    }

    fn fetch(&self, tag: &str, policy: &SignaturePolicy) -> FetchOutcome {
        let inputs = FetchInputs {
            repo_root: &self.root,
            target: TARGET,
            repo_slug: SLUG,
            tag,
            cosign_pubkey_env: None,
            cosign_identity_env: None,
            cosign_oidc_issuer_env: None,
        };
        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old}", self.fakebin.display()));
        let gh =
            crate::gh_invocation::resolver::test_stub::GhBinGuard::set(&self.fakebin.join("gh"));
        let out = fetch_and_verify_with_policy(&inputs, policy);
        drop(gh);
        std::env::set_var("PATH", old);
        out
    }

    /// Must verify; returns the parsed evidence record. Resets the marker so a
    /// following refusal can prove the candidate was not executed.
    fn verified(&self, tag: &str, policy: &SignaturePolicy) -> serde_json::Value {
        match self.fetch(tag, policy) {
            FetchOutcome::Verified {
                artifact,
                signature_line,
                ..
            } => {
                let _ = std::fs::remove_dir_all(&artifact.tmp_dir);
                let _ = std::fs::remove_file(&self.marker);
                let ev = signature_line
                    .lines()
                    .find_map(|l| l.strip_prefix("LOOM_SIGNATURE_EVIDENCE "))
                    .map(|l| serde_json::from_str(l).unwrap());
                ev.unwrap_or(serde_json::Value::Null)
            }
            FetchOutcome::VerificationFailed { lines } => panic!("expected Verified: {lines:?}"),
            FetchOutcome::DownloadFailed(m) => panic!("expected Verified: {m}"),
        }
    }

    /// Must refuse without executing the candidate; returns the joined lines.
    fn refused(&self, tag: &str, policy: &SignaturePolicy) -> String {
        let _ = std::fs::remove_file(&self.marker);
        let lines = match self.fetch(tag, policy) {
            FetchOutcome::VerificationFailed { lines } => lines,
            FetchOutcome::Verified { artifact, .. } => {
                let _ = std::fs::remove_dir_all(&artifact.tmp_dir);
                panic!("expected a refusal, got Verified")
            }
            FetchOutcome::DownloadFailed(m) => {
                panic!("expected a refusal, got DownloadFailed: {m}")
            }
        };
        assert!(!self.marker.exists(), "candidate was executed before the refusal");
        let all = lines.join("\n");
        assert!(all.contains("left untouched"), "{all}");
        all
    }
}

#[test]
#[serial]
fn required_mode_refuses_tag_moved_to_different_commit() {
    let fx = Fx::new();
    let policy = fx.policy(Some(A));
    fx.set_tag("v1.0.0", B, "ahead");
    let ev = fx.verified("v1.0.0", &policy);
    assert_eq!(ev["source_commit"], B);
    assert!(fx.record.is_file(), "adoption record must be written on success");

    // The same tag now points at a different (still descending) commit.
    fx.set_tag("v1.0.0", C, "ahead");
    let all = fx.refused("v1.0.0", &policy);
    assert!(all.contains("Tag movement detected"), "{all}");
    assert!(all.contains(B) && all.contains(C), "{all}");
    assert!(
        !all.contains("NOT evidence of tampering"),
        "a mismatch is not 'unavailable': {all}"
    );
}

#[test]
#[serial]
fn required_mode_refuses_asset_replacement_same_tag() {
    let fx = Fx::new();
    let policy = fx.policy(Some(A));
    fx.set_tag("v1.0.0", B, "ahead");
    let _ = fx.verified("v1.0.0", &policy);

    // Same tag, same commit, different (self-consistent, signed) asset.
    fx.set_asset("replaced");
    let all = fx.refused("v1.0.0", &policy);
    assert!(all.contains("Asset replacement detected"), "{all}");

    // Digest detection does not need an anchor.
    let all = fx.refused("v1.0.0", &fx.policy(None));
    assert!(all.contains("Asset replacement detected"), "{all}");
}

#[test]
#[serial]
fn required_mode_refuses_tag_not_descending_from_anchor() {
    let fx = Fx::new();
    let policy = fx.policy(Some(A));
    fx.set_tag("v1.0.0", C, "diverged");
    let all = fx.refused("v1.0.0", &policy);
    assert!(all.contains("not a descendant of approved source"), "{all}");
    assert!(!fx.record.exists(), "a refused release must not be recorded as adopted");

    // Forge unavailable: refused, but worded as unavailable, not tampering.
    std::fs::write(fx.api.join("FAIL"), "").unwrap();
    let all = fx.refused("v1.0.0", &policy);
    assert!(all.contains("source assurance unavailable"), "{all}");
    assert!(all.contains("NOT evidence of tampering"), "{all}");
}

#[test]
#[serial]
fn required_mode_accepts_legitimate_release_and_rollback_at_or_after_anchor() {
    let fx = Fx::new();
    let policy = fx.policy(Some(A));
    fx.set_tag("v1.1.0", B, "ahead");
    fx.set_tag("v1.0.0", A, "identical");
    fx.set_tag("v0.9.0", C, "behind");

    assert_eq!(fx.verified("v1.1.0", &policy)["source_check"], "ahead");
    // Rollback to the anchor itself passes...
    assert_eq!(fx.verified("v1.0.0", &policy)["source_check"], "identical");
    // ...and re-adopting the newer release again is unchanged, not "movement".
    assert_eq!(fx.verified("v1.1.0", &policy)["adoption_record"], "matched");
    // Rollback below the anchor is refused.
    let all = fx.refused("v0.9.0", &policy);
    assert!(all.contains("not a descendant of approved source"), "{all}");
    assert!(all.contains("behind"), "{all}");
}

#[test]
#[serial]
fn required_mode_evidence_reports_source_check_fields() {
    let fx = Fx::new();
    fx.set_tag("v1.0.0", B, "ahead");

    // No anchor: observable `not_configured`, and nothing claims a check that
    // did not run (the tag was never resolved, so no commit is reported).
    let ev = fx.verified("v1.0.0", &fx.policy(None));
    assert_eq!(ev["source_check"], "not_configured");
    assert!(ev["source_commit"].is_null(), "{ev}");
    assert!(ev["source_anchor"].is_null(), "{ev}");
    assert_eq!(ev["adoption_record"], "first_seen");

    let ev = fx.verified("v1.0.0", &fx.policy(Some(A)));
    assert_eq!(ev["source_check"], "ahead");
    assert_eq!(ev["source_commit"], B);
    assert_eq!(ev["source_anchor"], A);
    assert_eq!(ev["adoption_record"], "matched");
    assert_eq!(ev["signature_state"], "verified");

    // An invalid anchor is a config error refusal, not a silent skip.
    let all = fx.refused("v1.0.0", &fx.policy(Some("abc1234")));
    assert!(all.contains("configuration error"), "{all}");
}

#[test]
#[serial]
fn default_mode_ignores_source_policy_entirely() {
    let fx = Fx::new();
    std::fs::write(fx.api.join("FAIL"), "").unwrap();
    let policy = SignaturePolicy {
        require_signature: false,
        ..fx.policy(Some(A))
    };
    let ev = fx.verified("v1.0.0", &policy);
    assert!(ev.is_null(), "present-only mode prints no evidence: {ev}");
    assert!(!fx.record.exists(), "present-only mode writes no adoption record");
}
