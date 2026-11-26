#![no_main]
use byblos::go::sort::slice_by;
use libfuzzer_sys::fuzz_target;

// The Go sort port against std. The bytes sorted by value must equal std's result (which checks
// sorted and a permutation at once). Then (key, index) pairs sorted by key alone: the keys must
// come out non-decreasing and the indices must still be a permutation of 0..n (the sort is not
// stable, so the order of equal keys is not compared).
fuzz_target!(|data: &[u8]| {
    let mut got = data.to_vec();
    slice_by(&mut got, |a, b| a < b);
    let mut want = data.to_vec();
    want.sort_unstable();
    assert_eq!(got, want);

    let mut pairs: Vec<(u8, usize)> = data.iter().copied().zip(0..).collect();
    slice_by(&mut pairs, |a, b| a.0 < b.0);
    assert!(pairs.windows(2).all(|w| w[0].0 <= w[1].0));
    let mut idx: Vec<usize> = pairs.iter().map(|p| p.1).collect();
    idx.sort_unstable();
    assert!(idx.iter().copied().eq(0..data.len()));
});
