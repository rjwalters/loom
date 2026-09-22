//! Static contract: the *rendered* SigNoz trial deployment must keep matching the
//! safety and identity properties its README and `evidence.md` claim for it. No
//! Docker, network, backend, or credential is used, so this runs in ordinary CI
//! on any host.
//!
//! Why this exists (#8528): every property asserted below was established once,
//! by hand, on a trial host — "only the UI is published", "only the ingester joins
//! the shared gateway network", "no credential is committed", "these are the exact
//! image digests", "the histogram helper is checksummed before extraction". They
//! are all properties of committed, *regenerated* files: `casting.yaml` is edited
//! and `foundryctl forge` re-renders `pours/`, so any of them can be silently
//! undone by a later re-render, a hand-edit of the generated output, or an
//! upstream default change — on a host nobody is watching, with no error raised.
//!
//! Two of them are security properties rather than conveniences. SigNoz's
//! self-hosted OTLP receiver is unauthenticated by design (the trial keeps it
//! private and authenticates the separate Loom-facing gateway instead), so a
//! re-render that published 4317/4318 — or published anything on `0.0.0.0`
//! instead of `127.0.0.1` — would expose an open ingest endpoint. And the trial
//! host runs a separately-owned SigNoz installation that must not be touched, so
//! the `loom-signoz` project/volume/container naming is what keeps `docker compose
//! down --volumes` destroying only the disposable trial.
//!
//! As in [`signoz_trial_artifacts`], the authorities are derived rather than
//! restated: the shared-network hostname comes from the gateway config the
//! deployment actually exports to, the image digests come from the casting, and
//! the documented memory budget is re-summed from the rendered limits.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;

const COMPOSE: &str =
    include_str!("../../defaults/observability/signoz/pours/deployment/compose.yaml");
const CASTING: &str = include_str!("../../defaults/observability/signoz/casting.yaml");
const LOCK: &str = include_str!("../../defaults/observability/signoz/casting.yaml.lock");
const README: &str = include_str!("../../defaults/observability/signoz/README.md");
const COLLECTOR_CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");

/// Everything in this deployment is namespaced under one project prefix so a
/// wipe cannot reach the trial host's separately-owned SigNoz installation.
const PROJECT: &str = "loom-signoz";

/// The one network this project shares with the neutral gateway. Declared
/// `external` so `docker compose down` never removes it out from under
/// ClickStack or the gateway itself.
const SHARED_NETWORK: &str = "loom-observability";

/// Container ports that must never be published to a host interface. The OTLP
/// pair is unauthenticated on self-hosted SigNoz; the rest are datastore and
/// control-plane ports.
const PRIVATE_CONTAINER_PORTS: &[&str] = &["4317", "4318", "8123", "9000", "9181", "5432", "4320"];

/// Credentials the operator supplies from a private env file outside every
/// checkout. A rendered file may only ever *reference* these.
const REQUIRED_SECRET_VARS: &[&str] = &["SIGNOZ_TOKENIZER_JWT_SECRET", "SIGNOZ_POSTGRES_PASSWORD"];

/// What a key has to be called to be treated as a credential slot.
///
/// The scan is keyed on the *consumption site* rather than on the secret's own
/// variable name, because the two are not the same string anywhere it matters:
/// the Postgres password arrives as `SIGNOZ_POSTGRES_PASSWORD` but is consumed
/// under `POSTGRES_PASSWORD` and inside `SIGNOZ_SQLSTORE_POSTGRES_DSN`, and the
/// casting and the lock are YAML *mappings* (`KEY: value`) that never spell
/// `SIGNOZ_POSTGRES_PASSWORD=` at all. A scan for the variable's own name
/// therefore inspects almost none of the places a literal can be pasted — most
/// of all `casting.yaml`, the one file here that is meant to be hand-edited.
const CREDENTIAL_KEY: &str = r"[A-Za-z0-9_]*(?i:password|secret)[A-Za-z0-9_]*";

/// `(key, value)` pairs whose key matches [`CREDENTIAL_KEY`] but which are not
/// credential slots. Both are ClickHouse server settings the lock embeds
/// verbatim: a feature flag, and the `default` user's deliberately empty
/// password (that datastore is reachable only on the project's private network,
/// and the setting is upstream's, not this trial's).
///
/// The permitted value is pinned, so this exempts `password: ""` without
/// exempting `password: hunter2`; and every pair is asserted below to still
/// match something, so a rename upstream cannot leave a dead exemption quietly
/// widening the scan.
const NON_CREDENTIAL_SITES: &[(&str, &str)] =
    &[("password", ""), ("show_named_collection_secrets", "1")];

