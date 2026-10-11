//! Dashboard queue-panel source-contract test, split out of `serve.rs` (frozen
//! by the file-size ratchet).

const DASHBOARD_HTML: &str = include_str!("../dashboard.html");

/// #11139: the dashboard's queue panel must treat a partial listing
/// (`listing_incomplete`) like a failed one — flag it INCOMPLETE and
/// never render "no ready loom:issue work" over it. The page has no JS
/// test harness, so pin the `renderQueue` source contract here.
#[test]
fn dashboard_queue_panel_flags_a_partial_listing() {
    let start = DASHBOARD_HTML
        .find("function renderQueue(report)")
        .expect("renderQueue present");
    let end = start
        + DASHBOARD_HTML[start..]
            .find("\nfunction ")
            .expect("a function after renderQueue");
    let body = &DASHBOARD_HTML[start..end];
    assert!(body.contains("tick.listing_incomplete || []"), "{body}");
    assert!(body.contains("came back partial for"), "{body}");
    assert!(
        body.contains("if (failed.length > 0 || partial.length > 0) return;"),
        "an empty table under a partial listing must not read as empty: {body}"
    );
}
