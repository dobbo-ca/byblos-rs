# Threading

`Limits::max_threads` is the most OS threads one call uses, the calling thread included. 1 spawns
no thread and touches no pool; 0 counts as 1. The default is
`std::thread::available_parallelism()`, read once. The memory budget (`max_alloc`) covers all
threads together, and the cancel flag stops all of them. The output is byte-identical for every
value of `max_threads`: the parity job runs the Rust oracle with `-threads 1` and with the
default, and both outputs must equal Go's.

## Phase 1: serial

byblos starts no goroutines in the in-scope packages, so byblos-rs is serial in its own code. The
value of `max_threads` is passed to pdf-rs and jpeg-rs, which may use it. byblos-rs has no pool
of its own and names no thread crate (rayon and its crossbeam dependencies are in `Cargo.lock`
only through pdf-rs and jpeg-rs, see dependencies.md); a lazy shared pool is not
created.

## Nested crates

A call hands pdf-rs, jpeg-rs and deflate-rs the budget it has left: one `pdf::Mem` ledger
(`Limits::mem()`) and the `max_threads` it does not use itself. Since byblos runs nothing in
parallel in phase 1, they get all of it.

## Later, if the benchmark needs it

Each candidate is byte-identical to serial and stays inside `std::thread::scope` over disjoint
`chunks_mut` slices, with no `unsafe`:

- the Sauvola threshold pass, by row bands (the integral image stays serial);
- downsample, by output rows (x/image scales rows independently);
- render, by scanline bands (the edge list is shared and read-only);
- JBIG2 encode, only by stripe (one MQ stream is serial);
- Lloyd assignment in quantize, only if the sums stay in Go's order.

A caller-supplied pool, if one is ever added, wins over the crate's lazy shared pool.