// ---------------------------------------------------------------------------
// Minimal reader for Foundry's rendered block-style YAML
// ---------------------------------------------------------------------------

/// Splits a YAML block mapping into `key -> body` at exactly `indent` spaces.
///
/// Foundry renders plain block style with sorted keys, no anchors and no flow
/// mappings, and Compose sequences sit at the same indent as their key with a
/// `- ` prefix (which cannot match a key, because the space after the dash is
/// not a key character). That makes an indentation split exact for this file —
/// and when it is not, the key a test needs simply goes missing and that test
/// fails loudly rather than silently passing.
fn block_children(text: &str, indent: usize) -> BTreeMap<String, String> {
    let key = Regex::new(&format!(r"^ {{{indent}}}([A-Za-z0-9_./-]+):(.*)$")).unwrap();
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(captured) = key.captures(line) {
            let name = captured[1].to_owned();
            let inline = captured[2].trim().to_owned();
            out.insert(name.clone(), inline);
            current = Some(name);
            continue;
        }
        if let Some(name) = &current {
            let entry = out.get_mut(name).unwrap();
            entry.push('\n');
            entry.push_str(line);
        }
    }
    out
}

fn services() -> BTreeMap<String, String> {
    let top = block_children(COMPOSE, 0);
    let services = top
        .get("services")
        .expect("rendered compose must have a services mapping")
        .clone();
    let parsed = block_children(&services, 2);
    assert!(
        parsed.len() >= 5,
        "only parsed {} services out of the rendered compose; the reader is out of step with \
         Foundry's output format",
        parsed.len()
    );
    parsed
}

/// Items of a YAML block sequence, whatever indent it sits at.
fn sequence_items(body: &str) -> Vec<String> {
    let item = Regex::new(r"^\s*- (.+)$").unwrap();
    body.lines()
        .filter_map(|line| item.captures(line).map(|c| c[1].trim().to_owned()))
        .collect()
}

/// A scalar declared directly on a service (4-space indent), so a same-named key
/// nested deeper (`networks.<net>.name`, `logging.options.*`) cannot be mistaken
/// for it.
fn service_scalar(block: &str, key: &str) -> Option<String> {
    Regex::new(&format!(r"(?m)^ {{4}}{key}: (.+)$"))
        .unwrap()
        .captures(block)
        .map(|captured| captured[1].trim().to_owned())
}

/// Mebibytes from a Compose `mem_limit` value (`2g`, `768m`).
fn mebibytes(limit: &str) -> u64 {
    let (value, unit) = limit.split_at(limit.len() - 1);
    let value: u64 = value
        .parse()
        .unwrap_or_else(|_| panic!("unparsable mem_limit {limit}"));
    match unit {
        "g" => value * 1024,
        "m" => value,
        other => panic!("unexpected mem_limit unit {other} in {limit}"),
    }
}

/// Networks a service joins. Compose accepts both a bare sequence of names and a
/// mapping keyed by name (the form that carries `aliases`), and this deployment
/// renders both, so read them the same way.
fn service_networks(block: &str) -> BTreeSet<String> {
    let Some(networks) = block_children(block, 4).get("networks").cloned() else {
        return BTreeSet::new();
    };
    let mut names: BTreeSet<String> = block_children(&networks, 6).into_keys().collect();
    names.extend(sequence_items(&networks));
    names
}

/// `host:port` the neutral gateway's SigNoz exporter actually sends to.
fn gateway_signoz_target() -> (String, String) {
    let endpoint = Regex::new(r"otlp_http/signoz:\s*\n\s*endpoint:\s*(\S+)")
        .unwrap()
        .captures(COLLECTOR_CONFIG)
        .expect("collector config must define the otlp_http/signoz exporter endpoint")
        .get(1)
        .unwrap()
        .as_str()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_owned();
    let (host, port) = endpoint
        .split_once(':')
        .unwrap_or_else(|| panic!("gateway SigNoz endpoint {endpoint} has no explicit port"));
    (host.to_owned(), port.to_owned())
}

