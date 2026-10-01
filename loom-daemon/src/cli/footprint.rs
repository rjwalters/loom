//! `loom-daemon footprint build | show | overlap` (#9784) — classified,
//! revision-pinned issue footprints over the #9783 cache. Shadow-only:
//! exposes evidence, changes no dispatch behavior.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use loom_daemon::context_cache::store::ArtifactStore;
use loom_daemon::footprint::{self, ClassifierIdentity};

#[derive(clap::Subcommand)]
pub(crate) enum FootprintCommand {
    /// Classify a cached context artifact into a footprint artifact.
    Build {
        /// The #9783 content key to classify.
        #[arg(long, value_name = "KEY")]
        key: String,

        /// Store root (same store the context cache lives in).
        #[arg(long, value_name = "DIR")]
        store: PathBuf,

        /// Git checkout holding the pinned source revision (enables
        /// create-vs-edit provenance). Omit → provenance unknown.
        #[arg(long, value_name = "DIR")]
        repo: Option<PathBuf>,

        /// Comma-separated Curator affected-file baseline.
        #[arg(long, value_name = "A,B", default_value = "")]
        curator_files: String,

        /// Classifier version to run (only `v1` exists).
        #[arg(long, value_name = "V", default_value = "v1")]
        classifier_version: String,
    },

    /// Print a persisted footprint artifact.
    Show {
        /// The #9783 content key.
        #[arg(long, value_name = "KEY")]
        key: String,

        /// Store root.
        #[arg(long, value_name = "DIR")]
        store: PathBuf,

        /// Classifier version the footprint was built with.
        #[arg(long, value_name = "V", default_value = "v1")]
        classifier_version: String,
    },

    /// Pair-overlap summary across two footprints: intended-edit overlap
    /// (collision signal) separated from context-only overlap.
    Overlap {
        /// Two content keys, comma-separated.
        #[arg(long, value_name = "K1,K2")]
        keys: String,

        /// Store root.
        #[arg(long, value_name = "DIR")]
        store: PathBuf,

        /// Classifier version the footprints were built with.
        #[arg(long, value_name = "V", default_value = "v1")]
        classifier_version: String,
    },
}

impl FootprintCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Build {
                key,
                store,
                repo,
                curator_files,
                classifier_version,
            } => run_build(&key, &store, repo.as_deref(), &curator_files, &classifier_version),
            Self::Show {
                key,
                store,
                classifier_version,
            } => {
                let s = ArtifactStore::at(store);
                let f = footprint::load(&s, &key, &classifier_version)?;
                println!("{}", serde_json::to_string_pretty(&f)?);
                Ok(())
            }
            Self::Overlap {
                keys,
                store,
                classifier_version,
            } => {
                let mut parts = keys.split(',');
                let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
                    bail!("--keys wants exactly two comma-separated content keys");
                };
                let s = ArtifactStore::at(store);
                let fa = footprint::load(&s, a, &classifier_version)?;
                let fb = footprint::load(&s, b, &classifier_version)?;
                println!("{}", serde_json::to_string_pretty(&footprint::pair_overlap(&fa, &fb))?);
                Ok(())
            }
        }
    }
}

fn run_build(
    key: &str,
    store: &Path,
    repo: Option<&Path>,
    curator_files: &str,
    classifier_version: &str,
) -> Result<()> {
    if classifier_version != "v1" {
        bail!("unknown classifier version {classifier_version:?} (only `v1` exists)");
    }
    let s = ArtifactStore::at(store.to_path_buf());
    let context = footprint::load_context(&s, key)?;
    let curator: Vec<String> = curator_files
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(String::from)
        .collect();
    let pinned_tree: BTreeMap<String, u32> = match repo {
        Some(r) => footprint::pinned_tree(r, &context.input.source_revision),
        None => BTreeMap::new(),
    };
    let f = footprint::build(
        &context,
        &curator,
        ClassifierIdentity {
            name: "rules".into(),
            version: classifier_version.to_string(),
        },
        &pinned_tree,
    );
    let path = footprint::persist(&s, &f)?;
    println!(
        "footprint {}: {} location(s), {} intended edit(s), {} context-only, {} unknown-provenance — {}",
        f.context_key,
        f.locations.len(),
        f.coverage.intended_edits,
        f.coverage.context_reads,
        f.coverage.unknown_provenance,
        path.display()
    );
    Ok(())
}
