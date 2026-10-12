use std::collections::BTreeMap;

use super::*;
use crate::fleet_store::test_support::{snapshot_of, FakeForge};

/// A stand-in renderer: writes `fleet.json` as `{"source": <fleet.yml>}`,
/// and `repos.yml` unchanged, through a helper module beside it (so the
/// whole `scripts/` directory must be there). Fails on a `fleet.yml` that
/// says `bad`.
const RENDER_PY: &str = r#"
import json, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helper
root = sys.argv[sys.argv.index("--root") + 1]
src = open(os.path.join(root, "fleet.yml")).read()
if "bad" in src:
    print("render: fleet.yml:1: bad", file=sys.stderr)
    sys.exit(1)
open(os.path.join(root, "fleet.json"), "w").write(helper.dump(src))
open(os.path.join(root, "repos.yml"), "w").write("repos: []\n")
os.makedirs(os.path.join(root, "fleet", "hosts", "h"), exist_ok=True)
open(os.path.join(root, "fleet", "hosts", "h", "local.json"), "w").write("{}\n")
"#;

const HELPER_PY: &str =
    "import json\ndef dump(src):\n    return json.dumps({'source': src}) + '\\n'\n";

fn store_files(with_renderer: bool) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert("fleet.yml".to_string(), "a: 1\n".to_string());
    f.insert("fleet.json".to_string(), "{\"source\": \"a: 1\\n\"}\n".to_string());
    f.insert("repos.yml".to_string(), "repos: []\n".to_string());
    f.insert("README.md".to_string(), "# store\n".to_string());
    f.insert("schema/fleet.schema.json".to_string(), "{}\n".to_string());
    if with_renderer {
        f.insert(RENDERER.to_string(), RENDER_PY.to_string());
        f.insert("scripts/helper.py".to_string(), HELPER_PY.to_string());
    }
    f
}

fn setup(with_renderer: bool) -> (FakeForge, Snapshot) {
    let files = store_files(with_renderer);
    let forge = FakeForge::new(files.clone());
    let mut snapshot = snapshot_of(&files);
    snapshot.manifest.commit = forge.commit();
    (forge, snapshot)
}

fn have_python() -> bool {
    python_ready(PYTHON, "json")
}

#[test]
fn load_reads_fleet_yml_and_its_blob_sha_at_the_snapshot_commit() {
    let (forge, snapshot) = setup(false);
    let source = load(&forge, "acme/fleet", &snapshot).unwrap();
    assert_eq!(source.text, "a: 1\n");
    assert_eq!(&source.sha, &source.tree["fleet.yml"]);
    let change = source.change("a: 2\n".to_string());
    assert_eq!(
        (change.path.as_str(), change.before.as_deref(), change.after.as_str()),
        ("fleet.yml", Some("a: 1\n"), "a: 2\n")
    );
}

#[test]
fn a_store_without_fleet_yml_takes_no_proposal() {
    let mut files = store_files(false);
    files.remove("fleet.yml");
    let forge = FakeForge::new(files.clone());
    let mut snapshot = snapshot_of(&files);
    snapshot.manifest.commit = forge.commit();
    let err = load(&forge, "acme/fleet", &snapshot).unwrap_err();
    assert!(err.to_string().contains("the store has no fleet.yml"), "{err}");
}

#[test]
fn the_renders_an_edit_changes_come_with_it() {
    if !have_python() {
        eprintln!("skipped: no python3");
        return;
    }
    let (forge, snapshot) = setup(true);
    let source = load(&forge, "acme/fleet", &snapshot).unwrap();
    let Rendered::Files(files) =
        render_with(&forge, &source, &snapshot, "a: 2\n", PYTHON, "json").unwrap()
    else {
        panic!("rendered");
    };
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    // repos.yml rendered the same: not proposed. A new file is created.
    assert_eq!(paths, ["fleet.json", "fleet/hosts/h/local.json"]);
    assert_eq!(files[0].after, "{\"source\": \"a: 2\\n\"}\n");
    assert_eq!(files[0].before_sha.as_ref(), source.tree.get("fleet.json"));
    assert!(files[0].before.is_some());
    assert_eq!((files[1].before.as_deref(), files[1].before_sha.as_deref()), (None, None));
}

#[test]
fn a_renderer_that_refuses_the_edit_is_an_error() {
    if !have_python() {
        eprintln!("skipped: no python3");
        return;
    }
    let (forge, snapshot) = setup(true);
    let source = load(&forge, "acme/fleet", &snapshot).unwrap();
    let err = render_with(&forge, &source, &snapshot, "a: bad\n", PYTHON, "json").unwrap_err();
    assert!(err.to_string().contains("fleet.yml:1: bad"), "{err}");
}

#[test]
fn no_renderer_or_no_python_skips_with_the_reason() {
    let (forge, snapshot) = setup(false);
    let source = load(&forge, "acme/fleet", &snapshot).unwrap();
    let Rendered::Skipped(why) = render(&forge, &source, &snapshot, "a: 2\n").unwrap() else {
        panic!("skipped");
    };
    assert!(why.contains("no scripts/render.py"), "{why}");

    let (forge, snapshot) = setup(true);
    let source = load(&forge, "acme/fleet", &snapshot).unwrap();
    let skipped =
        render_with(&forge, &source, &snapshot, "a: 2\n", "no-such-python-loom", "json").unwrap();
    assert!(matches!(skipped, Rendered::Skipped(w) if w.contains("not available")));
}

#[test]
fn store_paths_that_could_leave_the_scratch_directory_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    for bad in ["../x", "a/../../x", "", "a//b"] {
        assert!(write_file(dir.path(), bad, b"x").is_err(), "{bad}");
    }
    write_file(dir.path(), "scripts/ok.py", b"x").unwrap();
    assert_eq!(files_under(dir.path()).unwrap(), ["scripts/ok.py"]);
}
