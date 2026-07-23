//! Conformance test: the Rust epic transition table vs. the authoritative
//! Python state-machine model (epic #3842 Phase 4, #3873).
//!
//! # Why this test derives its expectation (not hardcodes it)
//!
//! Per prior Judge feedback (#3867): a conformance test that hardcodes a
//! mirrored copy of the graph silently drifts and fails to catch the exact
//! divergence a conformance test exists to catch. So this test **derives** its
//! expectation by invoking the authoritative Python model
//! (`loom-tools/src/loom_tools/state_machine.py --json`), parsing the emitted
//! epic sub-graph (states, edges, barriers), and asserting the Rust
//! [`loom_daemon::epic_state::epic_transition_table`] conforms to it.
//!
//! If the Python model changes an epic edge, a role, a barrier string, or the
//! `creates_issues` flag, this test fails until the Rust table is brought back
//! into agreement — real drift is caught mechanically.
//!
//! # Gating
//!
//! The test shells out to `python3`. On a host without it (or where the model
//! module can't be imported), the test prints a skip message and returns
//! rather than failing — but in this repo's normal dev/CI environment
//! (`python3` present, `loom-tools/src` on disk) it runs and asserts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

use loom_daemon::epic_state::{epic_state_ids, epic_transition_table, EpicState};

/// Repo root = the loom-daemon crate dir's parent.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon has a parent dir")
        .to_path_buf()
}

/// A minimally-typed view of one Python edge, extracted from the `--json` dump.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DerivedEdge {
    src: String,
    dst: String,
    role: String,
    barrier: String,
    creates_issues: bool,
}

