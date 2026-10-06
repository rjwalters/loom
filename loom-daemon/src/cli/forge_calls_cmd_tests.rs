#![allow(clippy::unwrap_used)]
//! `forge calls` golden output over a fixture sink holding pre-W1 and W1
//! lines side by side.

use super::*;

const NOW: i64 = 1_900_000_000;

fn fixture() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    // The daemon's sink is owner-only; a group-writable one (umask 002) is
    // refused on read, so pin the mode rather than inherit the host's umask.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let rst = NOW + 1200;
    let lines = [
        // Written by a pre-W1 binary: no attribution at all.
        format!(
            r#"{{"t":{},"c":"claim.pr_view","p":"graphql","o":"ok","op":"unknown","rp":"acme/widget","ir":"writer"}}"#,
            NOW - 100
        ),
        format!(
            r#"{{"t":{},"c":"claim.pr_get","p":"core","o":"ok","rst":{rst},"rp":"acme/widget","ir":"reader","ro":"target","ca":"app-42","co":"acme","tk":"reader","rr":"core","pg":3}}"#,
            NOW - 90
        ),
        format!(
            r#"{{"t":{},"c":"claim.pr_get","p":"core","o":"not_modified","rst":{rst},"rp":"acme/widget","ir":"reader","ro":"target","ca":"app-42","co":"acme","tk":"reader","rr":"core"}}"#,
            NOW - 80
        ),
        format!(
            r#"{{"t":{},"c":"issue.list","p":"graphql","o":"rate_limited","rp":"acme/other","ir":"writer","ro":"remote","ca":"app-7","co":"acme","tk":"writer","rr":"graphql","rd":true}}"#,
            NOW - 70
        ),
        format!(
            r#"{{"t":{},"c":"api.comments","p":"core","o":"ok","ir":"writer","ro":"none","ca":"ambient","tk":"ambient","rr":"core","pu":true}}"#,
            NOW - 60
        ),
        // Outside the window.
        format!(r#"{{"t":{},"c":"old","p":"core","o":"ok"}}"#, NOW - 7200),
        "not json".to_string(),
    ];
    std::fs::write(
        dir.path()
            .join(format!("calls-{}.jsonl", NOW.div_euclid(3600))),
        lines.join("\n") + "\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join(forge_bucket_book::SNAPSHOT_FILE),
        format!(
            r#"[{{"key":{{"account":"app-42","owner":"acme","resource":"core"}},"reading":{{"limit":5000,"remaining":4880,"used":120,"reset_epoch":{rst},"observed_at":{},"source":"probe"}}}}]"#,
            NOW - 30
        ),
    )
    .unwrap();
    dir
}

fn report(by: GroupBy) -> String {
    let dir = fixture();
    let agg = buckets::aggregate_since(dir.path(), NOW - 3600, NOW, by);
    let book = forge_bucket_book::load(dir.path(), NOW);
    render(&agg, &book, by, "1h", Utc.timestamp_opt(NOW, 0).unwrap())
}

#[test]
fn by_bucket_golden() {
    let want = "\
Forge calls, last 1h by bucket — 5 row(s) as of 2030-03-17 17:46 UTC
  ACCOUNT  CRED_OWNER  INSTALLATION  RESOURCE  RESET    CHARGED    304  LIMITED  ERROR
  ambient  -           -             core      -              1      0        0      0
  app-42   acme        -             core      18:06Z         3      1        0      0
  app-7    acme        -             graphql   -              0      0        1      0
  unknown  -           -             graphql   -              1      0        0      0
  unattributed: 1 without a repo, 1 without a credential (older binary); 1 paginated row(s) with unknown pages; 1 cwd-route disagreement(s)
Bucket readings (newest, < 10m old):
  ACCOUNT        OWNER          RESOURCE    USED   LIMIT REMAINING  RESET   SOURCE
  app-42         acme           core         120    5000      4880  18:06Z  probe (30s ago)
";
    assert_eq!(report(GroupBy::Bucket), want);
}

#[test]
fn by_caller_golden() {
    let want = "\
Forge calls, last 1h by caller — 5 row(s) as of 2030-03-17 17:46 UTC
  CALLER          CHARGED    304  LIMITED  ERROR
  api.comments          1      0        0      0
  claim.pr_get          3      1        0      0
  claim.pr_view         1      0        0      0
  issue.list            0      0        1      0
";
    assert!(report(GroupBy::Caller).starts_with(want), "{}", report(GroupBy::Caller));
}

#[test]
fn an_empty_sink_says_so_and_since_is_validated() {
    let dir = tempfile::tempdir().unwrap();
    let agg = buckets::aggregate_since(dir.path(), NOW - 3600, NOW, GroupBy::Role);
    let text = render(&agg, &[], GroupBy::Role, "1h", Utc.timestamp_opt(NOW, 0).unwrap());
    assert!(text.contains("no rows recorded"), "{text}");
    assert!(text.contains("none believed"), "{text}");
    assert_eq!(parse_since("90m").unwrap(), 5400);
    assert_eq!(parse_since("3h").unwrap(), 10_800);
    for bad in ["", "h", "3d", "0m", "48h", "x1h"] {
        assert!(parse_since(bad).is_err(), "{bad}");
    }
}
