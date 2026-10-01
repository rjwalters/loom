//! `loom-daemon context fetch | status | export | import | replay` (#9783) —
//! the content-addressed retrieval cache surface. All logic lives in
//! [`loom_daemon::context_cache`]; this file is argument parsing and
//! orchestration only.
//!
//! Exit contract: 0 on success (a served cache hit or a completed retrieval,
//! including explicit `unavailable` sessions — those are recorded evidence,
//! not failures), 1 on argument/store/verification failure, 2 when the
//! provider could not answer at all and the caller should retry later.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

use loom_daemon::context_cache::{self, adapter, export, key, session, store};

#[derive(clap::Subcommand)]
pub(crate) enum ContextCommand {
    /// Derive the content key for an issue snapshot; serve from cache or run
    /// a bounded retrieval session and persist the artifact.
    Fetch {
        /// Issue number (with `--repo`, resolved via `gh`).
        #[arg(long, value_name = "N")]
        issue: Option<u32>,

        /// OWNER/REPO the issue lives in.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: String,

        /// Explicit title (offline mode; skips the gh lookup).
        #[arg(long, value_name = "TEXT")]
        title: Option<String>,

        /// Explicit body file (offline mode).
        #[arg(long, value_name = "PATH")]
        body_file: Option<PathBuf>,

        /// JSON file of requirement comments: [{"id":N,"revision":"…","body":"…"}].
        #[arg(long, value_name = "PATH")]
        requirements_file: Option<PathBuf>,

        /// The pinned immutable source revision retrieval binds to.
        #[arg(long, value_name = "SHA")]
        source_rev: String,

        /// Index identity/content manifest digest.
        #[arg(long, value_name = "ID", default_value = "unspecified")]
        index_id: String,

        /// Frozen query-policy version.
        #[arg(long, value_name = "V", default_value = "qp-v1")]
        query_policy: String,

        /// Adapter: `augment` (env-gated) or `fake` (tests/demo only).
        #[arg(long, value_name = "NAME", default_value = "augment")]
        adapter: String,

        /// Store root override (defaults to ~/.loom/context-cache).
        #[arg(long, value_name = "DIR")]
        store: Option<PathBuf>,
    },

    /// Report an artifact's presence/integrity without replaying it.
    Status {
        /// The content key (hex SHA-256).
        #[arg(long, value_name = "KEY")]
        key: String,

        /// Store root override.
        #[arg(long, value_name = "DIR")]
        store: Option<PathBuf>,
    },

    /// Write checksummed artifacts to a portable bundle.
    Export {
        /// Comma-separated keys; omit for the whole store.
        #[arg(long, value_name = "K1,K2")]
        keys: Option<String>,

        /// Bundle output path.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,

        /// Store root override.
        #[arg(long, value_name = "DIR")]
        store: Option<PathBuf>,
    },

    /// Import a bundle, verifying bundle + artifact integrity.
    Import {
        /// Bundle path.
        #[arg(long, value_name = "PATH")]
        bundle: PathBuf,

        /// Store root override.
        #[arg(long, value_name = "DIR")]
        store: Option<PathBuf>,
    },

    /// Print a saved artifact's responses — no provider, offline.
    Replay {
        /// The content key.
        #[arg(long, value_name = "KEY")]
        key: String,

        /// Store root override.
        #[arg(long, value_name = "DIR")]
        store: Option<PathBuf>,
    },

    /// Recover crashed-writer temp files and report store statistics.
    Recover {
        /// Store root override.
        #[arg(long, value_name = "DIR")]
        store: Option<PathBuf>,
    },
}

impl ContextCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Fetch {
                issue,
                repo,
                title,
                body_file,
                requirements_file,
                source_rev,
                index_id,
                query_policy,
                adapter: adapter_name,
                store: store_dir,
            } => run_fetch(FetchParams {
                issue,
                repo,
                title,
                body_file,
                requirements_file,
                source_rev,
                index_id,
                query_policy,
                adapter_name,
                store_dir,
            }),
            Self::Status {
                key,
                store: store_dir,
            } => {
                let s = open_store(store_dir.as_deref())?;
                match s.load(&key) {
                    Ok(Some(a)) => {
                        println!(
                            "{}: {} status={:?} results={} provider={}",
                            a.key,
                            a.completed_at,
                            a.session.status,
                            a.session.results.len(),
                            a.session.provider.name
                        );
                        Ok(())
                    }
                    Ok(None) => {
                        println!("{key}: absent");
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("{key}: CORRUPT: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Self::Export {
                keys,
                out,
                store: store_dir,
            } => {
                let s = open_store(store_dir.as_deref())?;
                let list: Vec<String> = keys
                    .map(|k| k.split(',').map(str::trim).map(str::to_string).collect())
                    .unwrap_or_default();
                let n = export::export(&s, &list, &out)?;
                println!("exported {n} artifact(s) to {}", out.display());
                Ok(())
            }
            Self::Import {
                bundle,
                store: store_dir,
            } => {
                let s = open_store(store_dir.as_deref())?;
                let n = export::import(&s, &bundle)?;
                println!("imported {n} artifact(s) from {}", bundle.display());
                Ok(())
            }
            Self::Replay {
                key,
                store: store_dir,
            } => {
                let s = open_store(store_dir.as_deref())?;
                let a = export::replay(&s, &key)?;
                println!("{}", serde_json::to_string_pretty(&a)?);
                Ok(())
            }
            Self::Recover { store: store_dir } => {
                let s = open_store(store_dir.as_deref())?;
                let removed = s.recover_orphans()?;
                let keys = s.keys()?;
                println!(
                    "recovered {removed} orphan tmp file(s); {} artifact(s) in store",
                    keys.len()
                );
                Ok(())
            }
        }
    }
}