/// Every `<host-ip>:<host-port>:<container-port>` mapping in the project.
fn published_ports() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, block) in services() {
        let Some(ports) = block_children(&block, 4).get("ports").cloned() else {
            continue;
        };
        for item in sequence_items(&ports) {
            out.push((name.clone(), item));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Trust boundary
// ---------------------------------------------------------------------------

/// SigNoz's self-hosted OTLP receiver is unauthenticated by default, so the trial
/// keeps it off every host interface and authenticates the neutral gateway
/// instead. A re-render that added a convenience port mapping would quietly turn
/// the trial host into an open ingest endpoint.
#[test]
fn only_the_documented_loopback_ui_port_is_published() {
    let published = published_ports();
    assert_eq!(
        published.len(),
        1,
        "the trial publishes exactly one host port (the UI); found {published:?}"
    );

    for (service, mapping) in &published {
        assert!(
            mapping.starts_with("127.0.0.1:"),
            "{service} publishes {mapping} without a loopback bind address, which exposes it on \
             every host interface"
        );
        let container_port = mapping.rsplit(':').next().unwrap();
        assert!(
            !PRIVATE_CONTAINER_PORTS.contains(&container_port),
            "{service} publishes container port {container_port}, which the README documents as \
             private (OTLP, ClickHouse, PostgreSQL, Keeper and OpAMP)"
        );
        assert!(
            README.contains(mapping.rsplit_once(':').unwrap().0),
            "the README must document the published endpoint {mapping}"
        );
    }
}

/// The gateway reaches SigNoz by network alias, not by host port. If the alias,
/// the shared network, or its `external` declaration drifts, the gateway's
/// exporter resolves nothing — and OTLP delivery failure is visible only in the
/// gateway's own metrics, never in SigNoz.
#[test]
fn only_the_ingester_joins_the_shared_gateway_network_under_the_exporters_hostname() {
    let (host, port) = gateway_signoz_target();

    let shared = block_children(COMPOSE, 0)
        .get("networks")
        .map(|networks| block_children(networks, 2))
        .and_then(|networks| networks.get(SHARED_NETWORK).cloned())
        .unwrap_or_else(|| panic!("rendered compose must declare the {SHARED_NETWORK} network"));
    assert!(
        shared.contains("external: true"),
        "{SHARED_NETWORK} must stay `external: true`; otherwise `docker compose down` removes the \
         network the gateway and ClickStack also use"
    );

    let attached: Vec<String> = services()
        .into_iter()
        .filter(|(_, block)| service_networks(block).contains(SHARED_NETWORK))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        attached.len(),
        1,
        "exactly one service (the ingester) may join {SHARED_NETWORK}; found {attached:?}"
    );

    let block = services().get(&attached[0]).cloned().unwrap();
    let networks = block_children(&block, 4).get("networks").cloned().unwrap();
    let aliases = sequence_items(
        &block_children(&networks, 6)
            .get(SHARED_NETWORK)
            .cloned()
            .unwrap(),
    );
    assert!(
        aliases.contains(&host),
        "the gateway exports to host '{host}', but {} is aliased {aliases:?} on {SHARED_NETWORK}",
        attached[0]
    );
    assert!(
        !published_ports()
            .iter()
            .any(|(_, mapping)| mapping.ends_with(&format!(":{port}"))),
        "the SigNoz receiver port {port} must stay reachable only over {SHARED_NETWORK}"
    );
}

/// Everything is namespaced so the trial cannot be confused with — or wiped
/// together with — the separately-owned SigNoz installation on the same host.
#[test]
fn every_rendered_object_is_namespaced_to_this_trial_project() {
    let top = block_children(COMPOSE, 0);
    assert_eq!(
        top.get("name").map(String::as_str),
        Some(PROJECT),
        "the Compose project name pins every default container/volume/network prefix"
    );

    let explicit_name = Regex::new(r"(?m)^\s*name: (.+)$").unwrap();
    for (volume, body) in block_children(top.get("volumes").unwrap(), 2) {
        let name = explicit_name
            .captures(&body)
            .map(|captured| captured[1].trim().to_owned())
            .unwrap_or_else(|| panic!("volume {volume} must declare an explicit name"));
        assert!(
            name.starts_with(PROJECT),
            "volume {name} is outside the {PROJECT} namespace, so wiping the trial could destroy \
             another deployment's data"
        );
    }

    for (network, body) in block_children(top.get("networks").unwrap(), 2) {
        if body.contains("external: true") {
            continue;
        }
        assert!(
            network.starts_with(PROJECT),
            "private network {network} is outside the {PROJECT} namespace"
        );
    }

    for (service, block) in services() {
        if let Some(container) = service_scalar(&block, "container_name") {
            assert!(
                container.starts_with(PROJECT),
                "{service} sets container_name {container}, outside the {PROJECT} namespace"
            );
        }
    }
}

/// A place where one of these files hands a credential to a process: a
/// `KEY: value` / `KEY=value` whose key names a password or a secret, or the
/// password field of a URL's `user:password@host` userinfo — which is how the
/// Postgres DSN carries it, under a key (`SIGNOZ_SQLSTORE_POSTGRES_DSN`) that
/// names neither.
#[derive(Debug)]
struct CredentialSite {
    /// What names the site, for the failure message.
    what: String,
    /// The value as written, normalised (see [`credential_sites`]).
    value: String,
}

/// The opaque marker a *required* interpolation collapses to. Deliberately free
/// of `:`, `=`, `@`, `/`, quotes and whitespace, so it cannot be mistaken for
/// structure by the site scan that runs after the collapse.
fn required_marker(var: &str) -> String {
    format!("<<{var}>>")
}

/// Percent-decodes `%XX`. Foundry URL-escapes userinfo, so the casting's `test`
/// patch operation — and the lock's copy of it — carry the interpolation as
/// `$%7BVAR%3A%3F…%7D`. Decoding first means one scan covers both spellings
/// instead of the escaped form being silently exempt.
fn percent_decode(text: &str) -> String {
    Regex::new(r"%([0-9A-Fa-f]{2})")
        .unwrap()
        .replace_all(text, |captured: &regex::Captures| {
            u8::from_str_radix(&captured[1], 16)
                .map(|byte| (byte as char).to_string())
                .unwrap_or_else(|_| captured[0].to_owned())
        })
        .into_owned()
}

/// Folds the YAML dumper's line wrapping back into one logical line.
///
/// Both the render and the lock wrap long plain scalars, and both secrets land
/// on the wrap: `${SIGNOZ_TOKENIZER_JWT_SECRET:?set a private` / `random
/// session-signing secret}`. A line-oriented scan would see only the first half
/// and could not tell a required interpolation from a truncated one. YAML
/// restores such a break as a single space, and so does this: a non-blank line
/// that is more indented than the line it follows and is not itself a comment,
/// a sequence item or a `key:` belongs to the previous line's scalar.
fn fold_wrapped_scalars(text: &str) -> String {
    let structural = Regex::new(r"^\s*(#|-(\s|$)|[A-Za-z0-9_./-]+:(\s|$))").unwrap();
    let mut folded: Vec<String> = Vec::new();
    let mut previous_indent = 0usize;
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        let continues = !line.trim().is_empty()
            && indent > previous_indent
            && !structural.is_match(line)
            && !folded.is_empty();
        if continues {
            let last = folded.last_mut().unwrap();
            last.push(' ');
            last.push_str(line.trim_start());
        } else {
            folded.push(line.to_owned());
            previous_indent = indent;
        }
    }
    folded.join("\n")
}

