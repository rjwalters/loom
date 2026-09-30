//! Pins the four daemon-implemented gates' `G` sets to the Rust source their
//! verdicts can actually reach (see "Gates implemented inside the daemon
//! binary" in the `inputs` module docs).
//!
//! These specs used to carry `loom-daemon/**`, which was sound but made them
//! stale on almost every base move (#9543/#9544). The narrowed globs are only
//! sound while they cover everything the checker's code reaches, and a
//! hand-kept list goes stale the moment a checker imports a new module. So
//! this file recomputes that reach from the source on every run: it follows
//! `mod x;`, `crate::…`, `loom_daemon::…` and `super::…` paths out of each
//! checker's entry point, transitively, and fails when any file it reaches —
//! or any file it `include_str!`s — is outside the spec's `G`. Adding an import
//! to a checker therefore fails HERE, at PR time, instead of silently letting
//! the freshness guard trust a verdict whose new input it cannot see.
//!
//! The walk is deliberately file-granular and over-inclusive (a whole file is
//! reached if anything in it is named; inline test modules count), because
//! over-listing only makes the guard refuse more often.

use super::*;
use regex::Regex;
use std::path::{Path, PathBuf};

/// Repo-relative prefix of the daemon crate's sources.
const SRC: &str = "loom-daemon/src/";

/// Build inputs every daemon-implemented gate must list: they change the
/// binary without touching any `.rs` file.
const BUILD_INPUTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    ".cargo/config.toml",
    "loom-daemon/Cargo.toml",
    "loom-daemon/build.rs",
];

/// `main()` and the CLI dispatch it delegates to — on every daemon
/// subcommand's path regardless of which subcommand it is.
const ENTRY_CHAIN: &[&str] = &[
    "loom-daemon/src/main.rs",
    "loom-daemon/src/daemon_service.rs",
];

