//! Tests for the source-revision / tag-movement gate (#10473).
//!
//! Two layers: pure unit tests drive [`gate`] with an in-memory API closure,
//! and end-to-end tests drive the real `fetch_and_verify_with_policy()`
//! against a FAKE `gh` (release download/view + `api` answered from fixture
//! files) and a fake `cosign`. No network, no live release. `PATH` and
//! `LOOM_GH_BIN` are process-global, so the end-to-end tests are `#[serial]`.

use super::*;
use crate::release_fetch::fetch::{FetchInputs, FetchOutcome};
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
        assert_eq!(r.source_check, Some(status));
        assert_eq!(r.source_commit.as_deref(), Some(commit));
        assert_eq!(r.source_anchor.as_deref(), Some(A));
        assert_eq!(r.adoption, Some("not_recorded"));
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
    let r = gate(&with(B), &inputs(&anchor, Some(&rec), "sha1")).unwrap();
    assert_eq!(r.adoption, Some("first_seen"));
    assert!(record_adoption(&r, &inputs(&anchor, Some(&rec), "sha1")).is_ok());
    let r = gate(&with(B), &inputs(&anchor, Some(&rec), "sha1")).unwrap();
    assert_eq!(r.adoption, Some("matched"));
    let e = gate(&with(C), &inputs(&anchor, Some(&rec), "sha1")).unwrap_err();
    assert_eq!(e.class, RefusalClass::TagMoved);
    let e = gate(&with(B), &inputs(&anchor, Some(&rec), "sha2")).unwrap_err();
    assert_eq!(e.class, RefusalClass::AssetReplaced);
    // Without an anchor the commit is unknown, so only the digest is compared,
    // and a later unanchored write keeps the commit already on record.
    let none = AnchorSetting::NotConfigured;
    let r = gate(&with(C), &inputs(&none, Some(&rec), "sha1")).unwrap();
    assert_eq!(r.source_check, Some("not_configured"));
    assert!(record_adoption(&r, &inputs(&none, Some(&rec), "sha1")).is_ok());
    let rec_json = read_record(&rec).unwrap();
    let entry = rec_json.entries.values().next().unwrap();
    assert_eq!(entry.commit.as_deref(), Some(B));
}

/// #10659 review: two updates both pass `gate()` on the same ABSENT tag with
/// different commits (or digests). Persistence re-validates under the record
/// lock, so the second is refused as tag movement / asset replacement and the
/// first pin survives -- it is never overwritten.
#[test]
fn source_adoption_second_conflicting_pin_after_gate_is_refused() {
    let dir = tempdir();
    let rec = dir.join("adoption.json");
    let anchor = AnchorSetting::Anchor(A.to_string());
    let with = |commit: &str| {
        let mut m = tag_map("v1.0.0", commit);
        m.insert(format!("repos/{SLUG}/compare/{A}...{commit}"), compare_body("ahead"));
        fake_api(m)
    };
    // Different commit, same tag: both gates see the tag as first_seen.
    let ra = gate(&with(B), &inputs(&anchor, Some(&rec), "sha1")).unwrap();
    let rb = gate(&with(C), &inputs(&anchor, Some(&rec), "sha1")).unwrap();
    assert_eq!((ra.adoption, rb.adoption), (Some("first_seen"), Some("first_seen")));
    record_adoption(&ra, &inputs(&anchor, Some(&rec), "sha1")).unwrap();
    let e = record_adoption(&rb, &inputs(&anchor, Some(&rec), "sha1")).unwrap_err();
    assert_eq!(e.class, RefusalClass::TagMoved);
    assert_eq!(e.partial.adoption, Some("tag_moved"));
    assert_eq!(e.partial.source_commit.as_deref(), Some(C));
    assert_eq!(e.partial.source_check, Some("ahead"));
    assert!(e.lines[0].contains("Tag movement detected"), "{:?}", e.lines);
    assert!(e.lines[0].contains("concurrent update"), "{:?}", e.lines);
    let entry = read_record(&rec)
        .unwrap()
        .entries
        .into_values()
        .next()
        .unwrap();
    assert_eq!((entry.commit.as_deref(), entry.asset_sha256.as_str()), (Some(B), "sha1"));

    // Different digest, same tag (unanchored: only the digest is pinned).
    let rec2 = dir.join("adoption2.json");
    let none = AnchorSetting::NotConfigured;
    let api = fake_api(HashMap::new());
    let r1 = gate(&api, &inputs(&none, Some(&rec2), "sha1")).unwrap();
    let r2 = gate(&api, &inputs(&none, Some(&rec2), "sha2")).unwrap();
    record_adoption(&r1, &inputs(&none, Some(&rec2), "sha1")).unwrap();
    let e = record_adoption(&r2, &inputs(&none, Some(&rec2), "sha2")).unwrap_err();
    assert_eq!(e.class, RefusalClass::AssetReplaced);
    assert_eq!(e.partial.adoption, Some("asset_replaced"));
    assert!(e.lines[0].contains("Asset replacement detected"), "{:?}", e.lines);
    let entries = read_record(&rec2).unwrap().entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries.values().next().unwrap().asset_sha256, "sha1");
    // Re-persisting the SAME pin is still accepted (the `matched` path).
    record_adoption(&r1, &inputs(&none, Some(&rec2), "sha1")).unwrap();
}

