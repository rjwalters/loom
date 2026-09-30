//! Static contract: the SigNoz trial's **backup-restore rehearsal overlay** must
//! keep being incapable of touching the live trial. No Docker, network, backend
//! or credential is used, so this runs in ordinary CI on any host.
//!
//! Why this exists (#8528). The README's backup procedure ends with *"Test
//! restoration into a separate project/network before relying on the backup."*
//! `restore-override.yaml` is that separate project. It has **not** been run
//! against real backup tarballs yet — `evidence.md` records what was actually
//! verified (the merged render, via `docker compose config`) and names the
//! follow-up that owns the live run. That is precisely why these assertions are
//! worth having *now*: the operator who eventually runs the rehearsal will do so
//! against a live trial, and the overlay is only safe there because of a handful
//! of properties that are **not** obvious from reading either file alone:
//!
//! * Every volume in the rendered compose carries an explicit top-level `name:`,
//!   so volume names are **not** project-prefixed. `docker compose -p
//!   loom-signoz-restore` on its own therefore yields a second project that
//!   mounts the **live** volumes read-write. Only the overlay's explicit volume
//!   renames prevent that, and a re-render that adds a fifth volume the overlay
//!   does not rename silently reintroduces it.
//! * The rendered ingester joins the shared gateway network (#8526) under the
//!   alias the gateway's exporter resolves. A rehearsal that joined it too would
//!   register that alias a second time and take a share of live OTLP traffic —
//!   invisibly, because Docker load-balances the alias and SigNoz has no way to
//!   report telemetry it never received.
//! * `down --volumes` is the rehearsal's teardown. It is only safe while every
//!   object it can reach is namespaced under the restore prefix.
//!
//! Each of those is a property of a **generated** file (`foundryctl forge`
//! re-renders `pours/`), so this test re-derives them from the rendered compose
//! rather than restating them, and fails if the overlay stops covering what the
//! render actually declares.
//!
//! The last group of tests covers the *documented commands* rather than the
//! overlay, because the rehearsal is only as safe as the lines an operator
//! pastes. Three defects in the first draft of this procedure were caught by
//! rendering it (`docker compose … config`) rather than by reading it: a service
//! whose pinned `container_name` was never re-pointed, an overlay `-f` path that
//! does not resolve from the documented working directory, and an `exec` that
//! addressed the *renamed container* instead of the service — the last of which
//! would have silently read the live ClickHouse.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;

const COMPOSE: &str =
    include_str!("../../defaults/observability/signoz/pours/deployment/compose.yaml");
const OVERRIDE: &str = include_str!("../../defaults/observability/signoz/restore-override.yaml");
const README: &str = include_str!("../../defaults/observability/signoz/README.md");

/// Filename the README must cite, so the overlay cannot be renamed out from
/// under the documented procedure.
const OVERRIDE_FILE: &str = "restore-override.yaml";

/// The live trial's project prefix. Everything the rehearsal creates must live
/// under a *longer* prefix that starts with it, so the two are unambiguously
/// distinguishable and neither can reach the other's objects.
const LIVE_PROJECT: &str = "loom-signoz";

/// The rehearsal's own prefix.
const RESTORE_PROJECT: &str = "loom-signoz-restore";

// ---------------------------------------------------------------------------
// Minimal YAML readers (same shape as `signoz_deployment_contract.rs`)
// ---------------------------------------------------------------------------

