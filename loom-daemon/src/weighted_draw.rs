//! A generic, seeded weighted draw over `(key, weight)` pairs (#11726).
//!
//! `P(key) = weight / Σ weight`, over the candidates still in play. A weight of
//! `0` is "off": the candidate is never drawn and never counted in `Σ`. The
//! randomness is [`PickRng`] — the same seedable `SplitMix64` the workspace
//! draw uses (#11683) — so a fixed seed reproduces an exact draw sequence.
//!
//! Two shapes:
//!
//! - [`draw`] — one roll, one pick: the "ruling" form, `roll ∈ [0, Σw)`, the
//!   pick is the candidate whose cumulative weight first exceeds `roll`.
//! - [`draw_order`] — a weighted random **permutation** (sequential sampling
//!   without replacement, a.k.a. Plackett–Luce). Its first step is exactly
//!   [`draw`]; every later step re-draws from the candidates not yet picked.
//!
//! Why the permutation matters: a caller that learns *after* drawing that its
//! pick cannot serve (no seat, held, at a ceiling) moves to the next step of
//! the order. The first **available** candidate in a Plackett–Luce order is
//! distributed exactly as `w / Σ w` over the available candidates — removing
//! an unavailable candidate and renormalising the rest, without having to ask
//! every candidate's availability up front. `role_draw_tests` checks that
//! numerically.
//!
//! Pure: no clock, no I/O, no global state.

use crate::priority_pick::PickRng;
use serde::{Deserialize, Serialize};

/// One roll of a draw: the candidates' total weight at that point, the roll in
/// `0..total`, and the key it picked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrawStep<K> {
    /// `Σ w` over the candidates still in play at this step.
    pub total: u64,
    /// The roll, in `0..total`.
    pub roll: u64,
    /// The picked key.
    pub picked: K,
}

/// Draw one key. `None` when no candidate has a positive weight.
///
/// Candidates are taken in the order given (the caller's declaration order),
/// which is part of what makes a draw replayable from its record.
pub fn draw<K: Clone>(candidates: &[(K, u64)], rng: &mut PickRng) -> Option<DrawStep<K>> {
    let total: u64 = candidates.iter().map(|(_, w)| *w).sum();
    if total == 0 {
        return None;
    }
    let roll = rng.below(total);
    let mut acc = 0u64;
    for (key, weight) in candidates {
        acc += *weight;
        if roll < acc {
            return Some(DrawStep {
                total,
                roll,
                picked: key.clone(),
            });
        }
    }
    // Unreachable: `roll < total == acc` after the last candidate.
    None
}

/// A full weighted permutation of the positive-weight candidates: step `i`
/// draws from the candidates steps `0..i` did not pick. Zero-weight
/// candidates never appear.
pub fn draw_order<K: Clone>(candidates: &[(K, u64)], rng: &mut PickRng) -> Vec<DrawStep<K>> {
    let mut remaining: Vec<(usize, u64)> = candidates
        .iter()
        .enumerate()
        .filter(|(_, (_, w))| *w > 0)
        .map(|(i, (_, w))| (i, *w))
        .collect();
    let mut steps = Vec::with_capacity(remaining.len());
    while let Some(step) = draw(&remaining, rng) {
        let index = step.picked;
        remaining.retain(|(i, _)| *i != index);
        steps.push(DrawStep {
            total: step.total,
            roll: step.roll,
            picked: candidates[index].0.clone(),
        });
    }
    steps
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn zero_total_draws_nothing() {
        let mut rng = PickRng::from_seed(1);
        assert!(draw::<&str>(&[], &mut rng).is_none());
        assert!(draw(&[("a", 0), ("b", 0)], &mut rng).is_none());
        assert!(draw_order(&[("a", 0)], &mut rng).is_empty());
    }

    #[test]
    fn a_zero_weight_candidate_is_never_drawn() {
        let mut rng = PickRng::from_seed(7);
        for _ in 0..2_000 {
            let order = draw_order(&[("off", 0), ("a", 3), ("b", 1)], &mut rng);
            assert_eq!(order.len(), 2);
            assert!(order.iter().all(|s| s.picked != "off"));
        }
    }

    #[test]
    fn the_same_seed_replays_the_same_draw() {
        let table = [("claude/opus", 80), ("claude/fable", 10), ("codex", 10)];
        let a = draw_order(&table, &mut PickRng::from_seed(42));
        let b = draw_order(&table, &mut PickRng::from_seed(42));
        assert_eq!(a, b);
        assert_eq!(a[0].total, 100);
        assert!(a[0].roll < 100);
    }

    #[test]
    fn first_step_frequencies_track_the_weights() {
        let table = [("a", 80u64), ("b", 10), ("c", 10)];
        let mut rng = PickRng::from_seed(11_726);
        let n = 20_000;
        let mut a = 0;
        for _ in 0..n {
            if draw(&table, &mut rng).unwrap().picked == "a" {
                a += 1;
            }
        }
        let share = f64::from(a) / f64::from(n);
        assert!((share - 0.8).abs() < 0.02, "share {share}");
    }
}
