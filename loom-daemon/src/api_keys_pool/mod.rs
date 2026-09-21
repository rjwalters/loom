//! Provider-neutral API-key account pool for native harness profiles
//! (issue #8401) — the API-key analogue of the Claude OAuth token pool
//! ([`crate::tokens_pool`]) and the Codex `CODEX_HOME` account pool
//! ([`crate::tokens_pool::account_lifecycle`]).
//!
//! # Why a third pool
//!
//! Neither existing pool fits an API-key subscription (a Z.ai GLM coding plan,
//! an OpenAI key, …). The Claude pool is shaped around OAuth tokens with 5h/7d
//! utilisation probes and a `.ranking` file; the Codex pool is shaped around
//! mutable `auth.json` profile directories and their refresh chain. An API-key
//! subscription is simpler than both — an opaque static secret plus a
//! provider-side allowance — but the spawn path still needs the same three
//! answers: *which account gets this spawn*, *is it usable right now*, and
//! *did the run end because it ran dry*.
//!
//! # What this slice covers
//!
//! | Capability | Status |
//! |---|---|
//! | Registry (`.loom/api-keys/<provider>/<account>.env`, `0600`, per-host) | [`registry`] |
//! | `loom-daemon api-keys {add,list,disable,enable,remove,mark-bad,unblock,health}` | `cli::api_keys` |
//! | Spawn-time ladder: explicit env > pool > fail-closed 78 | [`select`], `worker_spawn::credential` |
//! | Per-provider root resolution (per-repo pool, then shared machine pool) | [`paths::resolve_provider_root`] |
//! | Unreadable pool / unparsable state file fails **closed**, never "no pool" | [`paths::PoolReadError`], [`bad_marks::read_marks`] |
//! | Secret-free listing/health by construction | [`registry::ApiKeyAccount`] |
//! | Exhaustion/rate-limit bad-marking with a reset horizon (provider-wide) | [`bad_marks`], [`classify`] |
//! | Automatically calling `mark_bad` from a live failed spawn | **follow-up** — see [`bad_marks`] |
//! | Per-model-class exhaustion scoping (#8058's shape) | **follow-up** |
//! | Per-account concurrency cap | **follow-up** |
//!
//! The deferred rows are tracked in #8424; see the module docs of [`select`]
//! for why a ranking tier with no data source is left as a gap rather than
//! stubbed, and [`bad_marks`] for why automatic classification is not wired
//! to a live spawn's failure in this slice.

pub mod bad_marks;
pub mod classify;
pub mod paths;
pub mod registry;
pub mod select;

pub use bad_marks::{mark_bad, unmark, BadMark};
pub use classify::{classify, Classification};
pub use paths::PoolReadError;
pub use registry::{ApiKeyAccount, Credential, Ineligible};
pub use select::{
    health, is_pooled, list_accounts, select_api_key, select_api_key_for, EmptyApiKeyPoolError,
    ProviderHealth, SelectedApiKey, EX_CONFIG,
};
