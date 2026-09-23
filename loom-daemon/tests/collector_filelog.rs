//! Opt-in real Collector contract for the interactive-session filelog
//! receivers (#8664 item 2 / #8669): cargo test -p loom-daemon --test
//! collector_filelog -- --ignored --nocapture
//! Needs Docker. Owns a uniquely named, throwaway container.
//!
//! This does NOT hit the network or the real ClickStack/SigNoz exporters —
//! it runs the production config.yaml verbatim except for swapping the two
//! `otlp_http/*` exporters for a local `file` exporter, exactly the pattern
//! `collector_fanout.rs` uses for its auth/redaction assertions. The point
//! here is narrower and privacy-critical: does a synthetic session-store
//! fixture for each of the three new receivers (Codex/pi/Claude) come out
//! the other side of `file_log/*` + `transform/privacy` carrying ONLY the
//! allowlisted `loom.*` keys, with the raw parsed content (prompts, tool
//! output, model replies, and the malformed line) never appearing anywhere
//! in the exported record — not even in `body`, which `keep_keys` does not
//! touch.
use std::{
    fs,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use std::os::unix::fs::MetadataExt;

const IMAGE: &str = "otel/opentelemetry-collector-contrib:0.161.0@sha256:fd328de2552466ad78385e1b1289c3f2402b1c45f265b252aab1955b42845ac1";
const CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");

// Sentinel strings that must never appear anywhere in the exported output.
// Distinct per source so a leak can be attributed to the receiver at fault.
const CODEX_SENTINEL: &str = "LOOM_FILELOG_SENTINEL_CODEX_PROMPT_8669";
const PI_SENTINEL: &str = "LOOM_FILELOG_SENTINEL_PI_MESSAGE_8669";
const CLAUDE_SENTINEL: &str = "LOOM_FILELOG_SENTINEL_CLAUDE_TOOL_OUTPUT_8669";
const MALFORMED_LINE: &str = "this is not a json line at all, 8669";

fn unique_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker")
        .args(args)
        .output()
        .expect("Docker required");
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The production config with its two OTLP-HTTP exporters replaced by a
/// local `file` sink. Receivers, `transform/privacy`'s allowlist, and every
/// other processor/extension are untouched — this is the real allowlist
/// being exercised, not a rewritten copy of it.
fn file_sink_config() -> String {
    let exporters_start = CONFIG.find("exporters:\n").expect("exporters section");
    let service_start = CONFIG[exporters_start..]
        .find("service:\n")
        .map(|i| i + exporters_start)
        .expect("service section");
    format!(
        "{}exporters:\n  file:\n    path: /var/lib/otelcol/data.json\n    flush_interval: 100ms\n{}",
        &CONFIG[..exporters_start],
        &CONFIG[service_start..]
    )
    .replace(
        "exporters: [otlp_http/clickstack, otlp_http/signoz]",
        "exporters: [file]",
    )
}

struct Trial {
    dir: tempfile::TempDir,
    name: String,
    started: bool,
}
impl Drop for Trial {
    fn drop(&mut self) {
        if self.started {
            if std::thread::panicking() {
                if let Ok(out) = Command::new("docker").args(["logs", &self.name]).output() {
                    eprintln!(
                        "{}: {}{}",
                        self.name,
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
            }
            let _ = Command::new("docker")
                .args(["rm", "-f", &self.name])
                .output();
        }
    }
}
impl Trial {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
            name: format!("loom-filelog-{}", unique_id()),
            started: false,
        }
    }
    /// `rel_path` starts with "codex/", "pi/" or "claude/" — the same three
    /// source directories `config.yaml`'s `file_log/*` receivers tail at
    /// their fixed in-container paths (`/var/lib/loom-sessions/<source>`,
    /// deliberately not `${env:HOME}/...` — see README.md).
    fn write_fixture(&self, rel_path: &str, contents: &str) {
        let path = self.dir.path().join("sessions").join(rel_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    fn start(&mut self) {
        let config_file = self.dir.path().join("config.yaml");
        fs::write(&config_file, file_sink_config()).unwrap();
        let secrets = self.dir.path().join("secrets");
        fs::create_dir_all(&secrets).unwrap();
        for (name, value) in [
            ("loom_ingest_key", "fixture-loom-key"),
            ("clickstack_ingest_key", "fixture-clickstack-key"),
        ] {
            fs::write(secrets.join(name), format!("{value}\n")).unwrap();
        }
        let out = self.dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        let sessions = self.dir.path().join("sessions");
        for source in ["codex", "pi", "claude"] {
            fs::create_dir_all(sessions.join(source)).unwrap();
        }
        // Run the collector as the invoking user, mirroring the deployment's
        // `user: "${LOOM_COLLECTOR_UID}:${LOOM_COLLECTOR_GID}"` (compose.yaml).
        // Without this the image's built-in non-root UID cannot traverse the
        // 0700 tempdir tempfile created, and every extension that touches
        // /var/lib/otelcol dies with `mkdir ... permission denied` before any
        // receiver runs.
        let owner = fs::metadata(self.dir.path()).unwrap();
        let (uid, gid) = (owner.uid(), owner.gid());
        docker(&[
            "run",
            "-d",
            "--name",
            &self.name,
            "--user",
            &format!("{uid}:{gid}"),
            "-v",
            &format!("{}:/etc/otelcol/config.yaml:ro", config_file.display()),
            "-v",
            &format!("{}:/var/lib/loom-sessions/codex:ro", sessions.join("codex").display()),
            "-v",
            &format!("{}:/var/lib/loom-sessions/pi:ro", sessions.join("pi").display()),
            "-v",
            &format!("{}:/var/lib/loom-sessions/claude:ro", sessions.join("claude").display()),
            "-v",
            &format!("{}:/run/secrets:ro", secrets.display()),
            "-v",
            &format!("{}:/var/lib/otelcol", out.display()),
            IMAGE,
            "--config=/etc/otelcol/config.yaml",
        ]);
        self.started = true;
    }
    fn contents(&self) -> String {
        fs::read_to_string(self.dir.path().join("out").join("data.json")).unwrap_or_default()
    }
    fn wait_for(&self, needle: &str) {
        for _ in 0..300 {
            if self.contents().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if let Ok(out) = Command::new("docker").args(["logs", &self.name]).output() {
            eprintln!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        panic!("did not observe {needle}: {}", self.contents());
    }
}

fn write_fixtures(trial: &Trial) {
    // Codex rollout: session_meta -> loom.session_id, turn_context ->
    // loom.model, response_item carries the dangerous free-text field this
    // receiver must never reach into.
    trial.write_fixture(
        "codex/rollout-test.jsonl",
        &format!(
            concat!(
                r#"{{"type":"session_meta","payload":{{"id":"sess-codex-8669"}}}}"#,
                "\n",
                r#"{{"type":"turn_context","payload":{{"model":"gpt-5-codex"}}}}"#,
                "\n",
                r#"{{"type":"response_item","payload":{{"role":"user","content":"{sentinel}"}}}}"#,
                "\n",
            ),
            sentinel = CODEX_SENTINEL
        ),
    );

    // pi: a three-level agent_change chain (root -> child -> grandchild),
    // a raw message body, one malformed line, then a valid line recovering
    // after it.
    trial.write_fixture(
        "pi/pi-test.jsonl",
        &format!(
            concat!(
                r#"{{"type":"session","sessionId":"pi-sess-8669"}}"#,
                "\n",
                r#"{{"type":"model_change","sessionId":"pi-sess-8669","provider":"anthropic","modelId":"claude-opus-5"}}"#,
                "\n",
                r#"{{"type":"agent_change","sessionId":"pi-sess-8669","agentId":"agent-root","parentAgentId":null}}"#,
                "\n",
                r#"{{"type":"agent_change","sessionId":"pi-sess-8669","agentId":"agent-child","parentAgentId":"agent-root"}}"#,
                "\n",
                r#"{{"type":"agent_change","sessionId":"pi-sess-8669","agentId":"agent-grandchild","parentAgentId":"agent-child"}}"#,
                "\n",
                r#"{{"type":"message","sessionId":"pi-sess-8669","content":"{sentinel}"}}"#,
                "\n",
                "{malformed}\n",
                r#"{{"type":"message","sessionId":"pi-sess-8669","content":"recovered after malformed line"}}"#,
                "\n",
            ),
            sentinel = PI_SENTINEL,
            malformed = MALFORMED_LINE,
        ),
    );

    // Claude transcript: durationMs must survive as loom.duration_sec;
    // toolUseResult's raw content must never survive at all.
    trial.write_fixture(
        "claude/fixture-project/claude-test.jsonl",
        &format!(
            concat!(
                r#"{{"sessionId":"claude-sess-8669","type":"assistant","durationMs":4521}}"#,
                "\n",
                r#"{{"sessionId":"claude-sess-8669","type":"assistant","durationMs":150,"toolUseResult":{{"stdout":"{sentinel}"}}}}"#,
                "\n",
            ),
            sentinel = CLAUDE_SENTINEL
        ),
    );
}

#[test]
#[ignore = "requires Docker; starts one isolated pinned Collector container"]
fn interactive_session_filelogs_normalize_to_loom_attrs_and_never_leak_raw_content() {
    let mut trial = Trial::new();
    write_fixtures(&trial);
    trial.start();

    // Each receiver's static loom.runtime tag is the simplest positive
    // liveness signal that all three pipelines ran.
    trial.wait_for(r#""loom.runtime","value":{"stringValue":"codex"}"#);
    trial.wait_for(r#""loom.runtime","value":{"stringValue":"pi"}"#);
    trial.wait_for(r#""loom.runtime","value":{"stringValue":"claude"}"#);
    // Give the collector a moment to flush every line of every file, not
    // just the first record that matched each wait_for above.
    std::thread::sleep(Duration::from_secs(2));

    let data = trial.contents();

    // The core privacy assertion: none of the three source-specific
    // sentinels, and no fragment of the malformed line, appear anywhere in
    // the exported records -- including inside `body`, which `keep_keys`
    // does not touch (the filelog receivers must clear it themselves).
    for forbidden in [
        CODEX_SENTINEL,
        PI_SENTINEL,
        CLAUDE_SENTINEL,
        MALFORMED_LINE,
        "this is not a json",
    ] {
        assert!(
            !data.contains(forbidden),
            "forbidden raw content leaked into exported records: {forbidden}\n{data}"
        );
    }
    // Body must be empty on every record -- json_parser's raw parse target
    // is fully cleared, not merely re-serialized minus a few keys.
    assert!(
        !data.contains(r#""body":{"stringValue""#),
        "a record kept a non-empty body: {data}"
    );

    // Structural fields we DO expect: normalized identity and lineage.
    assert!(data.contains(r#""loom.session_id","value":{"stringValue":"sess-codex-8669"}"#));
    assert!(data.contains(r#""loom.model","value":{"stringValue":"gpt-5-codex"}"#));
    assert!(data.contains(r#""loom.session_id","value":{"stringValue":"pi-sess-8669"}"#));
    assert!(data.contains(r#""loom.provider","value":{"stringValue":"anthropic"}"#));
    assert!(data.contains(r#""loom.model","value":{"stringValue":"claude-opus-5"}"#));
    // Subagent lineage survives more than one level: the grandchild's
    // recorded parent is its immediate parent (agent-child), not the root
    // and not dropped.
    assert!(data.contains(r#""loom.agent_id","value":{"stringValue":"agent-child"}"#));
    assert!(data.contains(r#""loom.parent_agent_id","value":{"stringValue":"agent-root"}"#));
    assert!(data.contains(r#""loom.agent_id","value":{"stringValue":"agent-grandchild"}"#));
    assert!(data.contains(r#""loom.parent_agent_id","value":{"stringValue":"agent-child"}"#));
    // The root agent_change record (parentAgentId: null) must still get an
    // agent_id, but no fabricated parent_agent_id for it.
    assert!(data.contains(r#""loom.agent_id","value":{"stringValue":"agent-root"}"#));
    assert!(!data.contains(r#""loom.parent_agent_id","value":{"stringValue":"null"}"#));

    // Claude durationMs/1000 -> loom.duration_sec (ms -> s conversion, done
    // in the receiver's own operator pipeline via EXPR()).
    assert!(data.contains(r#""loom.duration_sec","value":{"doubleValue":4.521}"#));
    assert!(data.contains(r#""loom.duration_sec","value":{"doubleValue":0.15}"#));
    assert!(data.contains(r#""loom.session_id","value":{"stringValue":"claude-sess-8669"}"#));

    // The recovery line after the malformed one must still have been
    // processed -- one bad line does not stall or drop the rest of a file.
    assert!(data.contains(r#""loom.runtime","value":{"stringValue":"pi"}"#));
}

/// Cheap, no-Docker guard against the config drifting to admit a key this
/// PR did not intend: fails loudly if a future edit widens the filelog
/// receivers' `retain` lists or the privacy allowlist beyond the reviewed
/// set, without needing a container to catch it.
#[test]
fn filelog_retain_lists_and_privacy_allowlist_stay_within_the_reviewed_key_set() {
    let allowed: [&str; 7] = [
        "loom.runtime",
        "loom.session_id",
        "loom.agent_id",
        "loom.parent_agent_id",
        "loom.provider",
        "loom.model",
        "loom.duration_sec",
    ];
    let receivers_start = CONFIG.find("receivers:\n").expect("receivers section");
    let processors_start = CONFIG[receivers_start..]
        .find("\nprocessors:\n")
        .map(|i| i + receivers_start)
        .expect("processors section");
    let receivers_block = &CONFIG[receivers_start..processors_start];
    for retain_line in receivers_block.lines().filter(|l| l.contains("fields:")) {
        for token in retain_line.split("attributes[\\\"").skip(1) {
            let key = token.split("\\\"").next().unwrap();
            assert!(
                allowed.contains(&key),
                "a filelog receiver's retain list admits an unreviewed key: {key}"
            );
        }
    }
    assert!(
        CONFIG.contains("\"loom.session_id\", \"loom.agent_id\", \"loom.parent_agent_id\""),
        "transform/privacy's log keep_keys must list the three new #8669 keys"
    );
}

/// Static contract for #8686's host-side wiring (no Docker — this pins the
/// committed files, the same posture as `signoz_deployment_contract.rs`):
/// `compose.yaml` must bind-mount each runtime's session store **read-only**
/// to exactly the fixed in-container path its `file_log/*` receiver tails,
/// behind a **required** (`:?`) env var, and README's "Start and validate"
/// dotenv block must document that var. Drop any of those four properties
/// and a trial host either silently loses a tail (optional var), lets the
/// collector write a runtime's session store (no `:ro`), mounts to a path
/// nothing reads (target/receiver drift), or ships an env contract its own
/// docs don't name (README drift).
#[test]
fn session_store_mounts_pair_required_env_vars_with_fixed_receiver_paths() {
    const COMPOSE: &str = include_str!("../../defaults/observability/collector/compose.yaml");
    const README: &str = include_str!("../../defaults/observability/collector/README.md");

    // (required env var, fixed in-container mount target its receiver tails)
    const MOUNTS: &[(&str, &str)] = &[
        ("LOOM_CODEX_SESSIONS_DIR", "/var/lib/loom-sessions/codex"),
        ("LOOM_PI_SESSIONS_DIR", "/var/lib/loom-sessions/pi"),
        ("LOOM_CLAUDE_PROJECTS_DIR", "/var/lib/loom-sessions/claude"),
    ];

    for (var, target) in MOUNTS {
        let gated = format!("${{{var}:?");
        let mount_lines: Vec<&str> = COMPOSE.lines().filter(|l| l.contains(&gated)).collect();
        assert_eq!(
            mount_lines.len(),
            1,
            "{var} must gate exactly one compose mount, found {n}",
            n = mount_lines.len()
        );
        let line = mount_lines[0].trim_start_matches([' ', '-']);
        assert!(
            line.ends_with(&format!("{target}:ro")),
            "{var}'s mount must bind read-only to the fixed path {target} (got: {line})"
        );

        // The mount target must be the path its receiver actually tails:
        // mount/receiver drift makes the receiver idle while the host thinks
        // it is ingesting.
        let tailed_glob = format!("\"{target}/**/*.jsonl\"");
        assert!(
            CONFIG.contains(&tailed_glob),
            "no file_log receiver tails {tailed_glob}; the {var} mount feeds nothing"
        );

        // The env contract must be documented where operators provision it.
        assert!(
            README.contains(&format!("{var}=")),
            "README's 'Start and validate' dotenv block must document {var}"
        );
    }

    // The pre-existing volume/secret contract is unchanged by #8686.
    assert!(COMPOSE.contains("./config.yaml:/etc/otelcol/config.yaml:ro"));
    assert!(COMPOSE.contains("${LOOM_COLLECTOR_STATE_DIR:?"));
    assert!(COMPOSE.contains(":/var/lib/otelcol"));
}