/// Strips one matched pair of surrounding quotes, as YAML would.
fn unquote(value: &str) -> String {
    let value = value.trim();
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

/// Every credential-consumption site in one file, with its value normalised:
/// percent-decoded, unwrapped, and with each *required* `${VAR:?…}`
/// interpolation collapsed to [`required_marker`]. Collapsing before the scan is
/// what keeps the interpolation's own `:` and `?` from reading as further
/// structure inside the value they belong to — and it is why a non-required
/// spelling (`$VAR`, `${VAR}`, `${VAR:-default}`) survives as itself and fails.
fn credential_sites(text: &str) -> Vec<CredentialSite> {
    let text = Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*):\?[^}]*\}")
        .unwrap()
        .replace_all(&fold_wrapped_scalars(&percent_decode(text)), "<<$1>>")
        .into_owned();

    let mut sites = Vec::new();
    let keyed = Regex::new(&format!(r"(?m)({CREDENTIAL_KEY})[ \t]*[:=][ \t]*(.*)$")).unwrap();
    for captured in keyed.captures_iter(&text) {
        sites.push(CredentialSite {
            what: captured[1].to_owned(),
            value: unquote(&captured[2]),
        });
    }
    let userinfo = Regex::new(r"://([^\s:/@]+):([^\s/@]*)@").unwrap();
    for captured in userinfo.captures_iter(&text) {
        sites.push(CredentialSite {
            what: format!("the URL password of `{}`", &captured[1]),
            value: unquote(&captured[2]),
        });
    }
    sites
}

