//! Why a tick counted an error (Issue #11973): the bounded value set of the
//! `loom.dispatch.error_kind` attribute on the `loom.dispatch.tick` span.
//!
//! `loom.dispatch.errors` carried a count and nothing else, so an `error`
//! tick result could not say whether a listing failed, the shared GitHub rate
//! limit was hit, or a sweep failed to spawn. Every path that bumps
//! [`TickReport::errors`](super::TickReport::errors) now also notes one
//! [`DispatchErrorKind`], and the tick exports the kind it saw (or `mixed`).

/// One reason a tick counted an error. A closed set: the attribute is a
/// span attribute an operator groups by, so it must never carry free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DispatchErrorKind {
    /// A workspace's ready-issue listing failed (not a rate limit).
    ForgeListing,
    /// A listing or dispatch failed on the shared GitHub API rate limit.
    RateLimited,
    /// `dispatch()` died at token selection (#6614).
    TokenSelection,
    /// A peer host already claimed the issue (#5789).
    ClaimCollision,
    /// The local claim lock was already held (#8907).
    ClaimLock,
    /// The sweep child could not be spawned.
    DispatchSpawn,
    /// No runtime was admitted for the role (runtime configuration).
    Config,
    /// Any other dispatch failure.
    Other,
}

impl DispatchErrorKind {
    /// Every kind, in the order the attribute documents them.
    pub const ALL: [Self; 8] = [
        Self::ForgeListing,
        Self::RateLimited,
        Self::TokenSelection,
        Self::ClaimCollision,
        Self::ClaimLock,
        Self::DispatchSpawn,
        Self::Config,
        Self::Other,
    ];

    /// The attribute value when a tick saw more than one kind.
    pub const MIXED: &'static str = "mixed";

    /// The attribute value for this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ForgeListing => "forge_listing",
            Self::RateLimited => "rate_limited",
            Self::TokenSelection => "token_selection",
            Self::ClaimCollision => "claim_collision",
            Self::ClaimLock => "claim_lock",
            Self::DispatchSpawn => "dispatch_spawn",
            Self::Config => "config",
            Self::Other => "other",
        }
    }

    /// The kind of a failed ready-issue listing.
    #[must_use]
    pub fn of_listing(err: &anyhow::Error) -> Self {
        if crate::rate_limit_breaker::indicates_rate_limit(&format!("{err:#}")) {
            Self::RateLimited
        } else {
            Self::ForgeListing
        }
    }

    /// The kind of a `dispatch()` failure no typed refusal matched. Typed
    /// errors are matched by downcast; the spawn failure is an `anyhow`
    /// context (`failed to spawn ... sweep child`) with no type of its own.
    #[must_use]
    pub fn of_dispatch(err: &anyhow::Error) -> Self {
        let text = format!("{err:#}");
        if crate::rate_limit_breaker::indicates_rate_limit(&text) {
            Self::RateLimited
        } else if err
            .downcast_ref::<crate::runtime_admission::RuntimeRejection>()
            .is_some()
        {
            Self::Config
        } else if err
            .chain()
            .any(|cause| cause.to_string().starts_with("failed to spawn"))
        {
            Self::DispatchSpawn
        } else {
            Self::Other
        }
    }
}

/// The `loom.dispatch.error_kind` value for a tick that counted `errors`
/// errors and noted `kinds`: `None` when it counted none, the one kind's
/// token when it noted one, `mixed` when it noted several, and `other` for a
/// counted error no path classified.
#[must_use]
pub fn attribute_value(errors: usize, kinds: &[DispatchErrorKind]) -> Option<&'static str> {
    if errors == 0 {
        return None;
    }
    Some(match kinds {
        [] => DispatchErrorKind::Other.as_str(),
        [kind] => kind.as_str(),
        _ => DispatchErrorKind::MIXED,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn every_value_is_a_fixed_token() {
        let mut tokens: Vec<&str> = DispatchErrorKind::ALL.iter().map(|k| k.as_str()).collect();
        tokens.push(DispatchErrorKind::MIXED);
        let n = tokens.len();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), n, "tokens are distinct");
        for t in tokens {
            assert!(t.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{t}");
        }
    }

    #[test]
    fn listing_failures_split_on_the_rate_limit() {
        let limited = anyhow!("gh: API rate limit exceeded for installation (HTTP 403)");
        assert_eq!(DispatchErrorKind::of_listing(&limited), DispatchErrorKind::RateLimited);
        let down = anyhow!("gh: HTTP 502 Bad Gateway");
        assert_eq!(DispatchErrorKind::of_listing(&down), DispatchErrorKind::ForgeListing);
    }

    #[test]
    fn untyped_dispatch_failures_are_classified() {
        let spawn = anyhow!("No such file or directory").context("failed to spawn sweep child");
        assert_eq!(DispatchErrorKind::of_dispatch(&spawn), DispatchErrorKind::DispatchSpawn);
        let prset = anyhow!("EAGAIN").context("failed to spawn PrSet sweep child");
        assert_eq!(DispatchErrorKind::of_dispatch(&prset), DispatchErrorKind::DispatchSpawn);
        let limited = anyhow!("secondary rate limit").context("issue edit failed");
        assert_eq!(DispatchErrorKind::of_dispatch(&limited), DispatchErrorKind::RateLimited);
        let other = anyhow!("refusing to dispatch an empty PrSet");
        assert_eq!(DispatchErrorKind::of_dispatch(&other), DispatchErrorKind::Other);
    }

    #[test]
    fn the_attribute_is_absent_single_or_mixed() {
        use DispatchErrorKind as K;
        assert_eq!(attribute_value(0, &[]), None);
        assert_eq!(attribute_value(2, &[K::ForgeListing]), Some("forge_listing"));
        assert_eq!(attribute_value(2, &[K::ForgeListing, K::ClaimLock]), Some("mixed"));
        assert_eq!(attribute_value(1, &[]), Some("other"), "an unclassified error is other");
    }
}