fn block_children(text: &str, indent: usize) -> BTreeMap<String, String> {
    let key = Regex::new(&format!(r"^ {{{indent}}}([A-Za-z0-9_./-]+):(.*)$")).unwrap();
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(captured) = key.captures(line) {
            let name = captured[1].to_owned();
            out.insert(name.clone(), captured[2].trim().to_owned());
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

fn section(text: &str, name: &str) -> BTreeMap<String, String> {
    block_children(text, 0)
        .get(name)
        .map(|body| block_children(body, 2))
        .unwrap_or_default()
}

fn project_name(text: &str) -> String {
    block_children(text, 0)
        .get("name")
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

/// Declared key -> the concrete Docker object name it resolves to. Compose only
/// project-prefixes a key that has no explicit `name:`, and this deployment
/// gives every one an explicit name, so an absent `name:` is itself worth
/// reporting.
fn declared_names(text: &str, kind: &str) -> BTreeMap<String, Option<String>> {
    section(text, kind)
        .into_iter()
        .map(|(key, body)| {
            let name = Regex::new(r"(?m)^\s{4}name:\s*(\S+)\s*$")
                .unwrap()
                .captures(&body)
                .map(|captured| captured[1].to_owned());
            (key, name)
        })
        .collect()
}

fn sequence_items(body: &str) -> Vec<String> {
    let item = Regex::new(r"^\s*- (.+)$").unwrap();
    body.lines()
        .filter_map(|line| item.captures(line).map(|c| c[1].trim().to_owned()))
        .collect()
}

/// Service -> its declared `container_name`, for services that pin one.
fn container_names(text: &str) -> BTreeMap<String, String> {
    section(text, "services")
        .into_iter()
        .filter_map(|(service, body)| {
            Regex::new(r"(?m)^\s{4}container_name:\s*(\S+)\s*$")
                .unwrap()
                .captures(&body)
                .map(|captured| (service, captured[1].to_owned()))
        })
        .collect()
}

/// Service -> each `<host-ip>:<host-port>:<container-port>` mapping it publishes.
fn published_ports(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (service, body) in section(text, "services") {
        let Some(ports) = block_children(&body, 4).get("ports").cloned() else {
            continue;
        };
        for mapping in sequence_items(&ports) {
            out.push((service.clone(), mapping));
        }
    }
    out
}

/// Networks a service joins, in either the bare-sequence or the mapping form.
fn service_networks(body: &str) -> BTreeSet<String> {
    let Some(networks) = block_children(body, 4).get("networks").cloned() else {
        return BTreeSet::new();
    };
    let mut names: BTreeSet<String> = block_children(&networks, 6).into_keys().collect();
    names.extend(sequence_items(&networks));
    names
}

/// The network key the rendered compose declares `external: true` — the shared
/// gateway network the live ingester reaches SigNoz on. Derived, not named, so
/// renaming it upstream cannot make this test pass vacuously.
fn shared_network_key() -> String {
    let mut external: Vec<String> = section(COMPOSE, "networks")
        .into_iter()
        .filter(|(_, body)| {
            Regex::new(r"(?m)^\s{4}external:\s*true\s*$")
                .unwrap()
                .is_match(body)
        })
        .map(|(key, _)| key)
        .collect();
    assert_eq!(
        external.len(),
        1,
        "expected exactly one externally-declared network in the render, found {external:?}"
    );
    external.pop().unwrap()
}

// ---------------------------------------------------------------------------
// Reader sanity — a vacuous parse must fail loudly, not pass everything
// ---------------------------------------------------------------------------

#[test]
fn the_readers_actually_see_the_two_files() {
    assert!(
        section(COMPOSE, "services").len() >= 5,
        "parsed fewer than 5 rendered services; the reader is out of step with Foundry's output"
    );
    assert_eq!(
        declared_names(COMPOSE, "volumes").len(),
        4,
        "the README's backup procedure snapshots exactly four named volumes; the render now \
         declares a different number, so both the procedure and the overlay need revisiting"
    );
    assert!(
        !section(OVERRIDE, "services").is_empty()
            && !declared_names(OVERRIDE, "volumes").is_empty()
            && !declared_names(OVERRIDE, "networks").is_empty(),
        "the overlay parsed as empty; every assertion below would be vacuous"
    );
}

// ---------------------------------------------------------------------------
// The rehearsal cannot mount live data
// ---------------------------------------------------------------------------

/// The failure this guards is silent and total: because every rendered volume
/// carries an explicit `name:`, a restore project that does not re-point a
/// volume mounts the **live** one read-write. A "rehearsal" would then be a
/// second ClickHouse writing into the trial's own data directory.
#[test]
fn every_rendered_volume_is_repointed_to_a_distinct_restore_volume() {
    let live = declared_names(COMPOSE, "volumes");
    let restore = declared_names(OVERRIDE, "volumes");

    let live_names: BTreeSet<String> = live
        .iter()
        .map(|(key, name)| {
            name.clone()
                .unwrap_or_else(|| panic!("rendered volume {key} has no explicit name"))
        })
        .collect();

    for (key, live_name) in &live {
        let live_name = live_name.as_ref().unwrap();
        let restore_name = restore
            .get(key)
            .unwrap_or_else(|| {
                panic!(
                    "the render declares volume {key} but {OVERRIDE_FILE} does not re-point it, \
                     so a restore rehearsal would mount the live volume {live_name}"
                )
            })
            .as_ref()
            .unwrap_or_else(|| panic!("{OVERRIDE_FILE} re-points {key} without an explicit name"));

        assert_ne!(
            restore_name, live_name,
            "{OVERRIDE_FILE} maps {key} onto the live volume {live_name}"
        );
        assert!(
            restore_name.starts_with(&format!("{RESTORE_PROJECT}-")),
            "restore volume {restore_name} is outside the {RESTORE_PROJECT}- prefix, so \
             `down --volumes` scoping no longer bounds what the teardown can remove"
        );
        assert!(
            !live_names.contains(restore_name),
            "restore volume {restore_name} collides with a live volume name"
        );
    }
}

/// Container names are pinned in the render, so a second project inherits them
/// verbatim. Docker refuses the duplicate, which is merely a failed rehearsal —
/// but a rehearsal that *starts* is the one that must not be able to address a
/// live container by name.
#[test]
fn every_pinned_container_name_is_repointed_into_the_restore_namespace() {
    let live = container_names(COMPOSE);
    let restore = container_names(OVERRIDE);
    assert!(
        live.len() >= 5,
        "expected the render to pin at least five container names, found {}",
        live.len()
    );

    let live_names: BTreeSet<&String> = live.values().collect();
    for service in live.keys() {
        let restore_name = restore.get(service).unwrap_or_else(|| {
            panic!("{OVERRIDE_FILE} does not override container_name for service {service}")
        });
        assert!(
            !live_names.contains(restore_name),
            "restore container {restore_name} collides with a live container name"
        );
        assert!(
            restore_name.starts_with(&format!("{RESTORE_PROJECT}-")),
            "restore container {restore_name} is outside the {RESTORE_PROJECT}- prefix"
        );
    }
}

// ---------------------------------------------------------------------------
// The rehearsal cannot reach the shared gateway network
// ---------------------------------------------------------------------------

/// If the rehearsal joined the external gateway network it would claim the
/// exporter's alias a second time, and Docker would split live OTLP traffic
/// between the live ingester and a throwaway restored copy. Nothing reports
/// that: the gateway sees healthy 2xx responses either way.
#[test]
fn the_shared_gateway_network_is_neutralised_in_the_restore_project() {
    let shared = shared_network_key();
    let live_networks = declared_names(COMPOSE, "networks");
    let live_shared_name = live_networks
        .get(&shared)
        .and_then(Clone::clone)
        .unwrap_or_else(|| panic!("rendered network {shared} has no explicit name"));

    let body = section(OVERRIDE, "networks")
        .get(&shared)
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "{OVERRIDE_FILE} must redeclare the external network {shared}; without it the \
                 restore project attaches to the live gateway network {live_shared_name}"
            )
        });

    assert!(
        Regex::new(r"(?m)^\s{4}external:\s*false\s*$")
            .unwrap()
            .is_match(&body),
        "{OVERRIDE_FILE} must set `external: false` on {shared} so the restore project cannot \
         attach to the gateway's own network"
    );
    assert!(
        Regex::new(r"(?m)^\s{4}internal:\s*true\s*$")
            .unwrap()
            .is_match(&body),
        "{OVERRIDE_FILE} must set `internal: true` on {shared}; the quarantine bridge exists so \
         an accidental `up` of every service still reaches nothing"
    );

    let quarantine = Regex::new(r"(?m)^\s{4}name:\s*(\S+)\s*$")
        .unwrap()
        .captures(&body)
        .map(|captured| captured[1].to_owned())
        .unwrap_or_else(|| panic!("{OVERRIDE_FILE} must give {shared} an explicit distinct name"));
    assert_ne!(
        quarantine, live_shared_name,
        "{OVERRIDE_FILE} points {shared} at the live gateway network"
    );
    assert!(
        quarantine.starts_with(&format!("{RESTORE_PROJECT}-")),
        "quarantine network {quarantine} is outside the {RESTORE_PROJECT}- prefix"
    );
}

