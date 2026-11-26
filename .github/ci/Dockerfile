# CI image: every check runs inside this BuildKit build, so a GitHub-hosted runner needs only Docker.
# The aarch64 stages (arm64, parity-arm64) and the fuzz stage on aarch64 build natively on
# ubuntu-24.04-arm.
# The crate depends on four public dobbo-ca ports (pdf-rs, jpeg-rs, deflate-rs, png-rs) by git
# rev. They are fetched once in the `deps` stage; every later step runs offline.
# Every tool is installed in a stage before `COPY . .`, so a new commit does not install it again,
# and the fetch runs from the manifests and lockfiles alone, so a commit that leaves them alone
# does not fetch again.
FROM rust:1.99.1-bookworm AS toolchain
RUN rustup component add rustfmt clippy && rustup target add aarch64-unknown-linux-gnu
WORKDIR /src

# Only the manifests, with an empty target for what they name, so cargo can resolve them: the
# crate's (lib, the oracle and bench examples), and the fuzz crate's (tests/fuzz, its own lockfile,
# a stub for every [[bin]] path).
FROM toolchain AS deps
COPY Cargo.toml Cargo.lock ./
COPY tests/fuzz/Cargo.toml tests/fuzz/Cargo.lock tests/fuzz/
RUN mkdir -p src examples/oracle tests/fuzz/fuzz_targets && touch src/lib.rs examples/oracle/main.rs examples/bench.rs \
    && grep -o 'fuzz_targets/[a-z_0-9]*\.rs' tests/fuzz/Cargo.toml | sed 's|^|tests/fuzz/|' | xargs touch
RUN CARGO_NET_GIT_FETCH_WITH_CLI=true cargo fetch --locked && CARGO_NET_GIT_FETCH_WITH_CLI=true cargo fetch --locked --manifest-path tests/fuzz/Cargo.toml \
    && rm -rf src examples tests

FROM toolchain AS src
COPY --from=deps /usr/local/cargo/registry /usr/local/cargo/registry
COPY --from=deps /usr/local/cargo/git /usr/local/cargo/git
ENV CARGO_NET_OFFLINE=true
COPY . .

# The checks run in the build, so a red check fails the build; a rerun of the same source is a
# BuildKit cache hit. The target dir is a cache mount, so it persists within one builder. No SIMD here,
# so there is no scalar-tier run and no NEON module to lint. `cargo test` includes the doc-tests.
FROM src AS ci
# The target cache is shared by every branch and cargo compares mtimes: another branch's newer
# build would be reused as this one's, so every cargo step touches the sources first.
RUN --mount=type=cache,id=byblos-target,target=/src/target \
    cargo fmt --check
RUN --mount=type=cache,id=byblos-target,target=/src/target \
    find src examples tests -name "*.rs" -exec touch {} + && cargo clippy --locked --all-targets -- -D warnings
RUN --mount=type=cache,id=byblos-target,target=/src/target \
    find src examples tests -name "*.rs" -exec touch {} + && cargo test --locked
# The fuzz crate is its own workspace, so the steps above never compile it; this does (no nightly or
# sanitizer needed), so a broken target fails CI and not the weekly fuzz run.
RUN --mount=type=cache,id=byblos-target,target=/src/target \
    find src examples tests -name "*.rs" -exec touch {} + && CARGO_TARGET_DIR=/src/target/fuzz-check cargo check --locked --manifest-path tests/fuzz/Cargo.toml --bins
RUN --mount=type=cache,id=byblos-target,target=/src/target \
    find src examples tests -name "*.rs" -exec touch {} + && cargo build --locked --release --target aarch64-unknown-linux-gnu

# aarch64 tests, native on ubuntu-24.04-arm. byblos has no SIMD of its own: what is
# aarch64-specific is the NEON code of deflate-rs (adler32) and jpeg-rs, reached through the
# library's tests.
FROM src AS arm64
RUN --mount=type=cache,id=byblos-target,target=/src/target \
    find src examples tests -name "*.rs" -exec touch {} + && cargo test --locked

# Go parity: the Go oracle (tests/parity/go, vendored, no token) writes the inputs and runs every
# operation; the Rust example runs the same inputs; the two output trees must be equal for the
# operations listed in tests/parity/ops.txt. The Go side is vendored under tests/parity/go (upstream
# byblos at 086b88b, pdf-go and the public modules it builds with, all through `replace`
# directives), so the build needs no network and no token. build.sh applies the determinism patches
# to a copy of upstream, which stays verbatim in the repository.
# It copies tests/parity/go only, so BuildKit reuses the oracle until a file there changes.
FROM golang:1.27-bookworm AS go-oracle
ENV GOTOOLCHAIN=local CGO_ENABLED=0 GOFLAGS=-mod=mod GOPROXY=off
WORKDIR /src
COPY tests/parity/go tests/parity/go
RUN --mount=type=cache,id=byblos-go-build,target=/root/.cache/go-build sh tests/parity/go/build.sh /out/oracle

# It copies only the fixtures it reads, not diff.sh or ops.txt: enabling an op in ops.txt must not
# rerun the Go side (the Go outputs cover every op, the diff picks the enabled ones).
FROM go-oracle AS go-run
ENV GOMEMLIMIT=3GiB
COPY tests/data tests/data
RUN /out/oracle -gen tests/data /inputs && ls /inputs | wc -l && du -sh /inputs
# One file per (input, op): the tree is about inputs x ops files.
RUN /out/oracle /inputs /go-out && du -sh /go-out && ls /go-out | wc -l
# The run-twice check (docs/design.md section 10): Go seeds every map's iteration order per
# process, so a second run walks the maps in other orders. Any difference is a map loop that the
# determinism patch misses. It compares every output, not only the enabled ops (diff.sh does that),
# so a Go-side nondeterminism is found before the op that exposes it is enabled.
RUN /out/oracle /inputs /go-out-2 && diff -rq /go-out /go-out-2 && rm -rf /go-out-2

