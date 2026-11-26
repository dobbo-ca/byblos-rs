//! Go's `sort.Slice`: the unstable pattern-defeating quicksort of Go 1.27 (`sort/zsortfunc.go`,
//! `sort/sort.go`, `sort/slice.go`), with the same comparisons and swaps in the same order.
//!
//! The order of equal elements depends on that sequence, and byblos sorts keys that tie
//! (the median-cut axis sort, render edge lists), so `slice::sort_unstable_by` cannot stand in.
//! The code keeps Go's index arithmetic, including the shifting loops of
//! `partialInsertionSort_func`, which compare absolute positions `j` and `j-1`.
//!
//! Go BSD licence: see NOTICE.

/// `sort.Slice(v, less)` where `less(i, j)` is `less(&v[i], &v[j])`.
pub fn slice_by<T, F>(v: &mut [T], less: F)
where
    F: FnMut(&T, &T) -> bool,
{
    let length = v.len();
    let limit = bit_len(length);
    let mut d = Data { v, less };
    pdqsort(&mut d, 0, length, limit);
}

/// `bits.Len(uint(n))`.
fn bit_len(n: usize) -> usize {
    (usize::BITS - n.leading_zeros()) as usize
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hint {
    Unknown,
    Increasing,
    Decreasing,
}

struct Data<'a, T, F> {
    v: &'a mut [T],
    less: F,
}

impl<T, F: FnMut(&T, &T) -> bool> Data<'_, T, F> {
    fn less(&mut self, i: usize, j: usize) -> bool {
        (self.less)(&self.v[i], &self.v[j])
    }

    fn swap(&mut self, i: usize, j: usize) {
        self.v.swap(i, j);
    }
}

fn insertion_sort<T, F: FnMut(&T, &T) -> bool>(d: &mut Data<T, F>, a: usize, b: usize) {
    for i in a + 1..b {
        let mut j = i;
        while j > a && d.less(j, j - 1) {
            d.swap(j, j - 1);
            j -= 1;
        }
    }
}

