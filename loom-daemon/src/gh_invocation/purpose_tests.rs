#![allow(clippy::unwrap_used)]
//! Source pins for #12310: which modules tag their reads `discovery`.

use std::path::{Path, PathBuf};

const TAG: &str = "purpose::discovery()";

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(src().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Each discovery site of the epic's table sets the tag.
#[test]
fn every_discovery_site_sets_the_tag() {
    for rel in [
        "step_dispatch/listing.rs",
        "step_dispatch/gather.rs",
        "step_dispatch/stage_entry.rs",
        "pr_planning/listing.rs",
        "work_finder/landed_prefilter.rs",
        "work_finder/forge.rs",
        "observability/queue_blocked.rs",
        "quarantine_reconciliation/listing.rs",
        "claim_reconciliation/open_pr_listing.rs",
        "intake_reconcile.rs",
        "intake_reconcile/singleton.rs",
        "stale_blocked/batch.rs",
    ] {
        assert!(read(rel).contains(TAG), "{rel} does not tag its listing as discovery");
    }
}

/// No gate or write module sets it: the claim, lease, merge and dispatch
/// guard reads stay `gate`.
#[test]
fn no_gate_module_sets_the_tag() {
    let mut files = Vec::new();
    rust_files(&src(), &mut files);
    for path in files {
        let rel = path
            .strip_prefix(src())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let gate = rel.starts_with("claim_reconciliation/merge_sequence")
            || rel == "sweep_registry/guards.rs"
            || rel.starts_with("merge_guard");
        if gate {
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                !text.contains(".discovery()") && !text.contains("purpose::discovery"),
                "{rel} is a gate read and must not be tagged discovery"
            );
        }
    }
}
