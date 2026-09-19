use super::*;

const TWO_FILES: &str = "\
diff --git a/Cargo.lock b/Cargo.lock
index 1111111..2222222 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -100,7 +100,7 @@ name = \"serde\"
-version = \"1.0.1\"
+version = \"1.0.2\"
diff --git a/.github/workflows/ci.yml b/.github/workflows/ci.yml
index 3333333..4444444 100644
--- a/.github/workflows/ci.yml
+++ b/.github/workflows/ci.yml
@@ -12,7 +12,7 @@ jobs:
-      - uses: actions/checkout@v4
+      - uses: actions/checkout@v5
";

#[test]
fn splits_each_file_and_keeps_its_own_hunks() {
    let map = split_by_file(TWO_FILES);
    assert_eq!(map.len(), 2);
    assert!(map["Cargo.lock"].contains("+version = \"1.0.2\""));
    assert!(!map["Cargo.lock"].contains("actions/checkout"));
    assert!(map[".github/workflows/ci.yml"].contains("+      - uses: actions/checkout@v5"));
}

#[test]
fn paths_are_sorted_and_deduped() {
    assert_eq!(paths(TWO_FILES), [".github/workflows/ci.yml", "Cargo.lock"]);
}

#[test]
fn a_new_file_takes_its_path_from_the_plus_header() {
    let d = "\
diff --git a/uv.lock b/uv.lock
new file mode 100644
--- /dev/null
+++ b/uv.lock
@@ -0,0 +1,1 @@
+version = 1
";
    let map = split_by_file(d);
    assert!(map.contains_key("uv.lock"), "got {:?}", map.keys());
}

#[test]
fn a_deleted_file_falls_back_to_the_minus_header() {
    let d = "\
diff --git a/yarn.lock b/yarn.lock
deleted file mode 100644
--- a/yarn.lock
+++ /dev/null
@@ -1,1 +0,0 @@
-# yarn lockfile v1
";
    let map = split_by_file(d);
    assert!(map.contains_key("yarn.lock"), "got {:?}", map.keys());
}

#[test]
fn a_mode_only_change_still_reports_its_path() {
    let d = "\
diff --git a/scripts/run.sh b/scripts/run.sh
old mode 100644
new mode 100755
";
    assert_eq!(paths(d), ["scripts/run.sh"]);
}

#[test]
fn an_empty_diff_yields_nothing() {
    assert!(split_by_file("").is_empty());
    assert!(paths("").is_empty());
}

#[test]
fn a_minus_line_inside_a_hunk_is_content_not_a_header() {
    // The trap: a lockfile hunk whose removed line happens to start with
    // `--- `. It must stay body, not silently rename the file being parsed.
    let d = "\
diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1,3 +1,3 @@
---- not a header, just a removed line
++++ nor is this
";
    let map = split_by_file(d);
    assert_eq!(map.len(), 1);
    assert!(map.contains_key("Cargo.lock"), "got {:?}", map.keys());
    assert!(map["Cargo.lock"].contains("--- not a header"));
}
