/// Issue #9881: every OTLP log record must carry its kind as an ordinary
/// `loom.kind` attribute, and the gateway must admit it.
///
/// The dashboard's log queries cannot filter on the log `name` column
/// (SigNoz lowers `name` to `JSON_VALUE(body, …)` and the ClickHouse build
/// rejects the JSON functions — loom-ui#747), so they discriminate on
/// `loom.kind` instead. Nothing stamped it: the only rows that ever matched
/// were d1sync bring-up rows whose payload-copy `kind` predates the omit
/// list, which is exactly why `charts-outcomes` appeared to "stop on
/// 2026-09-29" while sweeps kept running — the query was reading the
/// backfill's tail, not live emission. These tests pin both halves of the
/// fix: the mapping stamps the key on every log record, and the mounted
/// gateway config admits it through `keep_keys` (logs AND spans — the
/// span-side readers use the same discriminator family).
const CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");
const MAPPING: &str = include_str!("../src/observability/otlp/mapping.rs");

fn keep_keys(context: &str) -> Vec<String> {
    let mut current = "";
    let mut keys = Vec::new();
    for line in CONFIG.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("- context:") {
            current = rest.trim();
        }
        if current == context && line.contains("keep_keys(") {
            keys.extend(line.split('"').skip(1).step_by(2).map(str::to_owned));
        }
    }
    keys
}

#[test]
fn the_gateway_admits_loom_kind_on_logs_and_spans() {
    assert!(
        keep_keys("log").iter().any(|key| key == "loom.kind"),
        "the gateway's log keep_keys strips loom.kind — every log-kind query \
         (charts-outcomes, charts-durations, the sweep-facts extractions) reads \
         only rows whose producer happened to stamp it (#9881)"
    );
    assert!(
        keep_keys("span").iter().any(|key| key == "loom.kind"),
        "the gateway's span keep_keys strips loom.kind — span-side kind \
         discrimination would silently lose every attribute (#9881)"
    );
}

#[test]
fn the_mapping_stamps_loom_kind_on_every_log_record() {
    // One stamp, applied AFTER the per-kind match, so no arm can forget it.
    // The per-arm form (#9881) did exactly that: the `eta.*`, `ci.*`,
    // `sweep.phase`, `daemon.event`, `pr.resolved`, `session.output`,
    // `auto_update.tick` and `token_ranking.refresh` arms never pushed it, so
    // `loom.kind LIKE 'eta%'` read as "no ETAs ever" (#10899). The behavioural
    // contract — every `otlp: Logs` registry kind maps to exactly one
    // `loom.kind` equal to its tag — is `mapping/tests/loom_kind.rs`; this
    // pins the shape that makes it hold for kinds added later.
    assert!(
        MAPPING.contains(
            r#"attributes.insert(0, kv_string("loom.kind", envelope.record.kind().to_string()));"#
        ),
        "loom.kind must be stamped once, centrally, from the record's own kind \
         tag, never a per-arm literal that can drift from telemetry/kinds.rs \
         (#9881, #10899)"
    );
    assert!(
        MAPPING.contains(r#"attributes.retain(|kv| kv.key != "loom.kind");"#),
        "an arm-pushed loom.kind must be replaced, not duplicated (#10899)"
    );
    assert!(
        !MAPPING.contains("kind_attribute"),
        "the per-arm stamp is gone; a new arm must not reintroduce it (#10899)"
    );
}

#[test]
fn the_stamp_is_the_registry_tag_not_a_second_vocabulary() {
    // The value comes from `TelemetryRecord::kind()` — the same tag the
    // envelope serializes — so the backend's kind vocabulary can never drift
    // from kinds.rs. A hand-written literal here would be a second place to
    // update on every new kind.
    assert!(
        !MAPPING.contains(r#"kv_string("loom.kind", "sweep"#),
        "loom.kind must not be spelled per-kind in the mapping; it is the \
         registry tag (#9881)"
    );
}