/// Every network, not just the external one — a project-local network that kept
/// its live name would put restore containers and live containers on one bridge,
/// resolving each other's service names.
#[test]
fn every_rendered_network_is_repointed_to_a_distinct_restore_network() {
    let live = declared_names(COMPOSE, "networks");
    let restore = declared_names(OVERRIDE, "networks");
    let live_names: BTreeSet<String> = live.values().flatten().cloned().collect();

    for (key, live_name) in &live {
        let live_name = live_name
            .as_ref()
            .unwrap_or_else(|| panic!("rendered network {key} has no explicit name"));
        let restore_name = restore
            .get(key)
            .and_then(Clone::clone)
            .unwrap_or_else(|| panic!("{OVERRIDE_FILE} does not re-point network {key}"));
        assert_ne!(
            &restore_name, live_name,
            "{OVERRIDE_FILE} maps network {key} onto the live network {live_name}"
        );
        assert!(
            !live_names.contains(&restore_name) || &restore_name == live_name,
            "restore network {restore_name} collides with a live network name"
        );
    }
}

/// Belt to the quarantine-network braces: the services that reach the gateway in
/// the live render are scaled to zero, so the documented single-service `up`
/// invocation is not the only thing keeping them down.
#[test]
fn services_on_the_shared_network_are_scaled_to_zero_in_the_restore_project() {
    let shared = shared_network_key();
    let attached: Vec<String> = section(COMPOSE, "services")
        .into_iter()
        .filter(|(_, body)| service_networks(body).contains(&shared))
        .map(|(service, _)| service)
        .collect();
    assert!(
        !attached.is_empty(),
        "no rendered service joins {shared}; this test would be vacuous"
    );

    let scaled_to_zero = Regex::new(r"(?m)^\s{6}replicas:\s*0\s*$").unwrap();
    for service in attached {
        let body = section(OVERRIDE, "services")
            .get(&service)
            .cloned()
            .unwrap_or_else(|| {
                panic!("{OVERRIDE_FILE} must override {service}, which joins the gateway network")
            });
        assert!(
            scaled_to_zero.is_match(&body),
            "{OVERRIDE_FILE} must set `deploy.replicas: 0` on {service}"
        );
    }
}

