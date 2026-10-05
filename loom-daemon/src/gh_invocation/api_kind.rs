//! Which GitHub API surface a `gh` invocation uses — the `github.api` span
//! attribute (#10282).
//!
//! The classification is per *subcommand*, not per top-level command: `gh
//! release view` with no tag is a REST `GET /releases/latest` while `gh release
//! list` is GraphQL, and `gh search` is REST throughout. Only command shapes
//! whose protocol is known from the `gh` source are labelled `rest`/`graphql`;
//! a shape known to touch both (e.g. `gh release view <tag>`: REST by tag, then
//! a GraphQL draft-release fallback) is `mixed`, and anything unrecognised is
//! `unknown` — never a confident guess.

/// The closed `github.api` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKind {
    Rest,
    Graphql,
    /// Known to issue both REST and GraphQL requests.
    Mixed,
    /// Not classified — no claim is made either way.
    Unknown,
}

impl ApiKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ApiKind::Rest => "rest",
            ApiKind::Graphql => "graphql",
            ApiKind::Mixed => "mixed",
            ApiKind::Unknown => "unknown",
        }
    }

    /// Every value, for vocabulary tests and docs.
    pub const ALL: [ApiKind; 4] = [
        ApiKind::Rest,
        ApiKind::Graphql,
        ApiKind::Mixed,
        ApiKind::Unknown,
    ];
}

/// Flags (across the subcommands classified here) whose value is the next
/// argument, so it is not mistaken for a positional.
const VALUE_FLAGS: &[&str] = &[
    "-R",
    "--repo",
    "-q",
    "--jq",
    "-t",
    "--template",
    "--json",
    "-X",
    "--method",
    "-H",
    "--header",
    "-f",
    "--raw-field",
    "-F",
    "--field",
    "--input",
    "--hostname",
    "--cache",
    "-p",
    "--preview",
    "-D",
    "--dir",
    "-O",
    "--output",
    "--pattern",
    "-A",
    "--archive",
];

/// The positional (non-flag) arguments of `args`, in order.
fn positionals(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--" {
            out.extend(iter.by_ref().map(String::as_str));
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            if !a.contains('=') && VALUE_FLAGS.contains(&a.as_str()) {
                iter.next();
            }
            continue;
        }
        out.push(a.as_str());
    }
    out
}