/// #10659 review: concurrent adoptions of DIFFERENT tags on one record must
/// not drop each other's entries (lost read-modify-write update).
#[test]
fn source_adoption_concurrent_different_tags_lose_no_entry() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 6;
    let dir = tempdir();
    let rec = dir.join("adoption.json");
    let barrier = std::sync::Barrier::new(THREADS);
    std::thread::scope(|s| {
        for t in 0..THREADS {
            let (rec, barrier) = (&rec, &barrier);
            s.spawn(move || {
                barrier.wait();
                for n in 0..PER_THREAD {
                    let entry = AdoptionEntry {
                        slug: SLUG.to_string(),
                        tag: format!("v{t}.{n}.0"),
                        target: TARGET.to_string(),
                        commit: None,
                        asset_sha256: format!("sha-{t}-{n}"),
                    };
                    write_entry(rec, entry).unwrap();
                }
            });
        }
    });
    let entries = read_record(&rec).unwrap().entries;
    assert_eq!(entries.len(), THREADS * PER_THREAD, "lost entries: {entries:?}");
    for t in 0..THREADS {
        for n in 0..PER_THREAD {
            let e = &entries[&record_key(SLUG, &format!("v{t}.{n}.0"), TARGET)];
            assert_eq!(e.asset_sha256, format!("sha-{t}-{n}"));
        }
    }
}

