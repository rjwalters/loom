//! Credential-store path recognition for the role-tool-policy matcher (#8256).
//!
//! A word names a credential store when, after **lexical** normalization, it
//! is a path inside one of [`CREDENTIAL_DIRS`] below a home directory. The
//! normalization is what the Bash path has no other source for: the
//! Edit/Write hook canonicalizes its target before asking, but a Bash word is
//! raw text, so `/tmp/../home/u/.ssh/x`, `//home/u/.ssh/x`, `/./home/…` and
//! `/proc/self/root/home/u/.ssh/x` must all be reduced to `/home/u/.ssh/x`
//! here, before the home-prefix test.
//!
//! Normalization is lexical only. A symlink that already exists on disk and
//! points into a credential directory is not resolved — that is a documented
//! limit (`guard-hooks.md`), not something a text matcher can see.

/// Home-relative credential locations (`credential-store`), as component
/// lists. A path is inside one when its home-relative components start with
/// the entry's components.
pub const CREDENTIAL_DIRS: [&[&str]; 12] = [
    &[".ssh"],
    &[".aws"],
    &[".gnupg"],
    &[".netrc"],
    &[".git-credentials"],
    &[".kube"],
    &[".azure"],
    &[".config", "gh"],
    &[".config", "gcloud"],
    &[".docker", "config.json"],
    &[".loom", "tokens"],
    &[".claude", ".credentials.json"],
];

/// Stand-in user name for `~/…` / `$HOME/…` when the hook's `$HOME` is
/// unknown. It sits under `/home/`, so `~/../other/.ssh` still normalizes to
/// a `/home/<u>/.ssh` path.
const UNKNOWN_HOME: &str = "/home/~";

/// `true` when `word` names a path inside a home credential store.
pub(super) fn is_credential_path(word: &str, home: Option<&str>) -> bool {
    candidates(word, home)
        .iter()
        .any(|(comps, anchored)| in_credential_dir(comps, *anchored))
}

fn in_credential_dir(comps: &[String], anchored: bool) -> bool {
    CREDENTIAL_DIRS.iter().any(|dir| {
        dir.len() <= comps.len()
            && dir.iter().zip(comps).all(|(want, got)| {
                // Glob widening only below an explicit home anchor: a bare
                // relative `*` or `.*` is far more often a repo glob than a
                // reach into `$HOME`.
                if anchored {
                    glob_match(got, want)
                } else {
                    got == want
                }
            })
    })
}

/// The home-relative component lists `word` could denote, each with `true`
/// when it was reached through an explicit home anchor. A relative path is
/// resolved against an unknown cwd — `$HOME` often enough (`cd && cat
/// .ssh/id_rsa`) to count as home-relative, unanchored.
fn candidates(word: &str, home: Option<&str>) -> Vec<(Vec<String>, bool)> {
    let home = home
        .map(|h| h.trim_end_matches('/'))
        .filter(|h| h.starts_with('/'));
    let mut out = Vec::new();
    if word.is_empty() {
        return out;
    }
    // Expand the home spellings to an absolute path, so `..` after them is
    // resolved like any other component (`~/../other/.ssh`).
    let mut abs: Option<String> = None;
    for prefix in ["~/", "$HOME/", "${HOME}/"] {
        if let Some(rest) = word.strip_prefix(prefix) {
            abs = Some(format!("{}/{rest}", home.unwrap_or(UNKNOWN_HOME)));
        }
    }
    if abs.is_none() {
        if let Some(rest) = word.strip_prefix('~') {
            // `~user/…`
            let Some((user, rest)) = rest.split_once('/') else {
                return out;
            };
            abs = Some(format!("/home/{user}/{rest}"));
        }
    }
    let abs = abs.or_else(|| word.starts_with('/').then(|| word.to_string()));
    if let Some(abs) = abs {
        let (comps, rooted) = normalize(&abs, true);
        if rooted {
            if let Some(rel) = below_home(&comps, home) {
                out.push((rel, true));
            }
        } else {
            // `/proc/<pid>/cwd/…`: relative to an unknown directory.
            out.push((comps, false));
        }
        return out;
    }
    if word.starts_with('$') {
        return out;
    }
    let (comps, _) = normalize(word, false);
    // `../../home/u/.ssh` climbs out of the unknown cwd: with enough `..` it
    // is the absolute path that follows them.
    if word.split('/').any(|c| c == "..") {
        let mut climbed: Vec<String> = Vec::new();
        for comp in word.split('/') {
            match comp {
                "" | "." => {}
                ".." if climbed.is_empty() => {}
                ".." => {
                    climbed.pop();
                }
                c => climbed.push(c.to_string()),
            }
        }
        let (rooted, _) = normalize(&format!("/{}", climbed.join("/")), true);
        if let Some(rel) = below_home(&rooted, home) {
            out.push((rel, true));
        }
    }
    out.push((comps, false));
    out
}