/// Classify a `gh` argument vector (without the `gh` program itself).
#[must_use]
pub fn classify(args: &[String]) -> ApiKind {
    let pos = positionals(args);
    let sub = pos.get(1).copied();
    match pos.first().copied() {
        // `gh api graphql` is GraphQL; any other endpoint path is REST.
        Some("api") => match sub {
            Some("graphql") => ApiKind::Graphql,
            Some(_) => ApiKind::Rest,
            None => ApiKind::Unknown,
        },
        // Actions, settings and search families are REST-only in `gh`.
        Some(
            "run" | "workflow" | "cache" | "secret" | "variable" | "search" | "ssh-key" | "gpg-key",
        ) => ApiKind::Rest,
        // Projects (v2) exist only in GraphQL.
        Some("project") => ApiKind::Graphql,
        // `gh pr diff` fetches the diff over REST after a GraphQL lookup.
        Some("pr") if sub == Some("diff") => ApiKind::Mixed,
        Some("issue" | "pr") if sub.is_some() => ApiKind::Graphql,
        Some("release") => match (sub, pos.get(2)) {
            // `FetchLatestRelease`: REST GET /releases/latest.
            (Some("view" | "download"), None) => ApiKind::Rest,
            // `FetchRelease`: REST by tag, then a GraphQL draft fallback.
            (Some("view" | "download" | "edit" | "delete" | "upload"), Some(_)) => ApiKind::Mixed,
            (Some("list"), _) => ApiKind::Graphql,
            _ => ApiKind::Unknown,
        },
        Some("label") => match sub {
            Some("list") => ApiKind::Graphql,
            Some("create" | "edit" | "delete") => ApiKind::Rest,
            Some("clone") => ApiKind::Mixed,
            _ => ApiKind::Unknown,
        },
        Some("repo") => match sub {
            Some("view" | "list") => ApiKind::Graphql,
            Some(_) => ApiKind::Mixed,
            None => ApiKind::Unknown,
        },
        _ => ApiKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(args: &[&str]) -> &'static str {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        classify(&owned).as_str()
    }

    #[test]
    fn api_endpoint_decides_rest_vs_graphql() {
        assert_eq!(kind(&["api", "repos/acme/widgets/issues"]), "rest");
        assert_eq!(kind(&["api", "-X", "POST", "repos/acme/widgets/labels"]), "rest");
        assert_eq!(kind(&["api", "graphql", "-f", "query=x"]), "graphql");
        // A flag value equal to "graphql" is not the endpoint.
        assert_eq!(kind(&["api", "-H", "graphql", "repos/acme/w"]), "rest");
        assert_eq!(kind(&["api"]), "unknown");
    }

    /// Regression (#10344 review): these were labelled `graphql`.
    #[test]
    fn release_view_without_tag_is_rest() {
        assert_eq!(kind(&["release", "view"]), "rest");
        assert_eq!(kind(&["release", "view", "--json", "tagName", "-R", "acme/widgets"]), "rest");
        assert_eq!(
            kind(&[
                "release",
                "view",
                "--json",
                "tagName,assets",
                "-R",
                "a/b",
                "--jq",
                ".x"
            ]),
            "rest"
        );
        assert_eq!(kind(&["release", "download", "-R", "a/b", "-p", "*.tgz"]), "rest");
    }

    /// Regression (#10344 review): `gh search` is REST, never GraphQL.
    #[test]
    fn search_is_rest() {
        for sub in ["issues", "prs", "repos", "code", "commits"] {
            assert_eq!(kind(&["search", sub, "is:open", "--json", "number"]), "rest", "{sub}");
        }
    }

    #[test]
    fn dual_protocol_shapes_are_mixed_not_guessed() {
        assert_eq!(
            kind(&[
                "release",
                "view",
                "v1.2.3",
                "--json",
                "publishedAt",
                "-R",
                "a/b"
            ]),
            "mixed"
        );
        assert_eq!(kind(&["release", "upload", "v1", "x.tgz"]), "mixed");
        assert_eq!(kind(&["pr", "diff", "12"]), "mixed");
        assert_eq!(kind(&["repo", "edit", "--add-topic", "x"]), "mixed");
        assert_eq!(kind(&["label", "clone", "a/b"]), "mixed");
    }

    #[test]
    fn graphql_and_rest_porcelain() {
        assert_eq!(kind(&["pr", "list"]), "graphql");
        assert_eq!(kind(&["issue", "view", "7", "--json", "labels"]), "graphql");
        assert_eq!(kind(&["repo", "view", "--json", "nameWithOwner"]), "graphql");
        assert_eq!(kind(&["release", "list", "-R", "a/b"]), "graphql");
        assert_eq!(kind(&["project", "item-list", "1"]), "graphql");
        assert_eq!(kind(&["label", "list"]), "graphql");
        assert_eq!(kind(&["label", "create", "x"]), "rest");
        assert_eq!(kind(&["run", "list"]), "rest");
        assert_eq!(kind(&["workflow", "run", "ci.yml"]), "rest");
    }

    #[test]
    fn unrecognised_commands_are_unknown() {
        assert_eq!(kind(&[]), "unknown");
        assert_eq!(kind(&["status"]), "unknown");
        assert_eq!(kind(&["auth", "status"]), "unknown");
        assert_eq!(kind(&["release"]), "unknown");
        assert_eq!(kind(&["release", "create", "v1"]), "unknown");
    }

    #[test]
    fn vocabulary_is_closed() {
        let names: Vec<_> = ApiKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(names, ["rest", "graphql", "mixed", "unknown"]);
    }
}