fn sift_down<T, F: FnMut(&T, &T) -> bool>(d: &mut Data<T, F>, lo: usize, hi: usize, first: usize) {
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

fn heap_sort<T, F: FnMut(&T, &T) -> bool>(d: &mut Data<T, F>, a: usize, b: usize) {
    let first = a;
    let lo = 0;
    let hi = b - a;

    // Build heap with greatest element at top. Go counts i down to 0 inclusive.
    let mut i = (hi - 1) / 2 + 1;
    while i > 0 {
        i -= 1;
        sift_down(d, i, hi, first);
    }

    // Pop elements, largest first, into end of data.
    let mut i = hi;
    while i > 0 {
        i -= 1;
        d.swap(first, first + i);
        sift_down(d, lo, i, first);
    }
}

fn pdqsort<T, F: FnMut(&T, &T) -> bool>(
    d: &mut Data<T, F>,
    mut a: usize,
    mut b: usize,
    mut limit: usize,
) {
    const MAX_INSERTION: usize = 12;

    let mut was_balanced = true;
    let mut was_partitioned = true;

    loop {
        let length = b - a;

        if length <= MAX_INSERTION {
            insertion_sort(d, a, b);
            return;
        }

        // Fall back to heapsort if too many bad choices were made.
        if limit == 0 {
            heap_sort(d, a, b);
            return;
        }

        // If the last partitioning was imbalanced, break patterns.
        if !was_balanced {
            break_patterns(d, a, b);
            limit -= 1;
        }

        let (mut pivot, mut hint) = choose_pivot(d, a, b);
        if hint == Hint::Decreasing {
            reverse_range(d, a, b);
            // The chosen pivot was pivot-a elements after the start; after reversing it is
            // pivot-a elements before the end.
            pivot = (b - 1) - (pivot - a);
            hint = Hint::Increasing;
        }

        // The slice is likely already sorted.
        if was_balanced
            && was_partitioned
            && hint == Hint::Increasing
            && partial_insertion_sort(d, a, b)
        {
            return;
        }

        // Probably many duplicates: partition into elements equal to and greater than the pivot.
        if a > 0 && !d.less(a - 1, pivot) {
            let mid = partition_equal(d, a, b, pivot);
            a = mid;
            continue;
        }

        let (mid, already_partitioned) = partition(d, a, b, pivot);
        was_partitioned = already_partitioned;

        let (left_len, right_len) = (mid - a, b - mid);
        let balance_threshold = length / 8;
        if left_len < right_len {
            was_balanced = left_len >= balance_threshold;
            pdqsort(d, a, mid, limit);
            a = mid + 1;
        } else {
            was_balanced = right_len >= balance_threshold;
            pdqsort(d, mid + 1, b, limit);
            b = mid;
        }
    }
}

fn partition<T, F: FnMut(&T, &T) -> bool>(
    d: &mut Data<T, F>,
    a: usize,
    b: usize,
    pivot: usize,
) -> (usize, bool) {
    d.swap(a, pivot);
    // i and j are inclusive of the elements remaining to be partitioned. j can reach a, never
    // below it, because i starts at a+1 and the loops stop at i > j.
    let mut i = a + 1;
    let mut j = b - 1;

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

fn partition_equal<T, F: FnMut(&T, &T) -> bool>(
    d: &mut Data<T, F>,
    a: usize,
    b: usize,
    pivot: usize,
) -> usize {
    d.swap(a, pivot);
    let mut i = a + 1;
    let mut j = b - 1;

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

fn partial_insertion_sort<T, F: FnMut(&T, &T) -> bool>(
    d: &mut Data<T, F>,
    a: usize,
    b: usize,
) -> bool {
    const MAX_STEPS: usize = 5;
    const SHORTEST_SHIFTING: usize = 50;
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

        // Shift the smaller one to the left. Go compares absolute positions j and j-1 down to 1.
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
        // Shift the greater one to the right.
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

fn break_patterns<T, F: FnMut(&T, &T) -> bool>(d: &mut Data<T, F>, a: usize, b: usize) {
    let length = b - a;
    if length >= 8 {
        let mut random = length as u64;
        let modulus = 1usize << bit_len(length);

        let mut idx = a + (length / 4) * 2 - 1;
        while idx <= a + (length / 4) * 2 + 1 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let mut other = (random as usize) & (modulus - 1);
            if other >= length {
                other -= length;
            }
            d.swap(idx, a + other);
            idx += 1;
        }
    }
}

fn choose_pivot<T, F: FnMut(&T, &T) -> bool>(
    d: &mut Data<T, F>,
    a: usize,
    b: usize,
) -> (usize, Hint) {
    const SHORTEST_NINTHER: usize = 50;
    const MAX_SWAPS: usize = 4 * 3;

    let l = b - a;

    let mut swaps = 0usize;
    let mut i = a + l / 4;
    let mut j = a + l / 4 * 2;
    let mut k = a + l / 4 * 3;

    if l >= 8 {
        if l >= SHORTEST_NINTHER {
            // Tukey ninther method.
            i = median_adjacent(d, i, &mut swaps);
            j = median_adjacent(d, j, &mut swaps);
            k = median_adjacent(d, k, &mut swaps);
        }
        // Find the median among i, j, k and store it into j.
        j = median(d, i, j, k, &mut swaps);
    }

    match swaps {
        0 => (j, Hint::Increasing),
        MAX_SWAPS => (j, Hint::Decreasing),
        _ => (j, Hint::Unknown),
    }
}

fn order2<T, F: FnMut(&T, &T) -> bool>(
    d: &mut Data<T, F>,
    a: usize,
    b: usize,
    swaps: &mut usize,
) -> (usize, usize) {
    if d.less(b, a) {
        *swaps += 1;
        return (b, a);
    }
    (a, b)
}

fn median<T, F: FnMut(&T, &T) -> bool>(
    d: &mut Data<T, F>,
    a: usize,
    b: usize,
    c: usize,
    swaps: &mut usize,
) -> usize {
    let (a, b) = order2(d, a, b, swaps);
    let (b, _c) = order2(d, b, c, swaps);
    let (_a, b) = order2(d, a, b, swaps);
    b
}

fn median_adjacent<T, F: FnMut(&T, &T) -> bool>(
    d: &mut Data<T, F>,
    a: usize,
    swaps: &mut usize,
) -> usize {
    median(d, a - 1, a, a + 1, swaps)
}

fn reverse_range<T, F: FnMut(&T, &T) -> bool>(d: &mut Data<T, F>, a: usize, b: usize) {
    let mut i = a;
    let mut j = b - 1;
    while i < j {
        d.swap(i, j);
        i += 1;
        j -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn check_sorted(v: &[i64]) {
        assert!(v.windows(2).all(|w| w[0] <= w[1]), "not sorted: {v:?}");
    }

    #[test]
    fn sorts_every_pattern_and_size() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for n in [
            0usize, 1, 2, 3, 12, 13, 24, 49, 50, 51, 100, 257, 1000, 5000,
        ] {
            let patterns: Vec<Vec<i64>> = vec![
                (0..n as i64).collect(),
                (0..n as i64).rev().collect(),
                vec![7; n],
                (0..n).map(|i| (i % 5) as i64).collect(),
                (0..n).map(|_| (rng.next() % 1000) as i64).collect(),
                (0..n).map(|_| (rng.next() % 3) as i64).collect(),
                (0..n)
                    .map(|i| if i % 2 == 0 { i as i64 } else { -(i as i64) })
                    .collect(),
            ];
            for mut v in patterns {
                let mut want = v.clone();
                want.sort_unstable();
                slice_by(&mut v, |a, b| a < b);
                check_sorted(&v);
                assert_eq!(v, want, "n = {n}");
            }
        }
    }

    // Up to 12 elements Go runs insertion sort, which is stable: ties keep their order.
    #[test]
    fn short_ties_keep_order() {
        let mut v: Vec<(i32, char)> =
            vec![(2, 'a'), (1, 'b'), (2, 'c'), (1, 'd'), (3, 'e'), (1, 'f')];
        slice_by(&mut v, |a, b| a.0 < b.0);
        let tags: String = v.iter().map(|x| x.1).collect();
        assert_eq!(tags, "bdface");
    }

    // Hand trace of pdqsort on 13 elements (one over the insertion limit), keys
    // [5,5,5,5,5,5,5,5,5,5,5,5,5] with tags 0..13. choosePivot: l=13, i=a+3, j=a+6, k=a+9;
    // median(3,6,9): no `less` is true, 0 swaps -> increasing hint, pivot 6. The slice was
    // balanced and partitioned and the hint is increasing, so partialInsertionSort runs: no
    // `less(i,i-1)` is true, i reaches b, and it returns true. Nothing moves.
    #[test]
    fn all_equal_13_untouched() {
        let mut v: Vec<(i32, usize)> = (0..13).map(|i| (5, i)).collect();
        slice_by(&mut v, |a, b| a.0 < b.0);
        let tags: Vec<usize> = v.iter().map(|x| x.1).collect();
        assert_eq!(tags, (0..13).collect::<Vec<_>>());
    }

    #[test]
    fn descending_13_is_deterministic() {
        let run = || {
            let mut calls = 0usize;
            let mut v: Vec<i32> = (1..=13).rev().collect();
            slice_by(&mut v, |a, b| {
                calls += 1;
                a < b
            });
            (v, calls)
        };
        let (v1, c1) = run();
        let (v2, c2) = run();
        assert_eq!(v1, (1..=13).collect::<Vec<_>>());
        assert_eq!(v1, v2);
        assert_eq!(c1, c2);
    }

    // Tied keys over 12 elements go through partitioning: the keys come out sorted and no
    // element is lost or duplicated.
    #[test]
    fn tied_keys_partition() {
        let mut v: Vec<(i32, usize)> = (0..14).map(|i| ((i % 2) as i32, i)).collect();
        slice_by(&mut v, |a, b| a.0 < b.0);
        assert!(v.windows(2).all(|w| w[0].0 <= w[1].0));
        let mut tags: Vec<usize> = v.iter().map(|x| x.1).collect();
        tags.sort_unstable();
        assert_eq!(tags, (0..14).collect::<Vec<_>>());
    }
}