/// Lexically normalize `path` into components: empty and `.` dropped, `..`
/// pops (and stays at `/` when rooted). The kernel's magic links are
/// followed lexically too: `/proc/<pid>/root` (and the per-task form) IS
/// the root directory, so reaching it resets to `/`; `/proc/<pid>/cwd` is the
/// process's unknown cwd, so reaching it makes the rest relative. Returns the
/// components and whether they are still rooted at `/`.
fn normalize(path: &str, rooted: bool) -> (Vec<String>, bool) {
    let mut out: Vec<String> = Vec::new();
    let mut rooted = rooted;
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c.to_string()),
        }
        if rooted {
            match proc_magic(&out) {
                Some(true) => out.clear(),
                Some(false) => {
                    out.clear();
                    rooted = false;
                }
                None => {}
            }
        }
    }
    (out, rooted)
}

/// `Some(true)` for a `/proc/<pid>/root` stack, `Some(false)` for a
/// `/proc/<pid>/cwd` one (each also as `/proc/<pid>/task/<tid>/…`).
fn proc_magic(stack: &[String]) -> Option<bool> {
    let is_pid = |s: &String| {
        matches!(s.as_str(), "self" | "thread-self" | "*")
            || (!s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || "*?[]".contains(c)))
    };
    let link = match stack {
        [p, pid, link] if p == "proc" && is_pid(pid) => link,
        [p, pid, t, tid, link] if p == "proc" && is_pid(pid) && t == "task" && is_pid(tid) => link,
        _ => return None,
    };
    match link.as_str() {
        "root" => Some(true),
        "cwd" => Some(false),
        _ => None,
    }
}

/// The components below a home directory: the hook's own `$HOME`, `/root`,
/// `/home/<u>`, or `/Users/<u>`.
fn below_home(comps: &[String], home: Option<&str>) -> Option<Vec<String>> {
    if let Some(h) = home {
        let (hc, _) = normalize(h, true);
        if !hc.is_empty() && comps.len() > hc.len() && comps[..hc.len()] == hc[..] {
            return Some(comps[hc.len()..].to_vec());
        }
    }
    match comps {
        [r, rest @ ..] if r == "root" => Some(rest.to_vec()),
        [b, _user, rest @ ..] if b == "home" || b == "Users" => Some(rest.to_vec()),
        _ => None,
    }
}

/// Shell-glob match of one path component: `*`, `?`, and `[…]` as one char.
/// A literal component compares exactly, so this only widens what a glob can
/// reach (`~/.s*h`, `~/.*/id_rsa`), never what a plain name does.
pub(super) fn glob_match(pattern: &str, name: &str) -> bool {
    // As in the shell, a wildcard never matches a leading dot.
    if name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && p[pi] == '[' {
            if let Some(close) = p[pi..].iter().position(|&c| c == ']') {
                pi += close + 1;
                ni += 1;
                continue;
            }
        }
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}