# The Go trees are bind mounts from go-run, not COPY, so a commit does not copy them again after
# `COPY . .`. The Rust oracle runs twice, with `-threads 1` and with the default max_threads, and
# both outputs must equal the Go tree. Each Rust output is deleted after its diff, pass or fail
# (the diff status is kept, then rm, then exit), so the layer and the cache export stay small.
FROM src AS parity
RUN --mount=type=cache,id=byblos-target,target=/src/target \
    --mount=type=bind,from=go-run,source=/inputs,target=/inputs,rw \
    --mount=type=bind,from=go-run,source=/go-out,target=/go-out,rw \
    echo "$(ls /inputs | wc -l) inputs, $(ls /go-out | wc -l) outputs" \
    && [ "$(ls /go-out | wc -l)" -gt 0 ] \
    && find src examples tests -name "*.rs" -exec touch {} + && cargo run --locked --release --example oracle -- -threads 1 /inputs /rust-out-1 \
    && { sh tests/parity/diff.sh /go-out /rust-out-1; s=$?; rm -rf /rust-out-1; [ $s -eq 0 ] || exit $s; } \
    && find src examples tests -name "*.rs" -exec touch {} + && cargo run --locked --release --example oracle -- /inputs /rust-out \
    && { sh tests/parity/diff.sh /go-out /rust-out; s=$?; rm -rf /rust-out; exit $s; }

# The oracle example run natively on aarch64 (ubuntu-24.04-arm) on every 10th input (subset.sh also
# cuts the Go tree down to those inputs). Single-threaded: the thread counts are compared in
# `parity`. DIFF_ALLOW_EMPTY=1: an op may have no input in the subset.
# byblos has no SIMD, so there is one tier; the point is the same bytes on the other architecture
# (floating point, integer widths, endianness), against the same Go tree.
FROM src AS parity-arm64
RUN --mount=type=cache,id=byblos-target,target=/src/target \
    --mount=type=bind,from=go-run,source=/inputs,target=/inputs,rw \
    --mount=type=bind,from=go-run,source=/go-out,target=/go-out,rw \
    sh tests/parity/subset.sh /inputs /go-out 10 /sub-in /sub-go \
    && find src examples tests -name "*.rs" -exec touch {} + \
    && cargo run --locked --release --example oracle -- -threads 1 /sub-in /rust-out \
    && { DIFF_ALLOW_EMPTY=1 sh tests/parity/diff.sh /sub-go /rust-out; s=$?; rm -rf /rust-out; exit $s; }

# The seeds of the data targets: the small test inputs (under 300 KB) and a sample of the oracle's
# `-gen` outputs (the mutations and the synthesized inputs), flat in /seeds/data. The `-gen` run is
# the same as go-run's, so the inputs are the same files.
FROM go-oracle AS go-gen
ENV GOMEMLIMIT=3GiB
COPY tests/data tests/data
RUN /out/oracle -gen tests/data /gen-in

FROM toolchain AS fuzz-seeds
COPY tests/data tests/data
RUN mkdir -p /seeds/data \
    && find tests/data -type f -size -300k | sort | while IFS= read -r f; do cp "$f" "/seeds/data/$(echo "$f" | tr '/ ' '__')"; done
RUN --mount=type=bind,from=go-gen,source=/gen-in,target=/gen-in,rw \
    find /gen-in -type f -size -64k | sort | awk 'NR % 25 == 0' | head -400 | while IFS= read -r f; do cp "$f" "/seeds/data/gen_$(basename "$f")"; done \
    && ls /seeds/data | wc -l && du -sh /seeds/data

# Fuzzing (fuzz.yml): nightly and cargo-fuzz, with the targets built into the image. The
# workflow runs it with `docker run`, so a BuildKit cache hit cannot skip the fuzzing. The same
# stage builds natively on x86_64 and on aarch64.
FROM toolchain AS fuzz-tools
RUN rustup toolchain install nightly --profile minimal \
    && cargo install cargo-fuzz --locked --version 0.13.1

FROM fuzz-tools AS fuzz
COPY --from=deps /usr/local/cargo/registry /usr/local/cargo/registry
COPY --from=deps /usr/local/cargo/git /usr/local/cargo/git
ENV CARGO_NET_OFFLINE=true
COPY . .
COPY --from=fuzz-seeds /seeds/data tests/fuzz/seeds/data
RUN mkdir -p tests/fuzz/corpus tests/fuzz/artifacts \
    && cargo +nightly fuzz build --fuzz-dir tests/fuzz

# Benchmark (bench.yml): the Go oracle and the Rust bench example in one image, so the two sides
# run on the same machine. /bench-in holds the generated inputs of go-gen (the same `-gen` run as go-run's, without its parity pass) without the mutations and
# without anything over 2 MiB (the bench uses neither); the rw bind mount avoids a copy of the whole
# tree. docker run, not RUN, runs tests/parity/bench.sh (docs/benchmarks.md).
FROM src AS bench
COPY --from=go-oracle /out/oracle /usr/local/bin/oracle
RUN --mount=type=bind,from=go-gen,source=/gen-in,target=/gen-in,rw \
    mkdir /bench-in && find /gen-in -maxdepth 1 -type f ! -name 'mut-*' -size -2049k -exec cp {} /bench-in/ \; \
    && ls /bench-in | wc -l && du -sh /bench-in
RUN --mount=type=cache,id=byblos-target,target=/src/target \
    find src examples tests -name "*.rs" -exec touch {} + && cargo build --locked --release --example bench && cp target/release/examples/bench /usr/local/bin/bench
