#!/bin/sh
# Time per class, Go and Rust on the same files, as two Markdown tables: tests/parity/bench.sh.
# Needs `oracle` (Go) and `bench` (Rust) on the PATH and the inputs in /bench-in (the Dockerfile's
# bench stage); run from the repo root. The classes and what each times are in docs/benchmarks.md.
#
#   1. Go with its default GOMAXPROCS against Rust with its default max_threads.
#   2. Go with GOMAXPROCS=1 against Rust with -threads 1.
#
# Not a gate: it always exits 0. A ratio (Rust / Go) over 1.10 is marked SLOW.
set -eu
export LC_ALL=C
d=${BENCH_IN:-/bench-in}
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
grep -m1 'model name' /proc/cpuinfo || true

# Best (lowest) of three runs per class and side: the machine is shared, and one run can be slow.
best() {
  awk '!($1 in m) || $2 < m[$1] { m[$1] = $2; f[$1] = $3 } END { for (k in m) print k, m[k], f[k] }' "$@"
}
for i in 1 2 3; do
  oracle -bench "$d" 0 >"$t/go-def-$i"
  oracle -bench "$d" 1 >"$t/go-one-$i"
  bench "$d" >"$t/rs-def-$i"
  bench -threads 1 "$d" >"$t/rs-one-$i"
done
for k in go-def go-one rs-def rs-one; do best "$t/$k"-? >"$t/$k"; done

# table TITLE GO RUST: the class order of the Go output, then the classes only Rust timed.
table() {
  echo "### $1"
  echo
  echo "| class | Go ms | Rust ms | ratio | |"
  echo "|---|---|---|---|---|"
  [ -s "$2" ] || echo "(no Go output)"
  awk '
    FNR == NR { g[$1] = $2; gf[$1] = $3; if (!($1 in seen)) { seen[$1] = 1; order[++n] = $1 } next }
    { r[$1] = $2; rf[$1] = $3; if (!($1 in seen)) { seen[$1] = 1; order[++n] = $1 } }
    END {
      for (i = 1; i <= n; i++) {
        k = order[i]
        if (k in g && k in r && gf[k] != rf[k]) {
          printf "| %s | %.3f | %.3f | - | FILES %s vs %s |\n", k, g[k] / 1e6, r[k] / 1e6, gf[k], rf[k]
        } else if (k in g && k in r && g[k] > 0) {
          q = r[k] / g[k]
          printf "| %s | %.3f | %.3f | %.2f | %s |\n", k, g[k] / 1e6, r[k] / 1e6, q, (q > 1.10 ? "SLOW" : "")
        } else {
          printf "| %s | %s | %s | - | |\n", k, (k in g ? sprintf("%.3f", g[k] / 1e6) : "-"), (k in r ? sprintf("%.3f", r[k] / 1e6) : "-")
        }
      }
    }' "$2" "$3"
  echo
}
table "Go default GOMAXPROCS against Rust default max_threads" "$t/go-def" "$t/rs-def"
table "Go GOMAXPROCS=1 against Rust max_threads 1" "$t/go-one" "$t/rs-one"