fn open_store(dir: Option<&Path>) -> Result<store::ArtifactStore> {
    match dir {
        Some(d) => {
            std::fs::create_dir_all(d)?;
            Ok(store::ArtifactStore::at(d.to_path_buf()))
        }
        None => store::ArtifactStore::default_open(),
    }
}

/// Grouped `Fetch` args: `run_fetch` took 10 positional params, which clippy's
/// `too_many_arguments` (default max 7) rejects.
struct FetchParams {
    issue: Option<u32>,
    repo: String,
    title: Option<String>,
    body_file: Option<PathBuf>,
    requirements_file: Option<PathBuf>,
    source_rev: String,
    index_id: String,
    query_policy: String,
    adapter_name: String,
    store_dir: Option<PathBuf>,
}

fn run_fetch(params: FetchParams) -> Result<()> {
    let FetchParams {
        issue,
        repo,
        title,
        body_file,
        requirements_file,
        source_rev,
        index_id,
        query_policy,
        adapter_name,
        store_dir,
    } = params;
    // Resolve title/body: explicit inputs (offline/testable) win; otherwise
    // gh resolves the issue from the forge.
    let (title, body) = match (title, body_file) {
        (Some(t), Some(bf)) => (t, std::fs::read_to_string(bf)?),
        (Some(t), None) => (t, String::new()),
        (None, Some(bf)) => (String::new(), std::fs::read_to_string(bf)?),
        (None, None) => {
            let Some(n) = issue else {
                bail!(
                    "context fetch needs --issue N (with --repo) or explicit --title/--body-file"
                );
            };
            let out = std::process::Command::new("gh")
                .args([
                    "issue",
                    "view",
                    &n.to_string(),
                    "--repo",
                    &repo,
                    "--json",
                    "title,body",
                ])
                .output()?;
            if !out.status.success() {
                bail!("gh issue view failed: {}", String::from_utf8_lossy(&out.stderr));
            }
            let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
            (
                v.get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string(),
                v.get("body")
                    .and_then(|b| b.as_str())
                    .unwrap_or_default()
                    .to_string(),
            )
        }
    };
    let requirement_comments: Vec<key::RequirementComment> = match requirements_file {
        Some(f) => serde_json::from_str(&std::fs::read_to_string(f)?)?,
        None => Vec::new(),
    };
    // Build the adapter first: the cache key's adapter_version must be the
    // provider's actual identity version/schema (e.g. `direct-context-v1/
    // <pkg version>`), not the `--adapter` selection name — a provider
    // behavior change has to re-key the cache (#9848 review finding).
    let adapter: Box<dyn adapter::RetrievalAdapter> = match adapter_name.as_str() {
        "fake" => Box::new(adapter::FakeAdapter::default()),
        "augment" => Box::new(adapter::AugmentAdapter::from_env()),
        other => bail!("unknown adapter {other:?} (want `augment` or `fake`)"),
    };
    let mut input = key::InputSnapshot {
        schema_version: 1,
        repo,
        issue: issue.unwrap_or(0),
        title,
        body,
        requirement_comments,
        source_revision: source_rev,
        index_identity: index_id,
        query_policy_version: query_policy,
        adapter_version: adapter.identity().version,
    };
    input.canonicalize();

    // Provenance validation (#9783 AC4/AC5): every returned location is
    // validated against the pinned revision's actual file tree before it
    // can enter the artifact. The checkout the CLI runs in supplies the
    // pinned tree; a resolver failure is recorded as explicitly-unavailable
    // validation (Partial + coverage note), never a silent pass.
    let repo_checkout = std::env::current_dir()?;
    let provenance = match session::pinned_source_index(&repo_checkout, &input.source_revision) {
        Ok(index) => session::ProvenanceSource::Index(&index),
        Err(e) => session::ProvenanceSource::Unavailable(e.to_string()),
    };
    let s = open_store(store_dir.as_deref())?;
    let (key, artifact, reused) =
        context_cache::fetch(input, &s, adapter.as_ref(), adapter::Budget::default(), provenance)?;
    println!(
        "key {} {} (reused={reused}, status={:?}, results={}, calls={})",
        key,
        artifact.completed_at,
        artifact.session.status,
        artifact.session.results.len(),
        artifact.session.budget_report.calls
    );
    match artifact.session.status {
        session::SessionStatus::Unavailable => std::process::exit(2),
        _ => Ok(()),
    }
}