/// Where a walk starts.
enum Root {
    /// A whole source file (path relative to `loom-daemon/src/`).
    File(&'static str),
    /// One function in a file that also hosts unrelated commands — walking the
    /// whole file would drag in every other command's dependencies.
    Fn(&'static str, &'static str),
}

/// One daemon-implemented gate: where its code starts, and the symbol that
/// dispatch code must name to route to it.
struct Surface {
    component: &'static str,
    roots: &'static [Root],
    /// Every bin-side file naming this symbol is on the dispatch path.
    entry_symbol: &'static str,
}

const SURFACES: &[Surface] = &[
    Surface {
        component: "Shell Budget Ratchet",
        roots: &[Root::File("cli/shell_budget.rs")],
        entry_symbol: "ShellBudgetArgs",
    },
    Surface {
        component: ".gitignore Convergence Check",
        roots: &[Root::Fn(
            "cli/misc_cmds.rs",
            "handle_update_gitignore_command",
        )],
        entry_symbol: "handle_update_gitignore_command",
    },
    Surface {
        component: "Secret Scan",
        roots: &[Root::File("cli/secret_scan_cmd.rs")],
        entry_symbol: "SecretScanArgs",
    },
    Surface {
        component: "MCP Guard Wiring Contract",
        roots: &[Root::File("cli/check_guard_wiring.rs")],
        entry_symbol: "CheckGuardWiringArgs",
    },
];

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Case-exact existence: macOS's default filesystem would otherwise resolve
/// `crate::Cli` to `cli/mod.rs`-shaped paths that Linux CI cannot see.
fn exists_exact(rel: &str) -> bool {
    let full = src_root().join(rel);
    let (Some(parent), Some(name)) = (full.parent(), full.file_name()) else {
        return false;
    };
    full.is_file()
        && std::fs::read_dir(parent)
            .map(|rd| rd.flatten().any(|e| e.file_name() == name))
            .unwrap_or(false)
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(src_root().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// Production source only: whole-line comments removed (so doc links such as
/// ``[`crate::points_marker`]`` are not mistaken for dependencies), and every
/// `#[cfg(test)]` module — inline or `mod x;` — cut out. Test code never
/// reaches the binary a gate runs, and an inline `mod tests { use super::*; }`
/// would otherwise resolve `super` one level too high.
fn code_of(text: &str) -> String {
    let code = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    strip_test_modules(&code)
}

fn strip_test_modules(code: &str) -> String {
    let head = Regex::new(
        r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z0-9_]+\s*([;{])",
    )
    .unwrap();
    let mut out = String::new();
    let mut rest = code;
    while let Some(cap) = head.captures(rest) {
        let whole = cap.get(0).expect("match");
        out.push_str(&rest[..whole.start()]);
        if &cap[1] == ";" {
            rest = &rest[whole.end()..];
            continue;
        }
        // Skip the brace-balanced inline module body.
        let body = &rest[whole.end() - 1..];
        let end = block_end(body).expect("an inline #[cfg(test)] module with unbalanced braces");
        rest = &body[end..];
    }
    out.push_str(rest);
    out
}

fn is_bin(rel: &str) -> bool {
    rel == "main.rs"
        || rel.starts_with("cli/")
        || rel == "daemon_service.rs"
        || rel.starts_with("daemon_service/")
}

fn crate_root(rel: &str) -> &'static str {
    if is_bin(rel) {
        "main.rs"
    } else {
        "lib.rs"
    }
}

/// The module path a file defines (`shell_budget/churn.rs` → `[shell_budget,
/// churn]`, `init/mod.rs` → `[init]`, `main.rs` → `[]`).
fn module_of(rel: &str) -> Vec<String> {
    let dir = if rel == "main.rs" || rel == "lib.rs" {
        ""
    } else if let Some(d) = rel.strip_suffix("/mod.rs") {
        d
    } else {
        rel.strip_suffix(".rs").unwrap_or(rel)
    };
    dir.split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_module_segment(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The deepest existing module file along `parts` (items such as `Cli` or
/// `update_gitignore` stop the descent at the module that defines them).
fn module_file(parts: &[String]) -> Option<String> {
    let mods: Vec<&str> = parts
        .iter()
        .map(String::as_str)
        .take_while(|s| is_module_segment(s))
        .collect();
    (1..=mods.len()).rev().find_map(|n| {
        let p = mods[..n].join("/");
        [format!("{p}.rs"), format!("{p}/mod.rs")]
            .into_iter()
            .find(|c| exists_exact(c))
    })
}

/// Resolve a `crate` / `loom_daemon` / `super` / relative path appearing in
/// `from` to the file that defines its deepest module.
fn resolve(from: &str, segs: &[String]) -> Option<String> {
    let first = segs.first()?;
    match first.as_str() {
        "loom_daemon" => Some(module_file(&segs[1..]).unwrap_or_else(|| "lib.rs".into())),
        "crate" => {
            let hit = module_file(&segs[1..]);
            Some(match hit {
                // In the binary crate, `crate::` names bin-side modules only; an
                // item such as `crate::Cli` lives in `main.rs`.
                Some(f) if is_bin(from) == is_bin(&f) => f,
                _ => crate_root(from).to_string(),
            })
        }
        "super" | "self" => {
            let mut base = module_of(from);
            let mut rest = segs;
            while let Some(s) = rest.first() {
                match s.as_str() {
                    "super" => {
                        base.pop();
                    }
                    "self" => {}
                    _ => break,
                }
                rest = &rest[1..];
            }
            base.extend(rest.iter().cloned());
            Some(module_file(&base).unwrap_or_else(|| crate_root(from).to_string()))
        }
        _ => {
            // A path relative to the file's own module (`telemetry_live::X`).
            let mut own = module_of(from);
            own.extend(segs.iter().cloned());
            module_file(&own)
        }
    }
}

struct Patterns {
    path: Regex,
    child_mod: Regex,
    include: Regex,
    flatten: Regex,
}

fn patterns() -> Patterns {
    Patterns {
        path: Regex::new(
            r"\b(?:crate|loom_daemon|super)(?:(?:::[A-Za-z_][A-Za-z0-9_]*)+(?:::\{[^}]*\})?|::\{[^}]*\})",
        )
        .unwrap(),
        child_mod: Regex::new(
            r#"(?m)^\s*(?:#\[path\s*=\s*"([^"]+)"\]\s*)?(?:pub(?:\([^)]*\))?\s+)?mod\s+([a-z_][a-z0-9_]*)\s*;"#,
        )
        .unwrap(),
        include: Regex::new(r#"include_(?:str|bytes)!\(\s*"([^"]+)""#).unwrap(),
        flatten: Regex::new(
            r"#\[command\(flatten\)\]\s*(?:#\[[^\]]*\]\s*)*[A-Z][A-Za-z0-9_]*\(\s*([A-Za-z0-9_:]+)\s*\)",
        )
        .unwrap(),
    }
}

/// Every file a path token in `code` (written in `from`) names.
fn named_files(pats: &Patterns, from: &str, code: &str) -> Vec<String> {
    let mut out = Vec::new();
    for m in pats.path.find_iter(code) {
        let tok = m.as_str();
        let (head, group) = match tok.find("::{") {
            Some(i) => (&tok[..i], Some(&tok[i + 3..tok.len() - 1])),
            None => (tok, None),
        };
        let segs: Vec<String> = head.split("::").map(str::to_string).collect();
        out.extend(resolve(from, &segs));
        for item in group.into_iter().flat_map(|g| g.split(',')) {
            let first = item
                .trim()
                .split(|c: char| c == ':' || c.is_whitespace())
                .next();
            if let Some(name) = first.filter(|n| !n.is_empty() && *n != "self") {
                let mut full = segs.clone();
                full.push(name.to_string());
                out.extend(resolve(from, &full));
            }
        }
    }
    out
}

/// The body of `fn name` in `text`, braces included.
fn fn_body(text: &str, name: &str) -> String {
    let at = text
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("fn {name} not found — was the handler renamed?"));
    let open = at + text[at..].find('{').expect("fn has a body");
    let end = block_end(&text[open..]).unwrap_or_else(|| panic!("unbalanced braces in fn {name}"));
    text[open..open + end].to_string()
}

/// Byte length of the `{ … }` block `s` starts with, skipping braces inside
/// string, raw-string and char literals (`format!("{x}")`, `'{'`).
fn block_end(s: &str) -> Option<usize> {
    let cs: Vec<(usize, char)> = s.char_indices().collect();
    let at = |k: usize| cs.get(k).map(|&(_, c)| c);
    let mut depth = 0usize;
    let mut k = 0;
    while k < cs.len() {
        let (i, c) = cs[k];
        match c {
            '"' => {
                k += 1;
                while k < cs.len() && cs[k].1 != '"' {
                    k += if cs[k].1 == '\\' { 2 } else { 1 };
                }
            }
            'r' if matches!(at(k + 1), Some('"' | '#'))
                && (k == 0 || !(cs[k - 1].1.is_alphanumeric() || cs[k - 1].1 == '_')) =>
            {
                let mut hashes = 0;
                let mut j = k + 1;
                while at(j) == Some('#') {
                    hashes += 1;
                    j += 1;
                }
                if at(j) == Some('"') {
                    j += 1;
                    'raw: while j < cs.len() {
                        if cs[j].1 == '"' && (1..=hashes).all(|h| at(j + h) == Some('#')) {
                            j += hashes;
                            break 'raw;
                        }
                        j += 1;
                    }
                    k = j;
                }
            }
            '\'' => {
                // A char literal (`'{'`, `'\''`), not a lifetime (`'a`).
                if at(k + 1) == Some('\\') {
                    let mut j = k + 2;
                    while j < cs.len() && cs[j].1 != '\'' {
                        j += 1;
                    }
                    k = j;
                } else if at(k + 2) == Some('\'') {
                    k += 2;
                }
            }
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        k += 1;
    }
    None
}

/// Is this a test-only file? Its `include_str!`s never reach the binary.
fn is_test_file(rel: &str) -> bool {
    rel.ends_with("tests.rs") || rel.contains("/tests/") || rel.contains("_tests/")
}

/// The files a checker reaches (relative to `loom-daemon/src/`) and the
/// repo-relative paths its non-test code `include_str!`s.
fn reach(roots: &[Root]) -> (BTreeSet<String>, BTreeSet<String>) {
    let pats = patterns();
    let mut seen = BTreeSet::new();
    let mut walked = BTreeSet::new();
    let mut includes = BTreeSet::new();
    let mut stack: Vec<(String, Option<&str>)> = roots
        .iter()
        .map(|r| match r {
            Root::File(f) => ((*f).to_string(), None),
            Root::Fn(f, name) => ((*f).to_string(), Some(*name)),
        })
        .collect();
    while let Some((file, only_fn)) = stack.pop() {
        seen.insert(file.clone());
        if only_fn.is_none() && !walked.insert(file.clone()) {
            continue;
        }
        // The crate roots are reached (and so must be listed) but not walked:
        // they declare every module in the crate, which is not the same as
        // this checker using them. `main.rs`'s real role — the subcommand
        // registry — is pinned separately below.
        if file == "main.rs" || file == "lib.rs" {
            continue;
        }
        let text = read(&file);
        let code = match only_fn {
            Some(name) => code_of(&fn_body(&text, name)),
            None => code_of(&text),
        };
        if only_fn.is_none() {
            let mine = module_of(&file);
            for cap in pats.child_mod.captures_iter(&code) {
                let found = match cap.get(1) {
                    // `#[path]` is relative to the declaring file's directory.
                    Some(rel) => {
                        let dir = Path::new(&file).parent().unwrap_or(Path::new(""));
                        normalize(&dir.join(rel.as_str()))
                    }
                    None => {
                        let mut child = mine.clone();
                        child.push(cap[2].to_string());
                        module_file(&child)
                            .unwrap_or_else(|| panic!("{file}: `mod {};` has no file", &cap[2]))
                    }
                };
                assert!(exists_exact(&found), "{file}: module file {found} does not exist");
                stack.push((found, None));
            }
        }
        for f in named_files(&pats, &file, &code) {
            stack.push((f, None));
        }
        if !is_test_file(&file) {
            let dir = Path::new(SRC).join(&file);
            let dir = dir.parent().expect("file has a parent");
            for cap in pats.include.captures_iter(&code) {
                includes.insert(normalize(&dir.join(&cap[1])));
            }
        }
    }
    (seen, includes)
}

/// Lexically fold `..` so an `include_str!` target reads as a repo path.
fn normalize(p: &Path) -> String {
    let mut out: Vec<String> = Vec::new();
    for c in p.components() {
        match c.as_os_str().to_str().unwrap_or_default() {
            ".." => {
                out.pop();
            }
            "." => {}
            s => out.push(s.to_string()),
        }
    }
    out.join("/")
}

fn covered(spec: &CheckSpec, repo_path: &str) -> bool {
    spec.global.iter().any(|g| glob_match(g, repo_path))
}

#[test]
fn every_file_a_daemon_gate_reaches_is_a_global_input() {
    for s in SURFACES {
        let spec = spec_for(s.component).unwrap_or_else(|| panic!("{} has no spec", s.component));
        assert!(
            !spec.global.contains(&"loom-daemon/**"),
            "{}: narrowed back to `loom-daemon/**` — if that is deliberate, drop it from SURFACES \
and say why in inputs.rs",
            s.component
        );
        let (files, includes) = reach(s.roots);
        assert!(files.len() > 1, "{}: the walk found only {files:?}", s.component);
        for f in &files {
            let path = format!("{SRC}{f}");
            assert!(
                covered(spec, &path),
                "{}'s code reaches `{path}`, which is not in its G set (inputs.rs). A change to \
that file on `main` would not mark this check stale. Add a glob covering it.",
                s.component
            );
        }
        for inc in &includes {
            assert!(
                covered(spec, inc),
                "{}'s code include_str!s `{inc}`, which is not in its G set (inputs.rs).",
                s.component
            );
        }
    }
}

#[test]
fn every_daemon_gate_lists_its_build_inputs_and_entry_chain() {
    for s in SURFACES {
        let spec = spec(s.component);
        for p in BUILD_INPUTS.iter().chain(ENTRY_CHAIN) {
            assert!(spec.global.contains(p), "{}: G must list {p}", s.component);
        }
    }
}

#[test]
fn every_file_that_dispatches_to_a_daemon_gate_is_a_global_input() {
    // The routing between `main()` and the handler: any bin-side file whose code
    // names the handler's entry type/function is on the path that decides what
    // `loom-daemon <subcommand>` runs.
    let root = src_root();
    let mut bin_files = vec!["main.rs".to_string(), "daemon_service.rs".to_string()];
    let mut dirs = vec![root.join("cli"), root.join("daemon_service")];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let rel = p
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                bin_files.push(rel);
            }
        }
    }
    for s in SURFACES {
        let spec = spec(s.component);
        let word = Regex::new(&format!(r"\b{}\b", s.entry_symbol)).unwrap();
        let mut hits = 0;
        for f in &bin_files {
            if is_test_file(f) || !word.is_match(&code_of(&read(f))) {
                continue;
            }
            hits += 1;
            assert!(
                covered(spec, &format!("{SRC}{f}")),
                "{}: `{f}` names `{}` (it dispatches to the gate) but is not in its G set",
                s.component,
                s.entry_symbol
            );
        }
        assert!(
            hits >= 2,
            "{}: expected a definition and a dispatch site for {}",
            s.component,
            s.entry_symbol
        );
    }
}

#[test]
fn every_file_feeding_the_top_level_subcommand_registry_is_a_shell_budget_input() {
    // `cli/shell_budget.rs` validates `Shell-Budget-Callout:` trailers against
    // `crate::Cli`'s top-level subcommand names, and `#[command(flatten)]`
    // lifts another enum's variants to the top level. So every file on that
    // flatten chain decides part of the verdict.
    let pats = patterns();
    let spec = spec("Shell Budget Ratchet");
    let mut seen = BTreeSet::new();
    let mut stack = vec!["main.rs".to_string()];
    while let Some(f) = stack.pop() {
        if !seen.insert(f.clone()) {
            continue;
        }
        let code = code_of(&read(&f));
        for cap in pats.flatten.captures_iter(&code) {
            let segs: Vec<String> = cap[1].split("::").map(str::to_string).collect();
            let target = resolve(&f, &segs)
                .or_else(|| module_file(&segs))
                .unwrap_or_else(|| panic!("{f}: cannot resolve flattened `{}`", &cap[1]));
            stack.push(target);
        }
    }
    assert!(
        seen.len() > 2,
        "the flatten walk found only {seen:?} — the parser probably broke"
    );
    for f in &seen {
        assert!(
            covered(spec, &format!("{SRC}{f}")),
            "`{f}` contributes top-level subcommands (via `#[command(flatten)]`), which the \
Shell Budget Ratchet reads; add it to that spec's G set"
        );
    }
}

// --- Regressions: the #9543/#9544 refusals ------------------------------------

fn set(paths: &[&str]) -> FileSet {
    file_set(paths.iter().map(|p| (*p, false)))
}

fn spec(ctx: &str) -> &'static CheckSpec<'static> {
    spec_for(ctx).unwrap_or_else(|| panic!("{ctx} must have a spec"))
}

#[test]
fn an_unrelated_daemon_file_on_main_no_longer_stales_the_shell_budget() {
    let d = set(&["loom-daemon/src/cli/forge_action.rs"]);
    let p = set(&["defaults/scripts/sweep-lease-publish.sh"]);
    assert_eq!(stale_reason(spec("Shell Budget Ratchet"), &d, &p), None);
}

#[test]
fn the_9543_pair_is_fresh_for_every_daemon_checks_component() {
    // The observed refusal: base moved `cli/forge_action.rs`, the PR touched
    // `init/post_init.rs` (plus, as such PRs do, a shell script).
    let specs = specs_for("Daemon Checks").expect("composite resolves");
    let d = set(&["loom-daemon/src/cli/forge_action.rs"]);
    let p = set(&[
        "loom-daemon/src/init/post_init.rs",
        "defaults/scripts/sweep-lease-publish.sh",
    ]);
    assert_eq!(composite_stale_reason(&specs, &d, &p, &CiScopes::unscoped()), None);
}

#[test]
fn a_move_inside_the_checker_is_still_stale() {
    let p = set(&["defaults/scripts/sweep-lease-publish.sh"]);
    for moved in [
        "loom-daemon/src/shell_budget/churn.rs",
        "loom-daemon/src/shell_budget.rs",
        "loom-daemon/src/cli/shell_budget.rs",
        "loom-daemon/src/main.rs",
        "loom-daemon/Cargo.toml",
    ] {
        let reason = stale_reason(spec("Shell Budget Ratchet"), &set(&[moved]), &p);
        assert!(reason.is_some(), "{moved} is a Shell Budget input");
    }
    // …and the gitignore gate still sees its own pattern source.
    let reason = stale_reason(
        spec(".gitignore Convergence Check"),
        &set(&["loom-daemon/src/init/post_init.rs"]),
        &set(&[".gitignore"]),
    );
    assert!(reason.is_some(), "EPHEMERAL_PATTERNS moving under a .gitignore edit is stale");
}