/// The README promises that neither the casting, the lock nor the rendered files
/// contain an actual credential, and that a missing one fails configuration
/// rather than starting with an empty session-signing secret. Both halves are
/// the same assertion: every *site that consumes* a credential must hold a
/// required interpolation of one of [`REQUIRED_SECRET_VARS`], in all three
/// files.
///
/// The per-file minimums below are the point of the test as much as the checks
/// they guard. Scanning is regex over three differently-shaped files, so the
/// realistic failure is not a wrong verdict but *no verdict*: a re-render, or a
/// reader that drifts out of step with Foundry's output, silently leaves the
/// scan with nothing to inspect and the test passes while enforcing nothing.
/// Requiring each file to yield at least the sites known to exist — and each
/// file to consume each secret at least once — makes that failure loud.
#[test]
fn credentials_are_required_interpolations_and_never_committed_values() {
    let markers: BTreeMap<String, &str> = REQUIRED_SECRET_VARS
        .iter()
        .map(|var| (required_marker(var), *var))
        .collect();
    let mut exemptions_matched: BTreeSet<&str> = BTreeSet::new();

    // Minimums, not exact counts: `compose.yaml` consumes each secret once plus
    // the DSN; `casting.yaml` declares both and carries the DSN twice (the
    // escaped `test` operation and its `replace`); the lock repeats the render
    // for both the compose file and its own copy of the casting.
    for (label, text, minimum) in [
        ("compose.yaml", COMPOSE, 3usize),
        ("casting.yaml", CASTING, 4),
        ("casting.yaml.lock", LOCK, 8),
    ] {
        let mut consumed: BTreeSet<&str> = BTreeSet::new();
        let mut policed = 0usize;

        for site in credential_sites(text) {
            if let Some((key, _)) = NON_CREDENTIAL_SITES
                .iter()
                .find(|(key, value)| *key == site.what && *value == site.value)
            {
                exemptions_matched.insert(key);
                continue;
            }
            let var = markers.get(&site.value).copied().unwrap_or_else(|| {
                panic!(
                    "{label} gives {} the value `{}`. Every credential site must hold a \
                     `${{VAR:?…}}` required interpolation of one of {REQUIRED_SECRET_VARS:?}, so \
                     that no credential is committed and a missing one fails \
                     `docker compose config` instead of starting the stack without it",
                    site.what, site.value
                )
            });
            consumed.insert(var);
            policed += 1;
        }

        assert!(
            policed >= minimum,
            "only found {policed} credential site(s) in {label}, expected at least {minimum} — \
             the scan has gone blind to sites it is supposed to police (a re-render changed the \
             file's shape, or a site was dropped), so this test would pass while enforcing \
             nothing"
        );
        for var in REQUIRED_SECRET_VARS {
            assert!(
                consumed.contains(var),
                "no site in {label} consumes {var}; found {consumed:?}"
            );
        }
    }

    assert_eq!(
        exemptions_matched.len(),
        NON_CREDENTIAL_SITES.len(),
        "a NON_CREDENTIAL_SITES exemption no longer matches anything ({exemptions_matched:?} of \
         {NON_CREDENTIAL_SITES:?}) — a dead exemption widens the scan for nothing and must be \
         deleted"
    );
}

// ---------------------------------------------------------------------------
// Supply chain
// ---------------------------------------------------------------------------

/// A floating tag, or a digest edited into the generated output without
/// re-rendering, both break the "setup evidence lists exact versions" acceptance
/// the trial is built on.
#[test]
fn rendered_images_are_digest_pinned_and_agree_with_the_casting() {
    let pinned = Regex::new(r"^(\S+?):(\S+?)@sha256:[0-9a-f]{64}$").unwrap();
    let mut tags = BTreeSet::new();
    let mut seen = 0usize;

    for (service, block) in services() {
        let image = service_scalar(&block, "image")
            .unwrap_or_else(|| panic!("{service} must declare a pinned image"));
        let captured = pinned.captures(&image).unwrap_or_else(|| {
            panic!("{service} image {image} is not pinned to a `<repo>:<tag>@sha256:<digest>`")
        });
        assert!(
            CASTING.contains(&image),
            "{service} image {image} does not appear in casting.yaml — the rendered output was \
             hand-edited, or `foundryctl forge` was not re-run after a pin change"
        );
        tags.insert(captured[2].to_owned());
        seen += 1;
    }
    assert!(seen >= 5, "expected every service to pin an image; only checked {seen}");

    // Version-shaped tags only: the README writes PostgreSQL's bare `16` as
    // prose, and a bare-integer substring search would pass against anything.
    for tag in tags
        .iter()
        .filter(|tag| Regex::new(r"^v?\d+\.\d+").unwrap().is_match(tag))
    {
        assert!(
            README.contains(tag),
            "the README's version table no longer lists the rendered image tag {tag}"
        );
    }
}

