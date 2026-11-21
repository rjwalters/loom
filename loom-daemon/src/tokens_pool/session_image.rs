//! The configured session image override and extra read-only mounts (issue
//! #14019).
//!
//! Two generic settings under `.loom/config.json → accounts.session`,
//! readable at every config tier via [`crate::config_resolver`]
//! (host-shared defaults through per-repo local):
//!
//! * **`image`** — the image a session container is created from, **pinned by
//!   digest** (`repo@sha256:<64 hex>`). Unset keeps today's behaviour byte
//!   for byte. A tag is refused at parse: it is not reproducible across
//!   hosts, and a daemon roll would silently move a recreated seat onto
//!   whatever the tag then names.
//! * **`extraMounts`** — a list of `{ "host": …, "container": … }` entries,
//!   always mounted **read-only** (a `rw` mode key is refused at parse, as
//!   is any other unexpected key). For large, library-free data only — PDKs,
//!   model files, caches; tools belong in a derived image.
//!
//! # The override image must carry the sealed control bundle
//!
//! A derived image is built `FROM` the released session image, so the
//! sealed control bundle (`defaults/docs/private-control-bundle.md`) under
//! [`bundle::CONTROL_ROOT`] comes along. [`verify_override_bundle`] proves
//! it before a container is created from the override: the image's own
//! manifest is read in a throwaway container (no network, no mounts,
//! read-only rootfs, capabilities dropped) and checked against the
//! constants this daemon enforces — protocol, control version, forced
//! policy, managed registration — and every sealed asset's recomputed
//! digest against what its manifest recorded. Anything missing or altered
//! refuses the image with a diagnostic that names the asset.
//!
//! Because the expectations are *this daemon's* constants, a daemon roll
//! that changes them fails a not-yet-rebuilt override image closed: the
//! operator rebuilds their derived image on the new base and updates the
//! digest pin. The version-skew reconciler additionally refuses to
//! recreate an override seat from the stock image, so a roll never
//! silently swaps one in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::private_workspace::bundle::{
    self, Manifest, CONTROL_PROTOCOL, CONTROL_ROOT, CONTROL_VERSION,
};
use crate::config_resolver;

/// One configured extra read-only bind mount (`accounts.session.extraMounts`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraMount {
    /// Absolute host path, mounted at `container`.
    pub host: PathBuf,
    /// Absolute container path; always read-only.
    pub container: PathBuf,
}

/// The resolved `accounts.session` settings (issue #14019). `Default` is
/// today's behaviour: the stock image and no extra mounts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionImageConfig {
    /// `accounts.session.image` — digest-pinned override; `None` = stock.
    pub image: Option<String>,
    /// `accounts.session.extraMounts` — always read-only.
    pub extra_mounts: Vec<ExtraMount>,
}

impl SessionImageConfig {
    /// Whether a session image override is configured at all.
    #[must_use]
    pub fn has_image_override(&self) -> bool {
        self.image.is_some()
    }
}

/// Read `.loom/config.json → accounts.session` (every tier,
/// [`config_resolver::resolve_effective_config`]) and validate it. A missing
/// block is today's behaviour; a malformed one is an **error**, never a
/// silent fallback to the stock image — a seat recreated from the stock
/// image because its override failed to parse would be exactly the
/// fail-open this issue exists to prevent.
///
/// # Errors
/// `image` is not a digest-pinned reference, or an `extraMounts` entry has a
/// wrong shape (non-object, missing/empty/relative path, or any key other
/// than `host`/`container` — there is no `rw` mode to configure).
pub fn read_session_image_config(repo_root: &Path) -> Result<SessionImageConfig> {
    let effective = config_resolver::resolve_effective_config(repo_root);
    let Some(block) = config_resolver::get_path(&effective, "accounts.session") else {
        return Ok(SessionImageConfig::default());
    };
    let image = match block.get("image") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(text)) => {
            validate_digest_pin(text)?;
            Some(text.clone())
        }
        Some(other) => bail!(
            "accounts.session.image must be a digest-pinned image reference string, got {other}"
        ),
    };
    let extra_mounts = parse_extra_mounts_config(block.get("extraMounts"))?;
    Ok(SessionImageConfig {
        image,
        extra_mounts,
    })
}

/// The configured session image must be pinned by digest
/// (`repo@sha256:<64 hex>`): a tag-only reference is refused, because it is
/// not reproducible across hosts and a daemon roll would silently move a
/// recreated seat onto whatever the tag names by then.
///
/// # Errors
/// Not a digest pin, the digest is not `sha256:`, or the digest is not 64
/// hex characters.
pub fn validate_digest_pin(image: &str) -> Result<()> {
    let Some((repo, digest)) = image.rsplit_once('@') else {
        bail!(
            "accounts.session.image {image:?} is not digest-pinned; name it as \
             <repo>@sha256:<64-hex digest> (`docker inspect --format \
             '{{index .RepoDigests 0}}' {image}` prints one)"
        );
    };
    if repo.is_empty() || repo.contains('@') {
        bail!("accounts.session.image {image:?} has an empty or malformed repository before the digest");
    }
    let Some(hex) = digest.strip_prefix("sha256:") else {
        bail!(
            "accounts.session.image {image:?} must pin a sha256 digest \
             (<repo>@sha256:<64-hex>), got digest {digest:?}"
        );
    };
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "accounts.session.image {image:?} has a malformed sha256 digest: it must be 64 hex \
             characters, got {hex:?}"
        );
    }
    Ok(())
}

