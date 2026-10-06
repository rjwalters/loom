//! Go's `sort.Slice`, ported line for line (#10516).
//!
//! `gh pr checks` orders its rows with two `sort.Slice` calls, and the second
//! one's `less` is not a strict weak ordering (rows in different non-`fail`
//! buckets compare as neither-less), so *which* permutation `gh` prints is a
//! property of Go's exact algorithm, not of the comparator. To reproduce that
//! output byte for byte the front runs the same algorithm: pattern-defeating
//! quicksort from `src/sort/zsortfunc.go` (`pdqsort_func` and helpers),
//! unchanged from Go 1.19 through go1.27.1 — the toolchain `gh` 2.100.0 is
//! built with. Indices are signed, as in Go, so every loop bound (including
//! `partialInsertionSort`'s `j >= 1`, which can walk below `a`) is kept
//! verbatim. Differential vectors produced by the real Go implementation pin
//! it (`go_sort_tests`).

/// Go's `sort.Slice(v, less)`: sorts `v` in place, not stably.
pub fn sort_slice<T>(v: &mut [T], less: impl Fn(&T, &T) -> bool) {
    let n = v.len() as isize;
    let limit = bits_len(v.len() as u64);
    let mut d = Data { v, less: &less };
    pdqsort(&mut d, 0, n, limit);
}

struct Data<'a, T, F> {
    v: &'a mut [T],
    less: &'a F,
}