/// The histogram helper is the one component fetched over the network at start-up
/// rather than pinned by digest, so the checksum is the entire integrity story —
/// and it is only worth anything if it runs *before* the archive is unpacked.
#[test]
fn the_histogram_helper_is_checksummed_before_extraction() {
    let init = services()
        .into_iter()
        .find(|(name, _)| name.contains("user-scripts"))
        .map(|(_, block)| block)
        .expect("the rendered compose must keep the histogram helper init job");

    let verify = init
        .find("sha256sum --check --strict")
        .expect("the init job must verify the downloaded archive's SHA-256");
    let extract = init
        .find("tar -xzf")
        .expect("the init job must extract the archive");
    assert!(
        verify < extract,
        "the init job extracts the histogram helper before verifying its checksum, which makes \
         the checksum decorative"
    );
    assert!(
        init.contains("Unsupported histogram helper architecture"),
        "the init job must refuse an architecture it has no pinned checksum for, rather than \
         skipping verification"
    );

    let expected: Vec<String> = Regex::new(r"expected=([0-9a-f]{64})")
        .unwrap()
        .captures_iter(&init)
        .map(|captured| captured[1].to_owned())
        .collect();
    assert_eq!(
        expected.len(),
        2,
        "both supported architectures (arm64 and amd64) must pin a checksum; found {expected:?}"
    );
    for digest in expected {
        assert!(
            README.contains(&digest),
            "the README must document the checksum {digest} the init job actually enforces, so \
             the documented upstream manifest and the executed check cannot drift apart"
        );
    }
}

// ---------------------------------------------------------------------------
// Footprint claims
// ---------------------------------------------------------------------------

/// The README states a memory budget an operator sizes a trial host against, and
/// `evidence.md` compares measured usage to it. Re-sum it from the rendered
/// limits so a later `casting.yaml` edit cannot leave a stale figure behind.
#[test]
fn the_documented_memory_budget_is_the_sum_of_the_rendered_limits() {
    let mut steady = 0;
    let mut transient = 0;
    for (service, block) in services() {
        let limit = service_scalar(&block, "mem_limit").unwrap_or_else(|| {
            panic!("{service} has no mem_limit; an unbounded service can starve the trial host")
        });
        let restart = service_scalar(&block, "restart").unwrap_or_default();
        if restart == "unless-stopped" {
            steady += mebibytes(&limit);
        } else {
            transient += mebibytes(&limit);
        }
    }

    // `{}` on an f64 renders 3.75 as "3.75" and 4.0 as "4", which is exactly how
    // a GiB figure is written in prose.
    let steady_gib = format!("{} GiB", steady as f64 / 1024.0);

    // The README states the budget twice — once as prose under "Resource
    // expectations", once in the rendered-deployment contract table — and a
    // `contains` check would go on passing while one of the two went stale, so
    // each statement is asserted separately. The two transient phrasings differ;
    // the steady figure is the same string in both places and is counted.
    assert_eq!(
        README.matches(&steady_gib).count(),
        2,
        "both README statements of the steady-state cap ({steady} MiB = {steady_gib}) must agree \
         with the rendered mem_limits — the prose under \"Resource expectations\" and the \
         rendered-deployment contract table"
    );
    for phrasing in [
        format!("{transient} MiB for transient"),
        format!("{transient} MiB transient"),
    ] {
        assert!(
            README.contains(&phrasing),
            "the README must state the rendered transient initialization cap as \"{phrasing}\""
        );
    }
}

/// The README's storage note ("three 10 MiB files per service") is only true
/// while every service actually caps its log driver.
#[test]
fn every_service_caps_its_container_logs() {
    for (service, block) in services() {
        assert!(
            block.contains("max-file: \"3\"") && block.contains("max-size: 10m"),
            "{service} does not cap container logs at three 10 MiB files, so trial logs can grow \
             without bound on the host"
        );
    }
    assert!(
        README.contains("three 10 MiB files per service"),
        "the README must keep documenting the rendered log rotation policy"
    );
}
