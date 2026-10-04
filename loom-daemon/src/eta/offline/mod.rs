//! Offline, point-in-time evaluation of ETA estimators on **logged**
//! `eta.estimate` / `eta.outcome` pairs (#10193).
//!
//! The replay harness ([`super::backtest`]) builds its cases with empty
//! features, so it cannot score a feature-based or fitted model. This module
//! works on what the daemon actually logged instead, and its whole design is
//! the issue's hard requirement: **strict temporal separation between
//! training and validation.** A model used at instant `t` may use only
//! information knowable before `t`.
//!
//! # The rules, and where each is enforced
//!
//! 1. **Every datum is keyed by when it became knowable**
//!    ([`dataset::LoggedEstimate::knowable_at`],
//!    [`dataset::LandingEvent::knowable_at`]): the instant the daemon logged
//!    it plus a safety margin for the store's own arrival lag, never the
//!    event time it describes.
//! 2. **Training labels are censored at the cutoff.** [`dataset::Logged::rows`]
//!    first drops every record knowable at or after the cutoff, *then* groups
//!    and labels what is left, so an item that landed after the cutoff enters
//!    as censored there (`elapsed = cutoff − as_of`), never as a landing.
//!    [`dataset::assert_point_in_time`] re-checks every built row.
//!    [`dataset::assert_point_in_time`] also refuses a row whose features
//!    carry a read instant (the friction readings, PR/issue creation) after
//!    its own `as_of`.
//! 3. **Everything fitted comes from the fold's training rows only**: for
//!    the Kaplan–Meier reference ([`model::KmModel::fit`]) the stage
//!    vocabulary, age-bucket edges, per-issue weights and cell quantiles; for
//!    the linear quantile regression ([`qr::QrModel::fit`]) the stage
//!    vocabulary, kept columns, means/imputation values, scales, censoring
//!    curve, weights and coefficients. Nothing is computed over the whole
//!    dataset.
//! 4. **Derived features are point-in-time.** This module derives none: it
//!    uses only the features logged live on the estimate itself (the
//!    friction features, [`crate::eta::friction`], are computed live from
//!    data that finished before `as_of`).
//! 5. **Walk-forward evaluation** ([`evaluate::plan`]): each validation day is
//!    scored by the model whose cutoff is that day's start.
//! 6. **Selection is nested in time**: settings are chosen on folds whose
//!    validation outcomes are observed only up to the freeze instant, which is
//!    at or before every reported fold's cutoff ([`evaluate::plan`] refuses a
//!    protocol that breaks this). The reported folds are scored once.
//! 7. **The same issue may sit on both sides of a cutoff**; confidence
//!    intervals resample whole issues ([`evaluate::bootstrap`]).
//! 8. **The heuristic baseline obeys the same rules**: it is the heuristic's
//!    own logged estimate, point-in-time by construction, scored on the
//!    identical `(issue, as_of)` rows as the model.
//!
//! There is no random split, k-fold or shuffle anywhere here. The only
//! pseudo-random draw is the issue bootstrap, which resamples the already
//! fixed reported rows from a fixed seed.
//!
//! The **future-invariance test** (`eta/tests/offline.rs`) is the mechanical
//! guarantee behind rules 1–3: fitting at cutoff `T`, then adding, deleting or
//! altering any record knowable at or after `T`, leaves the fitted model and
//! every fitted transform bit-identical.

pub mod candidate;
pub mod dataset;
pub mod evaluate;
pub mod model;
pub mod qr;