// ---------------------------------------------------------------------------
// The rehearsal cannot take the live host port, or open a new one
// ---------------------------------------------------------------------------

/// The rehearsal ran *alongside* the live trial, which is only possible on a
/// different host port — and, because it replays a real metastore with real
/// accounts, that port must stay loopback-bound like the live one. `ports`
/// merges by target port, so the override has to replace the list outright; a
/// dropped `!override` tag silently restores the live binding.
#[test]
fn the_restore_ui_port_is_distinct_loopback_and_the_only_one_published() {
    let live = published_ports(COMPOSE);
    let restore = published_ports(OVERRIDE);
    assert_eq!(
        live.len(),
        1,
        "expected the render to publish exactly one host port, found {live:?}"
    );
    assert_eq!(
        restore.len(),
        1,
        "the restore overlay must publish exactly one host port (its own UI), found {restore:?}"
    );

    let (live_service, live_mapping) = &live[0];
    let (restore_service, restore_mapping) = &restore[0];
    assert_eq!(
        live_service, restore_service,
        "the overlay republishes a different service than the render does"
    );
    assert!(
        restore_mapping.starts_with("127.0.0.1:"),
        "the restored UI mapping {restore_mapping} is not loopback-bound, which would expose a \
         restored copy of real accounts and telemetry on every host interface"
    );
    assert_ne!(
        restore_mapping, live_mapping,
        "the restore overlay reuses the live host port {live_mapping}, so the rehearsal cannot \
         run alongside the trial it is rehearsing a restore of"
    );

    let host_port = |mapping: &str| mapping.split(':').nth(1).unwrap().to_owned();
    assert_ne!(
        host_port(restore_mapping),
        host_port(live_mapping),
        "the restore overlay reuses the live host port number"
    );

    assert!(
        Regex::new(r"(?m)^\s{4}ports:\s*!override\s*$")
            .unwrap()
            .is_match(OVERRIDE),
        "the overlay's `ports:` must carry the `!override` tag; Compose merges ports by target \
         port, so without it the live binding survives and the rehearsal cannot start"
    );
}

