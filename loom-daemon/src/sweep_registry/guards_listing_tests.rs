//! Leg 0 of the registry's open-PR guard (#10514): the cached REST open-PR
//! listing answers, and the GraphQL closes-graph and the timeline walk are
//! never spawned on that path. The fake `gh` here REFUSES both (logs
//! `FORBIDDEN`, exits 97) unless a test opts into a GraphQL fallback answer.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::claim_reconciliation::open_pr_listing::test_support::{listing, row, Row};
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

const REPO: &str = "rjwalters/loom";

/// A `bash` arm serving the open-PR listing `rows` with `200` + an ETag, or a
/// `304` (exit 1, like real `gh`) when the caller presents that ETag.
fn etag_pulls_arm(rows: &[Row]) -> String {
    format!(
        "case \"$*\" in\n  \
         api*'pulls?state=open'*'If-None-Match'*)\n    \
         printf 'HTTP/2.0 304 Not Modified\\r\\n\\r\\n'; exit 1 ;;\n  \
         api*'pulls?state=open'*)\n    \
         printf 'HTTP/2.0 200 OK\\r\\nEtag: W/\"l1\"\\r\\n\\r\\n'\n    \
         echo '{}'\n    exit 0 ;;\nesac\n",
        listing(rows)
    )
}

/// The listing arm failing like a forge outage.
const FAILING_PULLS_ARM: &str = "case \"$*\" in api*'pulls?state=open'*)\n  \
     echo 'gh: Something went wrong (HTTP 502)' >&2; exit 1 ;;\nesac\n";

/// A registry whose fake `gh` runs `pulls_arm` for the listing, answers
/// `repo view`, answers `api graphql` with `graphql` (exit 0) when given and
/// otherwise refuses it, always refuses the timeline, and answers the #6788
/// known-PR recheck (`pulls/<n> --jq .state`) with `recheck`.
fn registry(
    ws: &Path,
    pulls_arm: &str,
    graphql: Option<&str>,
    recheck: &str,
) -> (SweepRegistry, PathBuf) {
    let log = ws.join("gh.log");
    let gql = graphql.map_or_else(String::new, |nodes| {
        format!(
            "if [[ \"$1\" == api && \"$2\" == graphql ]]; then\n  printf '%s\\n' '{{\"data\":{{\"repository\":{{\"issue\":{{\"closedByPullRequestsReferences\":{{\"nodes\":[{nodes}]}}}}}}}}}}'\n  exit 0\nfi\n"
        )
    });
    let script = format!(
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >> \"{log}\"\n\
         {pulls_arm}\
         if [[ \"$1\" == repo && \"$2\" == view ]]; then printf '{REPO}\\n'; exit 0; fi\n\
         {gql}\
         case \"$*\" in *graphql*|*timeline*) echo FORBIDDEN >> \"{log}\"; exit 97 ;; esac\n\
         if [[ \"$1\" == api && \"$2\" == repos/*/pulls/* ]]; then printf '%s\\n' '{recheck}'; exit 0; fi\n\
         exit 1\n",
        log = log.display(),
    );
    let gh = ws.join("fake-gh.sh");
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.gh_bin = Some(gh);
    config.skip_label_flip = true;
    config.journal_path = Some(ws.join("journal.json"));
    (SweepRegistry::new(config), log)
}

fn calls(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

fn assert_listing_only(log: &Path) {
    let calls = calls(log);
    assert!(!calls.contains("graphql"), "GraphQL spawned on the listing path:\n{calls}");
    assert!(!calls.contains("timeline"), "timeline walked on the listing path:\n{calls}");
    assert!(!calls.contains("FORBIDDEN"), "{calls}");
    assert!(calls.contains("pulls?state=open"), "{calls}");
}

fn ours(n: u32) -> Row {
    row(n, &[]).head(&format!("topic-{n}")).repo(REPO)
}

#[test]
fn the_listing_answers_every_verdict_without_graphql_or_timeline() {
    let rows = [
        ours(501).body("Closes #42"),
        ours(502).body("Part of #43"),
        row(503, &[]).repo(REPO).body("no phrase"), // head feature/issue-503
        ours(504).body("filed #45 to track it"),
        row(505, &[])
            .head("topic")
            .repo("outsider/loom")
            .author("rando", "NONE")
            .body("Closes #46"),
        row(506, &[])
            .head("topic")
            .repo("friend/loom")
            .author("pal", "COLLABORATOR")
            .body("Closes #47"),
    ];
    for (issue, want) in [
        (42, OpenPrProbe::Open(501)),
        (43, OpenPrProbe::Open(502)),
        (503, OpenPrProbe::Open(503)),
        (45, OpenPrProbe::NoneOpen),
        (46, OpenPrProbe::NoneOpen),
        (47, OpenPrProbe::Open(506)),
        (4, OpenPrProbe::NoneOpen),
    ] {
        let dir = tempdir().unwrap();
        let (reg, log) = registry(dir.path(), &etag_pulls_arm(&rows), None, "");
        assert_eq!(reg.probe_open_linked_pr_transports(issue), want, "issue #{issue}");
        assert_listing_only(&log);
    }
}

/// The second probe presents the first one's ETag; the `304` is served from
/// the cached body and still gives the right verdict.
#[test]
fn a_304_revalidation_still_answers() {
    let dir = tempdir().unwrap();
    let rows = [ours(601).body("Closes #60")];
    let (reg, log) = registry(dir.path(), &etag_pulls_arm(&rows), None, "");
    assert_eq!(reg.probe_open_linked_pr_transports(60), OpenPrProbe::Open(601));
    assert_eq!(reg.probe_open_linked_pr_transports(60), OpenPrProbe::Open(601));
    assert_eq!(reg.probe_open_linked_pr_transports(61), OpenPrProbe::NoneOpen);
    assert!(calls(&log).contains("If-None-Match"), "{}", calls(&log));
    assert_listing_only(&log);
}

/// A listing that cannot be read is no verdict: the GraphQL closes-graph
/// fallback answers instead.
#[test]
fn a_failed_listing_falls_back_to_graphql() {
    let dir = tempdir().unwrap();
    let nodes = r#"{"number":701,"state":"OPEN"}"#;
    let (reg, log) = registry(dir.path(), FAILING_PULLS_ARM, Some(nodes), "");
    assert_eq!(reg.probe_open_linked_pr_transports(70), OpenPrProbe::Open(701));
    assert!(calls(&log).contains("api graphql"), "{}", calls(&log));
}

/// Every transport failing is `ProbeFailed` — never `NoneOpen` (#7863) — and
/// the #6788 known-PR recheck still holds a memoized open PR.
#[test]
fn total_failure_is_probe_failed_and_the_memo_recheck_still_holds() {
    let dir = tempdir().unwrap();
    let (reg, _log) = registry(dir.path(), FAILING_PULLS_ARM, None, "open");
    assert_eq!(reg.probe_open_linked_pr_transports(80), OpenPrProbe::ProbeFailed);
    assert_eq!(reg.probe_open_linked_pr(80), OpenPrProbe::ProbeFailed, "no memo yet");
    reg.seed_open_pr_memo(80, 801, Utc::now() - chrono::Duration::hours(2));
    assert_eq!(reg.probe_open_linked_pr(80), OpenPrProbe::Open(801));
}
