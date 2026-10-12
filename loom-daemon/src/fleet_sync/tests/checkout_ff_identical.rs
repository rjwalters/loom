//! Row 9's exception (#11553): a checkout whose every dirty tracked file is
//! already byte-identical to `origin/<default>` is fast-forwarded, the
//! identical edits discarded immediately before the merge. Anything else
//! stays `dirty`, untouched.
//!
//! Real git in temp dirs only, as in the parent module.

use super::*;

fn read(fx: &Fixture, path: &str) -> String {
    fs::read_to_string(fx.host.join(path)).unwrap()
}

#[test]
fn identical_dirt_unstaged_is_discarded_and_fast_forwarded() {
    let fx = Fixture::new();
    let tip = fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "two\n");

    let found = pass(&fx, &Knobs::default());

    let report = only(&found);
    assert_eq!(report.state, CheckoutState::FastForwarded, "{report:?}");
    assert_eq!(fx.head(), tip);
    assert_eq!(git(&fx.host, &["status", "--porcelain"]), "", "and the tree is clean");
    assert_eq!(read(&fx, "README.md"), "two\n");
    let detail = report.detail.as_deref().unwrap();
    assert!(
        detail.contains("identical to origin") && detail.contains("README.md"),
        "{detail}"
    );
}

#[test]
fn identical_dirt_staged_copy_of_origin_is_fast_forwarded() {
    let fx = Fixture::new();
    let tip = fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "two\n");
    git(&fx.host, &["add", "README.md"]);

    let found = pass(&fx, &Knobs::default());

    assert_eq!(only(&found).state, CheckoutState::FastForwarded, "{found:?}");
    assert_eq!(fx.head(), tip);
    assert_eq!(git(&fx.host, &["status", "--porcelain"]), "");
}

#[test]
fn identical_dirt_differing_file_stays_dirty() {
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "mine\n");

    let report = assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert!(report.detail.as_deref().unwrap().contains("README.md"), "{report:?}");
}

#[test]
fn identical_dirt_mixed_identical_and_differing_stays_dirty() {
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "two\n");
    write(&fx.host.join(".loom/scripts/spawn.sh"), "#!/bin/sh\necho mine\n");

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert_eq!(read(&fx, "README.md"), "two\n", "the identical file is not discarded either");
}

#[test]
fn identical_dirt_staged_but_different_stays_dirty() {
    // The working tree equals origin, but the index holds a third blob: real
    // local work that `checkout HEAD --` would lose.
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "staged third\n");
    git(&fx.host, &["add", "README.md"]);
    write(&fx.host.join("README.md"), "two\n");

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Dirty);
    assert_eq!(git(&fx.host, &["show", ":README.md"]), "staged third");
}

#[test]
fn identical_dirt_diverged_is_unchanged() {
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    fx.commit_locally("src/local.rs");
    write(&fx.host.join("README.md"), "two\n");

    assert_skipped(&fx, &Knobs::default(), CheckoutState::Diverged);
}

#[test]
fn identical_dirt_with_autoapply_off_writes_nothing() {
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "two\n");
    let off = Knobs {
        write: false,
        ..Knobs::default()
    };

    let report = assert_skipped(&fx, &off, CheckoutState::Behind);
    assert!(
        report
            .detail
            .as_deref()
            .unwrap()
            .contains("identical to origin"),
        "{report:?}"
    );
}

#[test]
fn identical_dirt_on_a_held_host_writes_nothing() {
    let fx = Fixture::new();
    fx.push("README.md", "two\n");
    write(&fx.host.join("README.md"), "two\n");
    let held = Knobs {
        held: Some("draining"),
        ..Knobs::default()
    };

    assert_skipped(&fx, &held, CheckoutState::Behind);
}