// ---------------------------------------------------------------------------
// The overlay is a rehearsal tool, documented, and carries no credential
// ---------------------------------------------------------------------------

/// The overlay must be unmistakably a second project. A shared project name
/// would let `down` on either one reach the other's containers.
#[test]
fn the_overlay_declares_its_own_project_under_the_live_prefix() {
    let live = project_name(COMPOSE);
    let restore = project_name(OVERRIDE);
    assert_eq!(live, LIVE_PROJECT, "the rendered project name changed");
    assert_eq!(restore, RESTORE_PROJECT, "the overlay must declare its own project name");
    assert_ne!(live, restore);
    assert!(
        restore.starts_with(&format!("{live}-")),
        "the restore project {restore} should extend the live prefix {live} so operators can see \
         at a glance that both belong to this trial"
    );
}

/// A restore rehearsal replays a metastore whose database role and session
/// secret came from the operator's private env file. The overlay must keep
/// referencing that file, never inline a value — the same rule the rendered
/// deployment is held to.
#[test]
fn the_overlay_commits_no_credential_literal() {
    let credential =
        Regex::new(r"(?i)^\s*[-A-Za-z0-9_]*(password|secret|token|key)[A-Za-z0-9_]*:").unwrap();
    for (number, line) in OVERRIDE.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        assert!(
            !credential.is_match(line),
            "{OVERRIDE_FILE}:{} assigns a credential-shaped key; the rehearsal must reuse the \
             operator's private --env-file, not carry a value: {line}",
            number + 1
        );
    }
}

/// An overlay nobody is told to use is not a rehearsal procedure. The README has
/// to name the file, the project and the port an operator will actually see —
/// and the Compose version the `!override` tag needs, since the assertion above
/// requires that tag and the trial's stated baseline of "Compose v2" alone does
/// not provide it.
#[test]
fn the_readme_documents_the_rehearsal_procedure() {
    let (_, restore_mapping) = &published_ports(OVERRIDE)[0];
    let host_port = restore_mapping.split(':').nth(1).unwrap();

    for needle in [
        OVERRIDE_FILE,
        RESTORE_PROJECT,
        host_port,
        "--dry-run",
        "2.24.4",
    ] {
        assert!(
            README.contains(needle),
            "the SigNoz README must document {needle:?} as part of the restore rehearsal"
        );
    }
}

// ---------------------------------------------------------------------------
// The documented commands are the rehearsal — they get their own contract
// ---------------------------------------------------------------------------