impl<T, F: Fn(&T, &T) -> bool> Data<'_, T, F> {
    #[allow(clippy::cast_sign_loss)]
    fn less(&self, i: isize, j: isize) -> bool {
        (self.less)(&self.v[i as usize], &self.v[j as usize])
    }

    #[allow(clippy::cast_sign_loss)]
    fn swap(&mut self, i: isize, j: isize) {
        self.v.swap(i as usize, j as usize);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hint {
    Unknown,
    Increasing,
    Decreasing,
}

/// `math/bits.Len`.
fn bits_len(x: u64) -> isize {
    (64 - x.leading_zeros()) as isize
}

fn insertion_sort<T, F: Fn(&T, &T) -> bool>(d: &mut Data<'_, T, F>, a: isize, b: isize) {
    for i in a + 1..b {
        let mut j = i;
        while j > a && d.less(j, j - 1) {
            d.swap(j, j - 1);
            j -= 1;
        }
    }
}

fn sift_down<T, F: Fn(&T, &T) -> bool>(
    d: &mut Data<'_, T, F>,
    lo: isize,
    hi: isize,
    first: isize,
) {
    let mut root = lo;
    loop {
        let mut child = 2 * root + 1;
        if child >= hi {
            break;
        }
        if child + 1 < hi && d.less(first + child, first + child + 1) {
            child += 1;
        }
        if !d.less(first + root, first + child) {
            return;
        }
        d.swap(first + root, first + child);
        root = child;
    }
}

fn heap_sort<T, F: Fn(&T, &T) -> bool>(d: &mut Data<'_, T, F>, a: isize, b: isize) {
    let first = a;
    let lo = 0;
    let hi = b - a;
    let mut i = (hi - 1) / 2;
    while i >= 0 {
        sift_down(d, i, hi, first);
        i -= 1;
    }
    let mut i = hi - 1;
    while i >= 0 {
        d.swap(first, first + i);
        sift_down(d, lo, i, first);
        i -= 1;
    }
}

fn pdqsort<T, F: Fn(&T, &T) -> bool>(
    d: &mut Data<'_, T, F>,
    mut a: isize,
    mut b: isize,
    mut limit: isize,
) {
    const MAX_INSERTION: isize = 12;
    let mut was_balanced = true;
    let mut was_partitioned = true;
    loop {
        let length = b - a;
        if length <= MAX_INSERTION {
            insertion_sort(d, a, b);
            return;
        }
        if limit == 0 {
            heap_sort(d, a, b);
            return;
        }
        if !was_balanced {
            break_patterns(d, a, b);
            limit -= 1;
        }
        let (mut pivot, mut hint) = choose_pivot(d, a, b);
        if hint == Hint::Decreasing {
            reverse_range(d, a, b);
            pivot = (b - 1) - (pivot - a);
            hint = Hint::Increasing;
        }
        if was_balanced
            && was_partitioned
            && hint == Hint::Increasing
            && partial_insertion_sort(d, a, b)
        {
            return;
        }
        if a > 0 && !d.less(a - 1, pivot) {
            a = partition_equal(d, a, b, pivot);
            continue;
        }
        let (mid, already) = partition(d, a, b, pivot);
        was_partitioned = already;
        let (left, right) = (mid - a, b - mid);
        let balance_threshold = length / 8;
        if left < right {
            was_balanced = left >= balance_threshold;
            pdqsort(d, a, mid, limit);
            a = mid + 1;
        } else {
            was_balanced = right >= balance_threshold;
            pdqsort(d, mid + 1, b, limit);
            b = mid;
        }
    }
}

fn partition<T, F: Fn(&T, &T) -> bool>(
    d: &mut Data<'_, T, F>,
    a: isize,
    b: isize,
    pivot: isize,
) -> (isize, bool) {
    d.swap(a, pivot);
    let (mut i, mut j) = (a + 1, b - 1);
    while i <= j && d.less(i, a) {
        i += 1;
    }
    while i <= j && !d.less(j, a) {
        j -= 1;
    }
    if i > j {
        d.swap(j, a);
        return (j, true);
    }
    d.swap(i, j);
    i += 1;
    j -= 1;
    loop {
        while i <= j && d.less(i, a) {
            i += 1;
        }
        while i <= j && !d.less(j, a) {
            j -= 1;
        }
        if i > j {
            break;
        }
        d.swap(i, j);
        i += 1;
        j -= 1;
    }
    d.swap(j, a);
    (j, false)
}

fn partition_equal<T, F: Fn(&T, &T) -> bool>(
    d: &mut Data<'_, T, F>,
    a: isize,
    b: isize,
    pivot: isize,
) -> isize {
    d.swap(a, pivot);
    let (mut i, mut j) = (a + 1, b - 1);
    loop {
        while i <= j && !d.less(a, i) {
            i += 1;
        }
        while i <= j && d.less(a, j) {
            j -= 1;
        }
        if i > j {
            break;
        }
        d.swap(i, j);
        i += 1;
        j -= 1;
    }
    i
}

fn partial_insertion_sort<T, F: Fn(&T, &T) -> bool>(
    d: &mut Data<'_, T, F>,
    a: isize,
    b: isize,
) -> bool {
    const MAX_STEPS: usize = 5;
    const SHORTEST_SHIFTING: isize = 50;
    let mut i = a + 1;
    for _ in 0..MAX_STEPS {
        while i < b && !d.less(i, i - 1) {
            i += 1;
        }
        if i == b {
            return true;
        }
        if b - a < SHORTEST_SHIFTING {
            return false;
        }
        d.swap(i, i - 1);
        if i - a >= 2 {
            let mut j = i - 1;
            while j >= 1 {
                if !d.less(j, j - 1) {
                    break;
                }
                d.swap(j, j - 1);
                j -= 1;
            }
        }
        if b - i >= 2 {
            let mut j = i + 1;
            while j < b {
                if !d.less(j, j - 1) {
                    break;
                }
                d.swap(j, j - 1);
                j += 1;
            }
        }
    }
    false
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
fn break_patterns<T, F: Fn(&T, &T) -> bool>(d: &mut Data<'_, T, F>, a: isize, b: isize) {
    let length = b - a;
    if length >= 8 {
        let mut random = length as u64;
        let modulus = 1u64 << bits_len(length as u64);
        let mut idx = a + (length / 4) * 2 - 1;
        while idx <= a + (length / 4) * 2 + 1 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let mut other = (random & (modulus - 1)) as isize;
            if other >= length {
                other -= length;
            }
            d.swap(idx, a + other);
            idx += 1;
        }
    }
}

fn choose_pivot<T, F: Fn(&T, &T) -> bool>(
    d: &mut Data<'_, T, F>,
    a: isize,
    b: isize,
) -> (isize, Hint) {
    const SHORTEST_NINTHER: isize = 50;
    const MAX_SWAPS: usize = 4 * 3;
    let l = b - a;
    let mut swaps = 0usize;
    let mut i = a + l / 4;
    let mut j = a + l / 4 * 2;
    let mut k = a + l / 4 * 3;
    if l >= 8 {
        if l >= SHORTEST_NINTHER {
            i = median_adjacent(d, i, &mut swaps);
            j = median_adjacent(d, j, &mut swaps);
            k = median_adjacent(d, k, &mut swaps);
        }
        j = median(d, i, j, k, &mut swaps);
    }
    match swaps {
        0 => (j, Hint::Increasing),
        MAX_SWAPS => (j, Hint::Decreasing),
        _ => (j, Hint::Unknown),
    }
}

fn order2<T, F: Fn(&T, &T) -> bool>(
    d: &Data<'_, T, F>,
    a: isize,
    b: isize,
    swaps: &mut usize,
) -> (isize, isize) {
    if d.less(b, a) {
        *swaps += 1;
        return (b, a);
    }
    (a, b)
}

fn median<T, F: Fn(&T, &T) -> bool>(
    d: &Data<'_, T, F>,
    a: isize,
    b: isize,
    c: isize,
    swaps: &mut usize,
) -> isize {
    let (a, b) = order2(d, a, b, swaps);
    let (b, _c) = order2(d, b, c, swaps);
    let (_a, b) = order2(d, a, b, swaps);
    b
}

fn median_adjacent<T, F: Fn(&T, &T) -> bool>(
    d: &Data<'_, T, F>,
    a: isize,
    swaps: &mut usize,
) -> isize {
    median(d, a - 1, a, a + 1, swaps)
}

fn reverse_range<T, F: Fn(&T, &T) -> bool>(d: &mut Data<'_, T, F>, a: isize, b: isize) {
    let (mut i, mut j) = (a, b - 1);
    while i < j {
        d.swap(i, j);
        i += 1;
        j -= 1;
    }
}

#[cfg(test)]
#[path = "go_sort_tests.rs"]
mod tests;
