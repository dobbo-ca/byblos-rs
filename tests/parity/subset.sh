#!/bin/sh
# tests/parity/subset.sh <inputs> <go-out> <n> <sub-in> <sub-go>: copies every n-th input (sorted by
# name) to <sub-in> (plus the jbig2 globals file), and the Go outputs of the ops listed in ops.txt for those inputs to <sub-go>.
# Used by the parity-arm64 stage, which runs the Rust oracle natively on aarch64 on the subset only.
set -eu
export LC_ALL=C
inputs=$1 go=$2 n=$3 sub_in=$4 sub_go=$5
ops=$(dirname "$0")/ops.txt
list=$(sed -e 's/#.*//' "$ops" | tr -s ' \t' '\n\n' | grep -v '^$' || true)
mkdir -p "$sub_in" "$sub_go"
# fx-jbig2-symbol-globals.jb2 is always kept: jbig2-decode-globals pairs every .jb2 with it.
ls -1 "$inputs" | sort | awk -v n="$n" 'NR % n == 1 || n == 1 || $0 == "fx-jbig2-symbol-globals.jb2"' | while IFS= read -r f; do
  cp "$inputs/$f" "$sub_in/$f"
  for op in $list; do
    for e in out err; do
      if [ -f "$go/$f.$op.$e" ]; then cp "$go/$f.$op.$e" "$sub_go/"; fi
    done
  done
done
echo "$(ls "$sub_in" | wc -l) of $(ls "$inputs" | wc -l) inputs, $(ls "$sub_go" | wc -l) Go outputs"