/// The rehearsal section of the README, with shell line-continuations folded so
/// each command reads as one line. Every command an operator pastes for the
/// rehearsal lives here, and nowhere else in the README addresses the restore
/// project.
fn rehearsal_commands() -> Vec<String> {
    let start = README
        .find("## Backup-restore rehearsal")
        .expect("the README must carry a `## Backup-restore rehearsal` section");
    let rest = &README[start + "## Backup-restore rehearsal".len()..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    let folded = rest[..end].replace("\\\n", " ");
    folded
        .lines()
        .map(|line| line.trim().to_owned())
        .filter(|line| line.contains("docker "))
        .collect()
}

/// `-f` paths are resolved against the **working directory**, not against the
/// first compose file's directory (which is what governs paths *inside* the
/// files). A plausible-looking `-f ../../restore-override.yaml` therefore fails
/// outright from the directory the README tells the operator to stand in — the
/// first draft of this procedure shipped exactly that, and `docker compose
/// config` was what caught it.
#[test]
fn every_documented_overlay_reference_resolves_from_this_directory() {
    let commands = rehearsal_commands();
    let referencing: Vec<&String> = commands
        .iter()
        .filter(|command| command.contains(OVERRIDE_FILE))
        .collect();
    assert!(
        referencing.len() >= 3,
        "expected the rehearsal to document at least three overlay-scoped commands (up, exec, \
         down), found {}",
        referencing.len()
    );
    for command in referencing {
        assert!(
            command.contains(&format!("-f {OVERRIDE_FILE}")),
            "the overlay must be passed as `-f {OVERRIDE_FILE}` (resolved from this directory, \
             like every other command in this README), not by a relative path: {command}"
        );
    }
}

/// `docker compose exec` addresses a **service**, and the overlay deliberately
/// renames containers rather than services. Pasting the restore *container* name
/// there fails with "no such service" — and the mistake's more dangerous twin,
/// dropping `-f restore-override.yaml` from an otherwise-correct line, reads the
/// live ClickHouse instead. Both are caught by requiring every documented `exec`
/// target to be a service the render declares.
#[test]
fn documented_exec_commands_address_rendered_services_in_the_restore_project() {
    let services = section(COMPOSE, "services");
    let target = Regex::new(r"exec\s+(?:-\S+\s+)*([A-Za-z0-9][A-Za-z0-9._-]*)").unwrap();

    let mut checked = 0usize;
    for command in rehearsal_commands() {
        if !command.contains(" exec") {
            continue;
        }
        let captured = target
            .captures(&command)
            .unwrap_or_else(|| panic!("could not read the exec target out of: {command}"));
        let addressed = captured[1].to_owned();
        assert!(
            services.contains_key(&addressed),
            "the rehearsal `exec`s {addressed}, which is not a service the render declares — the \
             overlay renames containers, not services, so a restore container name fails here: \
             {command}"
        );
        assert!(
            command.contains(&format!("-f {OVERRIDE_FILE}")),
            "a rehearsal `exec` without `-f {OVERRIDE_FILE}` addresses the LIVE project's \
             {addressed}: {command}"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "the rehearsal documents no `exec` verification step; the restored data would be accepted \
         on appearance alone"
    );
}

/// The rehearsal moves volume contents with a throwaway container, and the trial
/// promises that setup evidence lists exact versions. So the helper image must be
/// one the render already pins by digest — that keeps the procedure pulling
/// nothing new onto the trial host and keeps it inside the trial's own version
/// ledger, instead of trusting whatever a floating tag resolves to on the day.
#[test]
fn every_rehearsal_helper_image_is_already_pinned_by_the_render() {
    let digest_ref =
        Regex::new(r"[A-Za-z0-9][A-Za-z0-9._/-]*:[A-Za-z0-9._-]+@sha256:[0-9a-f]{64}").unwrap();
    let section_start = README.find("## Backup-restore rehearsal").unwrap();
    let rest = &README[section_start..];
    let section_text = &rest[..rest[4..].find("\n## ").map_or(rest.len(), |at| at + 4)];

    let pinned: BTreeSet<String> = digest_ref
        .find_iter(section_text)
        .map(|found| found.as_str().to_owned())
        .collect();
    assert!(
        !pinned.is_empty(),
        "the rehearsal must name its helper image by digest; an unpinned tag is outside the \
         trial's version ledger"
    );
    for image in &pinned {
        assert!(
            COMPOSE.contains(image.as_str()),
            "rehearsal helper image {image} is not one the rendered deployment already pins, so \
             the procedure pulls an image nobody reviewed onto the trial host"
        );
    }

    // Every `docker run` in the section must use one of those pinned references,
    // whether spelled inline or through the shell variable the snapshot loop
    // assigns it to. Without this, adding one `docker run … alpine` line would
    // leave the check above passing while the procedure grew an unpinned image.
    for command in rehearsal_commands() {
        if !command.contains("docker run") {
            continue;
        }
        assert!(
            command.contains("@sha256:") || command.contains("TARBALLER"),
            "this rehearsal `docker run` names an image that is neither digest-pinned inline nor \
             the pinned helper: {command}"
        );
    }
}
