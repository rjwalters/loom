//! Test fixtures for the fleet store: an in-memory forge serving a store
//! through the [`Transport`] seam. No network.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use anyhow::{bail, Result};
use sha2::{Digest, Sha256};

use crate::fleet_store::fetch::{Reply, Transport};
use crate::fleet_store::StoreLocation;

/// A fixture store location.
pub(crate) fn location() -> StoreLocation {
    StoreLocation {
        repo: "acme/fleet".to_string(),
        reference: "main".to_string(),
    }
}

/// A 40-hex fake object id for `data`.
pub(crate) fn oid(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))[..40].to_string()
}

/// A small but complete fixture store.
pub(crate) fn sample_files() -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert(
        "fleet/defaults.json".to_string(),
        r#"{"autonomous":{"roleRunner":{"enabled":true,"roles":["judge"]},"autoUpdate":{"settleSecs":3600}},"forge":{"githubApp":{"appId":"1"}}}"#
            .to_string(),
    );
    f.insert(
        "fleet/hosts/build-1/defaults.json".to_string(),
        r#"{"autonomous":{"workFinder":{"maxConcurrent":8},"roleRunner":{"roles":["judge","doctor"]}},"forge":{"githubApp":{"privateKeyPath":"/k.pem"}}}"#
            .to_string(),
    );
    f.insert(
        "fleet/hosts/build-1/local.json".to_string(),
        r#"{"observability":{"enabled":true}}"#.to_string(),
    );
    f.insert(
        "fleet/hosts/build-2/defaults.json".to_string(),
        r#"{"autonomous":{"workFinder":{"maxConcurrent":2}}}"#.to_string(),
    );
    f.insert(
        "fleet/state.yml".to_string(),
        "fleet:\n  state: running\nhosts:\n  build-2:\n    state: paused\n".to_string(),
    );
    f.insert(
        "repos.yml".to_string(),
        "root: /srv/src\nrepos:\n  - name: app\n    dir: app\n    fleet: true\n    fleet_priority: 5\n"
            .to_string(),
    );
    // Outside the contract: must not be fetched.
    f.insert("README.md".to_string(), "# store".to_string());
    f
}

/// An in-memory forge: one repo at one commit, with an ETag on the ref.
pub(crate) struct FakeForge {
    pub(crate) files: RefCell<BTreeMap<String, String>>,
    pub(crate) offline: Cell<bool>,
    /// Status to answer every request with instead of serving (e.g. 401).
    pub(crate) fail_status: Cell<Option<u16>>,
    pub(crate) calls: RefCell<Vec<String>>,
}

impl FakeForge {
    pub(crate) fn new(files: BTreeMap<String, String>) -> Self {
        Self {
            files: RefCell::new(files),
            offline: Cell::new(false),
            fail_status: Cell::new(None),
            calls: RefCell::new(Vec::new()),
        }
    }

    /// The commit id: a hash of the whole tree.
    pub(crate) fn commit(&self) -> String {
        let all: String = self
            .files
            .borrow()
            .iter()
            .map(|(k, v)| format!("{k}\0{v}\0"))
            .collect();
        oid(all.as_bytes())
    }

    pub(crate) fn blob_fetches(&self) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|c| c.contains("/git/blobs/"))
            .count()
    }
}

impl Transport for FakeForge {
    fn get(&self, api_path: &str, accept: Option<&str>, etag: Option<&str>) -> Result<Reply> {
        self.calls.borrow_mut().push(api_path.to_string());
        if self.offline.get() {
            bail!("network is unreachable");
        }
        if let Some(status) = self.fail_status.get() {
            return Ok(Reply {
                status,
                etag: None,
                body: r#"{"message":"Bad credentials"}"#.to_string(),
            });
        }
        let commit = self.commit();
        let files = self.files.borrow();
        if api_path == "repos/acme/fleet/commits/main" {
            assert_eq!(accept, Some("application/vnd.github.sha"));
            let tag = format!("\"{commit}\"");
            if etag == Some(tag.as_str()) {
                return Ok(Reply {
                    status: 304,
                    etag: Some(tag),
                    body: String::new(),
                });
            }
            return Ok(Reply {
                status: 200,
                etag: Some(tag),
                body: format!("{commit}\n"),
            });
        }
        if api_path == format!("repos/acme/fleet/git/trees/{commit}?recursive=1") {
            let mut tree: Vec<serde_json::Value> = vec![serde_json::json!({
                "path": "fleet", "type": "tree", "sha": oid(b"dir")
            })];
            for (p, c) in files.iter() {
                tree.push(serde_json::json!({"path": p, "type": "blob", "sha": oid(c.as_bytes())}));
            }
            return Ok(Reply {
                status: 200,
                etag: None,
                body: serde_json::json!({"sha": commit, "tree": tree, "truncated": false})
                    .to_string(),
            });
        }
        if let Some(sha) = api_path.strip_prefix("repos/acme/fleet/git/blobs/") {
            use base64::{engine::general_purpose, Engine as _};
            let Some(content) = files.values().find(|c| oid(c.as_bytes()) == sha) else {
                return Ok(Reply {
                    status: 404,
                    etag: None,
                    body: r#"{"message":"Not Found"}"#.to_string(),
                });
            };
            // GitHub wraps base64 at 60 columns; the reader must cope.
            let encoded = general_purpose::STANDARD.encode(content.as_bytes());
            let wrapped: Vec<String> = encoded
                .as_bytes()
                .chunks(60)
                .map(|c| String::from_utf8_lossy(c).to_string())
                .collect();
            return Ok(Reply {
                status: 200,
                etag: None,
                body: serde_json::json!({"content": wrapped.join("\n"), "encoding": "base64"})
                    .to_string(),
            });
        }
        Ok(Reply {
            status: 404,
            etag: None,
            body: r#"{"message":"Not Found"}"#.to_string(),
        })
    }
}

/// A snapshot of `files` (contract paths only), without going through a fetch.
pub(crate) fn snapshot_of(files: &BTreeMap<String, String>) -> crate::fleet_store::fetch::Snapshot {
    use crate::fleet_store::fetch::{Manifest, Snapshot};
    let contract: BTreeMap<String, String> = files
        .iter()
        .filter(|(p, _)| crate::fleet_store::is_contract_path(p))
        .map(|(p, c)| (p.clone(), c.clone()))
        .collect();
    Snapshot {
        manifest: Manifest {
            version: 1,
            repo: "acme/fleet".to_string(),
            reference: "main".to_string(),
            commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
            etag: None,
            confirmed_at: chrono::Utc::now(),
            files: contract
                .iter()
                .map(|(p, c)| (p.clone(), oid(c.as_bytes())))
                .collect(),
        },
        files: contract
            .into_iter()
            .map(|(p, c)| (p, c.into_bytes()))
            .collect(),
    }
}
