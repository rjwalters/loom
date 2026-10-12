//! How many backoff attempts the stale-`mergeable=false` recheck actually
//! cost, for the merge-admission telemetry record (#6978, #8191 slice; the
//! `[[ "$_MSM_REASON" =~ recheck\ \#([0-9]+) ]]` block in `merge-pr.sh` before
//! this port).
//!
//! The recheck's decision text embeds `recheck #N` only on the path where the
//! recheck resolved `mergeable=true` mid-loop; every other path consumed the
//! whole configured budget. Telemetry only: it reads the already-computed
//! reason and never changes the decision.
//!
//! Semantics preserved from bash `=~` (POSIX ERE, unanchored): the LEFTMOST
//! occurrence of `recheck #` that is immediately followed by at least one ASCII
//! digit wins, and the digit run is taken greedily. An occurrence not followed
//! by a digit does not stop the scan. The digits are returned verbatim (as
//! text), so an over-long run cannot overflow or be normalised.

const NEEDLE: &str = "recheck #";

/// The retries-used figure: the digits after the leftmost `recheck #<digits>`
/// in `reason`, else `configured` unchanged.
#[must_use]
pub fn retries_used(reason: &str, configured: &str) -> String {
    let mut from = 0;
    while let Some(rel) = reason[from..].find(NEEDLE) {
        let start = from + rel + NEEDLE.len();
        let digits: String = reason[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return digits;
        }
        from = start;
    }
    configured.to_string()
}

#[cfg(test)]
mod tests;
