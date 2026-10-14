//! What a refused snapshot page does to the credential that asked (W8).
//!
//! The listing runs under one credential at a time, and the two kinds have
//! opposite blast radii:
//!
//! - A **reader** App is read-only and one of several. Its dry pool is its
//!   own problem: the reader is withdrawn for that `(app, owner)` bucket
//!   until the refusal's own reset ([`crate::forge_identity::withdraw_after`],
//!   the same withdrawal every routed read applies), and the lookup falls
//!   through to the writer's snapshot or to private. It is NEVER reported to
//!   the host-wide breaker: tripping that stops every forge call on the host,
//!   the writes included, for a budget none of them spend.
//! - The **writer** is the credential everything else runs on, so its
//!   refusal is the breaker's business — reported with the `GH_CONFIG_DIR`
//!   it ran under and the refused response's head, so the breaker's reset
//!   comes from the refused bucket and not from the host's ambient token.

use crate::forge_bucket_book::Resource;
use crate::forge_identity::{Failure, IdentityRole};
use crate::forge_listing::HttpResponse;
use crate::rate_limit_breaker::report::FailureContext;

use super::{Request, SNAPSHOT_CALLER};

/// GitHub's refusals of this endpoint to a credential that is not an App
/// installation (lowercase): an OAuth or classic token, and a fine-grained
/// personal access token.
const NOT_INSTALLATION_SIGNATURES: &[&str] = &[
    "authenticate with an installation access token",
    "not accessible by personal access token",
];

/// How much of a refusal's body the breaker's classifier is shown.
const BODY_EXCERPT_CHARS: usize = 512;

/// Whether the forge itself said "this is not an installation token" (in the
/// JSON body, or in `gh`'s stderr rendering of it).
pub(super) fn is_installation_token_refusal(body: &str, stderr: &str) -> bool {
    [body, stderr].iter().any(|text| {
        let text = text.to_ascii_lowercase();
        NOT_INSTALLATION_SIGNATURES
            .iter()
            .any(|sig| text.contains(sig))
    })
}

/// The status line and headers of `gh api --include` output: everything
/// before the first blank line.
pub(super) fn response_head(raw: &str) -> String {
    let end = ["\r\n\r\n", "\n\n"]
        .iter()
        .filter_map(|sep| raw.find(sep))
        .min()
        .unwrap_or(raw.len());
    raw[..end].to_string()
}

/// One failed page read.
pub(super) struct Refusal<'a> {
    pub(super) response: Option<&'a HttpResponse>,
    pub(super) stderr: &'a str,
    pub(super) head: Option<&'a str>,
}

/// Report `refusal` for the credential `req` ran under (see the module docs).
pub(super) fn report(req: &Request<'_>, refusal: &Refusal<'_>) {
    if req.cred.role == IdentityRole::Reader {
        withdraw_reader(req, refusal);
    } else {
        report_writer(req, refusal);
    }
}

/// The reader failure a refusal amounts to: a rate limit or a refused token.
/// A `403`/`404` that is neither (coverage, for a per-repo read) says nothing
/// about a listing, and a 5xx is not the credential's fault.
fn reader_failure(refusal: &Refusal<'_>) -> Option<Failure> {
    let r = refusal.response;
    let classified = crate::forge_identity::classify_failure(
        refusal.stderr,
        r.map(|r| r.status),
        r.map(|r| &r.ratelimit),
        Resource::Core,
    )
    .filter(Failure::is_app_wide);
    classified.or_else(|| {
        // A rate limit named only in the body (no telling stderr or header).
        let r = r.filter(|r| super::is_rate_limited(r, refusal.stderr))?;
        let reset = r
            .ratelimit
            .reset_epoch
            .and_then(crate::forge_identity::epoch_time);
        Some(Failure::rate_limited(Resource::Core).with_reset(reset))
    })
}

fn withdraw_reader(req: &Request<'_>, refusal: &Refusal<'_>) {
    let (Some(failure), Some(app_id)) = (reader_failure(refusal), req.cred.app_id.as_deref())
    else {
        return;
    };
    crate::forge_identity::withdraw_after(app_id, req.owner_repo, failure, SNAPSHOT_CALLER);
}

fn report_writer(req: &Request<'_>, refusal: &Refusal<'_>) {
    // The classifier reads text: `gh`'s stderr, plus the start of the body
    // for a refusal whose stderr does not carry the forge's message.
    let body: String = refusal
        .response
        .map(|r| r.body.chars().take(BODY_EXCERPT_CHARS).collect())
        .unwrap_or_default();
    let text = format!("{}\n{body}", refusal.stderr);
    let ctx = FailureContext {
        root: None,
        program: Some(req.gh.to_string_lossy().into_owned()),
        config_dir: req.cred.config_dir.clone(),
        response_head: refusal.head.map(str::to_string),
    };
    crate::rate_limit_breaker::global_observe_failure_ctx(&text, SNAPSHOT_CALLER, ctx);
}