/// Parse `accounts.session.extraMounts`. Entries are objects with **exactly**
/// `host` and `container`, both non-empty absolute paths — any other key
/// (a `rw`/`mode` switch in particular) is refused: extra mounts are always
/// read-only, and a silently ignored `rw` key would read as one.
///
/// # Errors
/// Not an array, an entry is not an object, or the shape is wrong.
pub fn parse_extra_mounts_config(value: Option<&serde_json::Value>) -> Result<Vec<ExtraMount>> {
    let Some(serde_json::Value::Array(items)) = value else {
        return match value {
            None | Some(serde_json::Value::Null) => Ok(Vec::new()),
            Some(other) => bail!(
                "accounts.session.extraMounts must be an array of {{host, container}} objects, \
                 got {other}"
            ),
        };
    };
    let mut mounts = Vec::with_capacity(items.len());
    for item in items {
        let serde_json::Value::Object(map) = item else {
            bail!("accounts.session.extraMounts entries must be objects, got {item}");
        };
        for key in map.keys() {
            if key != "host" && key != "container" {
                bail!(
                    "accounts.session.extraMounts entries accept only \"host\" and \
                     \"container\"; unexpected key {key:?} (extra mounts are always \
                     read-only — there is no rw mode to configure)"
                );
            }
        }
        let host = mount_path(map.get("host"), "host")?;
        let container = mount_path(map.get("container"), "container")?;
        mounts.push(ExtraMount {
            host: PathBuf::from(host),
            container: PathBuf::from(container),
        });
    }
    Ok(mounts)
}

fn mount_path(value: Option<&serde_json::Value>, key: &str) -> Result<String> {
    let Some(serde_json::Value::String(text)) = value else {
        bail!("accounts.session.extraMounts entries need a string {key:?} path");
    };
    if text.is_empty() {
        bail!("accounts.session.extraMounts entries need a non-empty {key:?} path");
    }
    if !text.starts_with('/') {
        bail!("accounts.session.extraMounts {key:?} path {text:?} must be absolute");
    }
    Ok(text.clone())
}

/// The `loom.extra-mounts` container label's JSON: what a container created
/// with extra mounts records so `session status` can report the mounts
/// actually in use. Never fails for [`ExtraMount`] (string fields only).
#[must_use]
pub fn extra_mounts_label_json(mounts: &[ExtraMount]) -> String {
    serde_json::to_string(mounts).expect("ExtraMount serializes to JSON")
}

/// Parse a container's `loom.extra-mounts` label for `session status`.
/// Lenient by design: a malformed label (never written by this daemon)
/// reports as no extra mounts rather than failing the status.
#[must_use]
pub fn parse_extra_mounts_label(text: &str) -> Vec<ExtraMount> {
    serde_json::from_str(text).unwrap_or_default()
}

/// Read-only probe seam: run `script` with `/bin/sh -c` in a throwaway
/// container from `image` — no network, no mounts, read-only rootfs, all
/// capabilities dropped — and return its stdout. [`super::session_lifecycle::ContainerRunner`]
/// inherits this so the bundle verification runs through the same Docker
/// seam every other session operation uses (and every test fake can fake).
pub trait BundleProbe {
    /// Run the probe script in a throwaway container from `image`.
    ///
    /// # Errors
    /// Docker could not run the probe, or the container failed.
    fn run_image_script(&self, _image: &str, _script: &str) -> Result<String> {
        bail!("this container runner cannot probe images")
    }
}

/// Marker separating the manifest JSON from the recomputed digest lines in
/// [`bundle_probe_script`]'s output.
pub const BUNDLE_PROBE_MARKER: &str = "__LOOM_BUNDLE_PROBE_V1__";

/// The absence marker `cat` leaves when the manifest is missing entirely.
pub const BUNDLE_PROBE_MISSING: &str = "__LOOM_BUNDLE_NO_MANIFEST__";

/// The script [`verify_override_bundle`] runs in the override image: print
/// the sealed manifest, then the sha256 of every other file in the bundle.
/// Both `find` and `sha256sum` are in the base image (Ubuntu coreutils).
#[must_use]
pub fn bundle_probe_script() -> String {
    format!(
        "cat {CONTROL_ROOT}/manifest.json 2>/dev/null || echo {BUNDLE_PROBE_MISSING}\n\
         echo {BUNDLE_PROBE_MARKER}\n\
         cd {CONTROL_ROOT} 2>/dev/null && find . -type f ! -path ./manifest.json \
         | LC_ALL=C sort | xargs -r sha256sum\n"
    )
}

