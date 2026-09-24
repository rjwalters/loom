//! Provider-neutral API-key account pool for native harness profiles
//! (issues #8401, #8424) — the API-key analogue of the Claude OAuth token pool
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
//! # What this covers
//!
//! | Capability | Where |
//! |---|---|
//! | Registry (`.loom/api-keys/<provider>/<account>.env`, `0600`, per-host) | [`registry`] |
//! | `loom-daemon api-keys {add,list,disable,enable,remove,limit,mark-bad,unblock,sync,health}` | `cli::api_keys` |
//! | Pull-based convergence on an external secret source, for hosts nobody seeds by hand (#8511) | [`sync`] |
//! | Spawn-time ladder: explicit env > pool > fail-closed 78 | [`select`], `worker_spawn::credential` |
//! | Per-provider root resolution (per-repo pool, then shared machine pool) | [`paths::resolve_provider_root`] |
//! | Unreadable pool / unparsable state file fails **closed**, never "no pool" | [`paths::PoolReadError`], [`bad_marks::read_marks`] |
//! | Secret-free listing/health by construction | [`registry::ApiKeyAccount`] |
//! | Exhaustion/rate-limit bad-marking with a reset horizon | [`bad_marks`], [`classify`] |
//! | Auth/401 classified apart from exhaustion, with no reset horizon (#8424) | [`classify::Classification::CredentialFailure`] |
//! | Per-model-class exhaustion scoping, #8058's rule (#8424) | [`bad_marks::mark_bad_for_class`], [`bad_marks::is_bad_for_class`] |
//! | Automatic `mark_bad` from a real failed spawn, post-hoc (#8424) | [`ingest`] |
//! | Per-account concurrency cap, enforced at selection (#8424) | [`limits`], [`inflight`], [`select`] |
//!
//! Two rows stay open, both documented where they bite rather than hidden: no
//! health probe exists for an opaque API key, so [`select`] leaves its ranking
//! tier an explicit gap instead of stubbing it, and no live Z.ai *exhaustion*
//! string has been captured yet, so [`classify`] records each pattern's
//! provenance and a test keeps that labelling honest.

pub mod bad_marks;
pub mod classify;
pub mod inflight;
pub mod ingest;
pub mod limits;
pub mod paths;
pub mod registry;
pub mod select;
pub mod sync;

pub use bad_marks::{
    is_bad_for_class, mark_bad, mark_bad_for_class, unmark, unmark_for_class, BadMark,
};
pub use classify::{classify, Classification};
pub use ingest::{ingest_launch_log, LaunchFeedback};
pub use limits::AccountLimits;
pub use paths::PoolReadError;
pub use registry::{ApiKeyAccount, Credential, Ineligible};
pub use select::{
    health, is_pooled, list_accounts, select_api_key, select_api_key_for, EmptyApiKeyPoolError,
    ProviderHealth, SelectedApiKey, EX_CONFIG,
};
pub use sync::{sync, SyncOptions, SyncOutcome, SyncPlan, SyncState};