/// Invoke the Python model's JSON exporter and return `(epic_state_ids,
/// epic_edges, terminal_state_ids)` for the epic sub-graph, or `None` when
/// `python3` / the model is unavailable (skip path).
fn derive_python_epic_subgraph() -> Option<(BTreeSet<String>, Vec<DerivedEdge>, BTreeSet<String>)> {
    let root = repo_root();
    let pythonpath = root.join("loom-tools").join("src");

    let output = Command::new("python3")
        .arg("-m")
        .arg("loom_tools.state_machine")
        .arg("--json")
        .env("PYTHONPATH", &pythonpath)
        .current_dir(&root)
        .output()
        .ok()?;

    if !output.status.success() {
        eprintln!(
            "SKIP: `python3 -m loom_tools.state_machine --json` failed \
             (status {:?}); stderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;

    // Epic-lane states.
    let mut epic_states: BTreeSet<String> = BTreeSet::new();
    let mut terminals: BTreeSet<String> = BTreeSet::new();
    for s in json["states"].as_array()? {
        if s["lane"].as_str() == Some("epic") {
            let id = s["id"].as_str()?.to_string();
            if s["terminal"].as_bool() == Some(true) {
                terminals.insert(id.clone());
            }
            epic_states.insert(id);
        }
    }

    // Epic sub-graph edges: BOTH endpoints are epic-lane states. This scopes to
    // the supervisor's own transitions among the five derived states and
    // deliberately excludes the lane-entry edge `new → epic:needs_decomp` (src
    // `new` is not an epic state) — the Rust supervisor begins at needs_decomp.
    let mut edges: Vec<DerivedEdge> = Vec::new();
    for t in json["transitions"].as_array()? {
        let src = t["src"].as_str()?.to_string();
        let dst = t["dst"].as_str()?.to_string();
        if epic_states.contains(&src) && epic_states.contains(&dst) {
            edges.push(DerivedEdge {
                src,
                dst,
                role: t["role"].as_str()?.to_string(),
                barrier: t["barrier"].as_str().unwrap_or("").to_string(),
                creates_issues: t["creates_issues"].as_bool().unwrap_or(false),
            });
        }
    }

    Some((epic_states, edges, terminals))
}

#[test]
fn rust_epic_states_conform_to_python_model() {
    let Some((py_states, _edges, py_terminals)) = derive_python_epic_subgraph() else {
        return; // skip: python3/model unavailable
    };

    // The five Rust EpicState ids must exactly equal the Python epic-lane states.
    let rust_states: BTreeSet<String> = epic_state_ids().iter().map(|s| s.to_string()).collect();
    assert_eq!(
        rust_states, py_states,
        "Rust EpicState ids diverge from the Python epic-lane states"
    );
    assert_eq!(rust_states.len(), 5, "expected exactly five derived epic states");

    // Terminal conformance: Python's terminal epic state(s) must match Rust's
    // `is_terminal` (only epic:done).
    let rust_terminals: BTreeSet<String> = [
        EpicState::NeedsDecomp,
        EpicState::Designed,
        EpicState::Active,
        EpicState::PhaseJoin,
        EpicState::Done,
    ]
    .into_iter()
    .filter(|s| s.is_terminal())
    .map(|s| s.as_state_id().to_string())
    .collect();
    assert_eq!(
        rust_terminals, py_terminals,
        "Rust terminal epic state(s) diverge from the Python model"
    );
    assert_eq!(
        rust_terminals,
        BTreeSet::from(["epic:done".to_string()]),
        "epic:done must be the sole terminal derived state"
    );
}

#[test]
fn rust_epic_edges_conform_to_python_model() {
    let Some((_states, py_edges, _terminals)) = derive_python_epic_subgraph() else {
        return; // skip: python3/model unavailable
    };

    // Key edges by (src, dst) for order-independent comparison.
    let py_by_key: BTreeMap<(String, String), &DerivedEdge> = py_edges
        .iter()
        .map(|e| ((e.src.clone(), e.dst.clone()), e))
        .collect();

    let rust_edges = epic_transition_table();
    let rust_by_key: BTreeMap<(String, String), _> = rust_edges
        .iter()
        .map(|e| ((e.src.to_string(), e.dst.to_string()), e))
        .collect();

    // Same set of (src, dst) edges.
    let py_keys: BTreeSet<_> = py_by_key.keys().cloned().collect();
    let rust_keys: BTreeSet<_> = rust_by_key.keys().cloned().collect();
    assert_eq!(
        rust_keys, py_keys,
        "Rust epic transition edges (src->dst) diverge from the Python model.\n\
         Python: {py_keys:?}\nRust:   {rust_keys:?}"
    );

    // Field-by-field conformance for every edge: role, barrier, creates_issues.
    for (key, py) in &py_by_key {
        let rust = rust_by_key.get(key).expect("edge key present in both");
        assert_eq!(
            rust.role, py.role,
            "edge {}->{}: role differs (rust={:?}, python={:?})",
            key.0, key.1, rust.role, py.role
        );
        assert_eq!(
            rust.barrier, py.barrier,
            "edge {}->{}: barrier differs (rust={:?}, python={:?})",
            key.0, key.1, rust.barrier, py.barrier
        );
        assert_eq!(
            rust.creates_issues, py.creates_issues,
            "edge {}->{}: creates_issues differs (rust={}, python={})",
            key.0, key.1, rust.creates_issues, py.creates_issues
        );
    }

    // Sanity: the epic sub-graph has exactly five intra-lane edges.
    assert_eq!(rust_edges.len(), 5, "expected five epic transition-table edges");
}

#[test]
fn phase_join_barrier_conforms_to_python_model() {
    let Some((_states, py_edges, _terminals)) = derive_python_epic_subgraph() else {
        return; // skip: python3/model unavailable
    };

    // Every Python edge touching epic:phase_join must declare a non-empty
    // barrier (the barrier-hygiene invariant), and the Rust table must carry the
    // identical barrier string on the same edge.
    let rust_edges = epic_transition_table();
    let mut checked = 0;
    for py in &py_edges {
        let touches_join = py.src == "epic:phase_join" || py.dst == "epic:phase_join";
        if !touches_join {
            continue;
        }
        assert!(
            !py.barrier.is_empty(),
            "python phase-boundary edge {}->{} must declare a barrier",
            py.src,
            py.dst
        );
        let rust = rust_edges
            .iter()
            .find(|e| e.src == py.src && e.dst == py.dst)
            .unwrap_or_else(|| {
                panic!("rust table missing phase-boundary edge {}->{}", py.src, py.dst)
            });
        assert_eq!(
            rust.barrier, py.barrier,
            "phase-boundary edge {}->{}: barrier mismatch",
            py.src, py.dst
        );
        checked += 1;
    }
    // active→phase_join, phase_join→active, phase_join→done = 3 boundary edges.
    assert_eq!(checked, 3, "expected three phase-join barrier edges");
}
