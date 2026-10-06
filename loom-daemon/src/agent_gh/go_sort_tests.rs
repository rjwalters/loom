//! Differential test: [`sort_slice`] against the real Go `sort.Slice`.
//!
//! `fixtures/pr_checks/go_sort_digests.txt` holds one FNV-1a-64 digest per
//! case of the permutation go1.27.1's `sort.Slice` produced. The inputs are
//! regenerated here from the same splitmix64 stream, so the vectors cost a
//! few KiB. The generator (run with `go run`), kept here verbatim in spirit:
//!
//! ```text
//! for c := 0; c < 400; c++ {
//!     r := rng(c); n := r.next() % 130; kind := c % 4
//!     kind 0: per element b, n, l = r.next()%5, r.next()%8, r.next()%3
//!             less = gh printTable's comparator (b == 0 is "fail")
//!     kind 1: n = r.next()%4               less = n_i < n_j
//!     kind 2: n = i, then r.next()%6 random swaps (two draws each)
//!     kind 3: n = (len - i) / 2
//!     sort.Slice(v, less); digest the original indices, 4 LE bytes each
//! }
//! ```
//!
//! Kinds 2 and 3 drive the sorted / reversed fast paths
//! (`partialInsertionSort`, `reverseRange`), kind 1 `partitionEqual`, kind 0
//! the inconsistent comparator `gh pr checks` really uses.

#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use super::sort_slice;

const DIGESTS: &str = include_str!("fixtures/pr_checks/go_sort_digests.txt");

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

#[derive(Clone, Copy)]
struct El {
    idx: u32,
    b: u64,
    n: u64,
    l: u64,
}

fn gen(c: u64) -> (u64, Vec<El>) {
    let mut r = Rng(c);
    let len = r.next() % 130;
    let kind = c % 4;
    let mut v = Vec::new();
    for i in 0..len {
        let mut e = El {
            idx: i as u32,
            b: 0,
            n: 0,
            l: 0,
        };
        match kind {
            0 => {
                e.b = r.next() % 5;
                e.n = r.next() % 8;
                e.l = r.next() % 3;
            }
            1 => e.n = r.next() % 4,
            2 => e.n = i,
            _ => e.n = (len - i) / 2,
        }
        v.push(e);
    }
    if kind == 2 {
        let mut s = r.next() % 6;
        while s > 0 && len > 1 {
            let (a, b) = ((r.next() % len) as usize, (r.next() % len) as usize);
            v.swap(a, b);
            s -= 1;
        }
    }
    (kind, v)
}

fn gh_less(a: &El, b: &El) -> bool {
    if a.b == b.b {
        if a.n == b.n {
            return a.l < b.l;
        }
        return a.n < b.n;
    }
    a.b == 0
}

#[test]
fn matches_go_sort_slice_on_every_vector() {
    let want: Vec<&str> = DIGESTS.lines().collect();
    assert_eq!(want.len(), 400);
    for (c, want) in want.iter().enumerate() {
        let (kind, mut v) = gen(c as u64);
        if kind == 0 {
            sort_slice(&mut v, gh_less);
        } else {
            sort_slice(&mut v, |a, b| a.n < b.n);
        }
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for e in &v {
            for byte in e.idx.to_le_bytes() {
                h ^= u64::from(byte);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
        assert_eq!(format!("{h:016x}"), *want, "case {c} (kind {kind}, n {})", v.len());
    }
}

#[test]
fn sorts_correctly_under_a_consistent_comparator() {
    let mut v: Vec<u32> = (0..500).map(|i| (i * 7919) % 503).collect();
    sort_slice(&mut v, |a, b| a < b);
    assert!(v.windows(2).all(|w| w[0] <= w[1]));
    let mut empty: Vec<u32> = Vec::new();
    sort_slice(&mut empty, |a, b| a < b);
    assert!(empty.is_empty());
}