/// The write is serialized on the record lock: while another holder has it,
/// a writer neither reads nor writes the record; a holder past the bound is a
/// storage failure (`write_failed`), never a silent unlocked write.
#[test]
fn source_adoption_write_waits_for_the_record_lock() {
    let dir = tempdir();
    let rec = dir.join("adoption.json");
    let entry = |tag: &str| AdoptionEntry {
        slug: SLUG.to_string(),
        tag: tag.to_string(),
        target: TARGET.to_string(),
        commit: None,
        asset_sha256: "sha1".to_string(),
    };
    let held = lock_record(&rec, std::time::Duration::ZERO).unwrap();
    // Bounded wait while held: a storage failure, and nothing was written.
    match write_entry_waiting(&rec, entry("v0"), std::time::Duration::from_millis(60)) {
        Err(PersistError::Storage(why)) => assert!(why.contains("timed out"), "{why}"),
        other => panic!("expected a lock-timeout storage failure, got {other:?}"),
    }
    assert!(!rec.exists());
    // A default-wait writer blocks until the holder releases, then succeeds.
    std::thread::scope(|s| {
        let h = s.spawn(|| write_entry(&rec, entry("v1")));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!h.is_finished(), "writer must wait for the lock");
        assert!(!rec.exists(), "nothing may be written while the lock is held");
        drop(held);
        h.join().unwrap().unwrap();
    });
    assert_eq!(read_record(&rec).unwrap().entries.len(), 1);
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
        self.fetch_with_evidence(tag, policy).0
    }

    fn fetch_with_evidence(
        &self,
        tag: &str,
        policy: &SignaturePolicy,
    ) -> (FetchOutcome, serde_json::Value) {
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
        let (out, record) = crate::release_fetch::fetch_and_verify_with_evidence(&inputs, policy);
        drop(gh);
        std::env::set_var("PATH", old);
        (out, serde_json::to_value(&record).unwrap())
    }

    /// Must refuse; returns the durable evidence record (not the refusal text).
    fn refused_record(&self, tag: &str, policy: &SignaturePolicy) -> serde_json::Value {
        let _ = std::fs::remove_file(&self.marker);
        let (out, ev) = self.fetch_with_evidence(tag, policy);
        assert!(matches!(out, FetchOutcome::VerificationFailed { .. }), "expected a refusal");
        assert!(!self.marker.exists(), "candidate was executed before the refusal");
        assert_eq!(ev["outcome"], "source_assurance_refused", "{ev}");
        ev
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
fn required_mode_refuses_when_adoption_record_is_unwritable() {
    let fx = Fx::new();
    // The record is absent (reads as empty) but its temp-file path is a
    // directory, so the write fails deterministically, even when running as root.
    let tmp = fx
        .record
        .with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let policy = fx.policy(Some(A));
    fx.set_tag("v1.0.0", B, "ahead");
    let all = fx.refused("v1.0.0", &policy);
    assert!(all.contains("adoption record"), "{all}");
    assert!(
        all.contains("NOT evidence of tampering"),
        "a storage failure is not a mismatch: {all}"
    );
}

#[test]
#[serial]
fn required_mode_refuses_tag_moved_to_different_commit() {
    let fx = Fx::new();
    let policy = fx.policy(Some(A));
    fx.set_tag("v1.0.0", B, "ahead");
    let ev = fx.verified("v1.0.0", &policy);
    assert_eq!(ev["source_revision"], B);
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
    assert!(ev["source_revision"].is_null(), "{ev}");
    assert!(ev["source_anchor"].is_null(), "{ev}");
    assert_eq!(ev["adoption_record"], "first_seen");

    let ev = fx.verified("v1.0.0", &fx.policy(Some(A)));
    assert_eq!(ev["source_check"], "ahead");
    assert_eq!(ev["source_revision"], B);
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

#[test]
#[serial]
fn refusal_evidence_keeps_the_source_facts_established_before_it() {
    let fx = Fx::new();
    let policy = fx.policy(Some(A));

    // Ancestry refusal: tag resolved and adoption checked, ancestry said no.
    fx.set_tag("v0.9.0", C, "behind");
    let ev = fx.refused_record("v0.9.0", &policy);
    assert_eq!(ev["source_revision"], C, "{ev}");
    assert_eq!(ev["source_anchor"], A, "{ev}");
    assert_eq!(ev["source_check"], "behind", "{ev}");
    assert_eq!(ev["adoption_record"], "first_seen", "{ev}");
    fx.set_tag("v0.8.0", C, "diverged");
    let ev = fx.refused_record("v0.8.0", &policy);
    assert_eq!(ev["source_check"], "diverged", "{ev}");

    // API failure: nothing resolved, so every check that did not run is null.
    std::fs::write(fx.api.join("FAIL"), "").unwrap();
    let ev = fx.refused_record("v1.0.0", &policy);
    assert_eq!(ev["source_anchor"], A, "{ev}");
    assert!(ev["source_revision"].is_null(), "{ev}");
    assert!(ev["source_check"].is_null(), "{ev}");
    assert!(ev["adoption_record"].is_null(), "{ev}");
    std::fs::remove_file(fx.api.join("FAIL")).unwrap();

    // Moved tag / replaced asset against an adopted release.
    fx.set_tag("v1.0.0", B, "ahead");
    let _ = fx.verified("v1.0.0", &policy);
    fx.set_tag("v1.0.0", C, "ahead");
    let ev = fx.refused_record("v1.0.0", &policy);
    assert_eq!(ev["adoption_record"], "tag_moved", "{ev}");
    assert_eq!(ev["source_revision"], C, "{ev}");
    assert!(ev["source_check"].is_null(), "ancestry never ran: {ev}");
    fx.set_tag("v1.0.0", B, "ahead");
    fx.set_asset("replaced");
    let ev = fx.refused_record("v1.0.0", &policy);
    assert_eq!(ev["adoption_record"], "asset_replaced", "{ev}");
    assert_eq!(ev["source_revision"], B, "{ev}");
}

#[test]
#[serial]
fn refusal_evidence_reports_adoption_write_failure() {
    let fx = Fx::new();
    let tmp = fx
        .record
        .with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    fx.set_tag("v1.0.0", B, "ahead");
    let ev = fx.refused_record("v1.0.0", &fx.policy(Some(A)));
    assert_eq!(ev["adoption_record"], "write_failed", "{ev}");
    assert_eq!(ev["source_check"], "ahead", "{ev}");
    assert_eq!(ev["source_revision"], B, "{ev}");
    assert_eq!(ev["source_anchor"], A, "{ev}");
}