/// Verify that the digest-pinned override `image` carries the sealed control
/// bundle this daemon enforces: its manifest matches this daemon's protocol,
/// control version, forced policy and managed registration, and every sealed
/// asset's content matches the digest its own manifest recorded. Called
/// before any container is created from the configured image — a daemon roll
/// that changed the enforced constants refuses a not-yet-rebuilt image.
///
/// # Errors
/// The probe failed, the image carries no (readable) manifest, the manifest
/// disagrees with this daemon's enforced constants, or a sealed asset is
/// missing or altered. The diagnostic names the offending asset or field.
pub fn verify_override_bundle(image: &str, probe: &dyn BundleProbe) -> Result<()> {
    let output = probe
        .run_image_script(image, &bundle_probe_script())
        .with_context(|| {
            format!("probing session image {image} for its sealed control bundle failed")
        })?;
    let (manifest_text, digests_text) = split_probe_output(&output).with_context(|| {
        format!(
            "session image {image} does not carry a readable sealed control bundle at \
             {CONTROL_ROOT}/manifest.json; a session image override must be built FROM the \
             released loom-worker-session image"
        )
    })?;
    let manifest: Manifest = serde_json::from_str(manifest_text.trim())
        .with_context(|| format!("session image {image}'s control manifest is unreadable"))?;
    assert_bundle_matches_daemon(image, &manifest)?;
    assert_sealed_assets_intact(image, &manifest, digests_text)?;
    Ok(())
}

/// Split [`bundle_probe_script`]'s output at [`BUNDLE_PROBE_MARKER`] into
/// `(manifest JSON, digest lines)`.
fn split_probe_output(output: &str) -> Result<(&str, &str)> {
    if output.contains(BUNDLE_PROBE_MISSING) {
        bail!("no manifest");
    }
    let Some((manifest, digests)) = output.split_once(BUNDLE_PROBE_MARKER) else {
        bail!("probe output ended before the marker");
    };
    Ok((manifest, digests))
}

/// The manifest must describe exactly the boundary this daemon enforces.
fn assert_bundle_matches_daemon(image: &str, manifest: &Manifest) -> Result<()> {
    if manifest.protocol != CONTROL_PROTOCOL {
        bail!(
            "session image {image} ships control protocol {:?}; this daemon enforces \
             {CONTROL_PROTOCOL:?}",
            manifest.protocol
        );
    }
    if manifest.control_version != CONTROL_VERSION {
        bail!(
            "session image {image} ships control bundle version {}; this daemon enforces \
             {CONTROL_VERSION} — rebuild the derived image FROM the base session image this \
             daemon's version ships, then update the digest pin",
            manifest.control_version
        );
    }
    let expected = bundle::expected_policy();
    if manifest.policy != expected {
        let mut keys: Vec<String> = manifest
            .policy
            .keys()
            .chain(expected.keys())
            .cloned()
            .collect();
        keys.sort();
        keys.dedup();
        let differing: Vec<String> = keys
            .iter()
            .filter(|key| manifest.policy.get(key.as_str()) != expected.get(key.as_str()))
            .map(|key| {
                format!(
                    "{key}={:?} (this daemon enforces {:?})",
                    manifest.policy.get(key.as_str()).unwrap_or(&String::new()),
                    expected.get(key.as_str())
                )
            })
            .collect();
        bail!(
            "session image {image}'s forced guard policy differs from this daemon's: {}",
            differing.join("; ")
        );
    }
    let registration = bundle::registration();
    if manifest.registration != registration {
        bail!(
            "session image {image}'s managed hook registration does not match this daemon's \
             (manifest: {:?}; enforced: {registration:?})",
            manifest.registration
        );
    }
    Ok(())
}

/// Every sealed asset named by the manifest must exist in the image with
/// exactly the digest the manifest recorded, and the manifest must seal
/// exactly the asset set this daemon knows.
fn assert_sealed_assets_intact(image: &str, manifest: &Manifest, digests_text: &str) -> Result<()> {
    let mut recomputed = BTreeMap::new();
    for line in digests_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((digest, path)) = line.split_once(char::is_whitespace) else {
            bail!("unreadable probe output line: {line:?}");
        };
        recomputed.insert(path.trim().trim_start_matches("./"), digest.trim());
    }
    let expected: Vec<&str> = bundle::sealed_assets().collect();
    for asset in &expected {
        let Some(sealed) = manifest.assets.get(*asset) else {
            bail!(
                "session image {image}'s control bundle is missing the sealed asset {asset}; \
                 the derived image was built from an altered or outdated bundle — rebuild it \
                 FROM the unaltered base session image"
            );
        };
        let Some(found) = recomputed.get(*asset) else {
            bail!(
                "session image {image} is missing its sealed asset {asset} although the \
                 manifest seals it; refusing the altered image"
            );
        };
        if found != sealed {
            bail!(
                "session image {image}'s sealed asset {asset} no longer matches the digest \
                 its own manifest recorded (altered layer); refusing the image"
            );
        }
    }
    for asset in manifest.assets.keys() {
        if !expected.contains(&asset.as_str()) {
            bail!(
                "session image {image}'s manifest seals an asset this daemon does not know: \
                 {asset}; rebuild the derived image on this daemon's base session image"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "session_image_tests.rs"]
mod tests;
