#!/bin/sh
# tests/parity/diff.sh <go-out> <rust-out>: compares only the operations listed in ops.txt. For
# each op, the two trees must hold the same set of <input>.<op>.out / .err files with the same
# bytes. Prints "op N files" per op and a total. Exits 1 on any difference (the count per op, and
# for each op up to 4 samples with both sides: the text of a small file, else its size and the
# first differing byte), or when a listed op has no Go output (unless DIFF_ALLOW_EMPTY=1, which the
# subset run of parity-arm64 sets). With no ops listed it prints "0 ops enabled" and exits 0.
# Known gaps: an output file named in known-gaps.txt (one name per line, # comments; each line
# says which docs/needs-from-pdf-rs.md item it waits for) may differ and is only counted. A listed
# file that exists on either side and is now equal fails the run ("gap fixed"), so the list stays
# exact. Before the samples, every differing file name is printed once.
set -eu
export LC_ALL=C
[ $# -eq 2 ] || { echo "usage: diff.sh <go-out> <rust-out>" >&2; exit 2; }
go=$1
rs=$2
ops=$(dirname "$0")/ops.txt
list=$(sed -e 's/#.*//' "$ops" | tr -s ' \t' '\n\n' | grep -v '^$' || true)
if [ -z "$list" ]; then
  echo "0 ops enabled"
  exit 0
fi
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
: >"$t/bad"
: >"$t/raw"
gapfile=$(dirname "$0")/known-gaps.txt
if [ -f "$gapfile" ]; then
  sed -e 's/#.*//' -e 's/[[:space:]]*$//' "$gapfile" | grep -v '^$' | sort -u >"$t/gaps" || true
else
  : >"$t/gaps"
fi
names() { # dir op: the output files of one op, sorted
  ls -1 "$1" | while IFS= read -r f; do
    case $f in *."$2".out | *."$2".err) echo "$f" ;; esac
  done | sort
}
n_ops=0
total=0
for op in $list; do
  names "$go" "$op" >"$t/go"
  names "$rs" "$op" >"$t/rs"
  n=$(wc -l <"$t/go" | tr -d ' ')
  if [ "$n" -eq 0 ] && [ "${DIFF_ALLOW_EMPTY:-}" != 1 ]; then
    echo "$op: no Go outputs" | tee -a "$t/bad"
  fi
  : >"$t/bad.$op"
  comm -3 "$t/go" "$t/rs" | tr -d '\t' >>"$t/bad.$op" || true
  comm -12 "$t/go" "$t/rs" | while IFS= read -r f; do
    cmp -s "$go/$f" "$rs/$f" || echo "$f" >>"$t/bad.$op"
  done
  cat "$t/bad.$op" >>"$t/raw"
  if [ -s "$t/gaps" ]; then
    grep -vxFf "$t/gaps" "$t/bad.$op" >"$t/keep.$op" || true
    mv "$t/keep.$op" "$t/bad.$op"
  fi
  cat "$t/bad.$op" >>"$t/bad"
  echo "$op $n files"
  n_ops=$((n_ops + 1))
  total=$((total + n))
done
echo "$n_ops ops enabled, $total files compared"
gap_hit=0
if [ -s "$t/gaps" ]; then
  sort -u "$t/raw" >"$t/raw.s"
  while IFS= read -r g; do
    if grep -qxF "$g" "$t/raw.s"; then
      gap_hit=$((gap_hit + 1))
    elif [ -e "$go/$g" ] || [ -e "$rs/$g" ]; then
      echo "gap fixed: $g (remove it from known-gaps.txt)" | tee -a "$t/bad"
    fi
  done <"$t/gaps"
fi
echo "known gaps: $gap_hit of $(wc -l <"$t/gaps" | tr -d ' ') listed files differ"
if [ -s "$t/bad" ]; then
  echo "== every differing file"
  sort -u "$t/bad"
  echo "DIFFERENCES: $(wc -l <"$t/bad" | tr -d ' ')"
  notbin() { # file: at most 64 KiB and no NUL byte
    n=$(wc -c <"$1")
    [ "$n" -le 65536 ] && [ "$(LC_ALL=C tr -d '\000' <"$1" | wc -c)" -eq "$n" ]
  }
  show() { # side file
    if [ ! -f "$1" ]; then
      echo "missing"
    elif [ "$(wc -c <"$1")" -le 160 ] && ! LC_ALL=C grep -q '[^[:print:][:space:]]' "$1"; then
      tr '\n' '|' <"$1"
      echo
    else
      echo "$(wc -c <"$1" | tr -d ' ') bytes, head $(head -c 40 "$1" | od -An -c | tr -s ' \n' '  ')"
    fi
  }
  for op in $list; do
    c=$(wc -l <"$t/bad.$op" | tr -d ' ')
    [ "$c" -eq 0 ] || echo "count $op: $c differ"
  done
  for op in $list; do
    c=$(wc -l <"$t/bad.$op" | tr -d ' ')
    [ "$c" -eq 0 ] && continue
    echo "== $op: $c differ"
    step=$(((c + 5) / 6))
    awk -v s="$step" 'NR % s == 1 || s == 1' "$t/bad.$op" | head -6 | while IFS= read -r f; do
      echo "  $f"
      echo "    go:   $(show "$go/$f")"
      echo "    rust: $(show "$rs/$f")"
      if [ -f "$go/$f" ] && [ -f "$rs/$f" ]; then
        if notbin "$go/$f" && notbin "$rs/$f"; then
          diff "$go/$f" "$rs/$f" | head -14 | cut -c1-200 | sed 's/^/    | /'
        else
          off=$(cmp "$go/$f" "$rs/$f" 2>/dev/null | sed -n 's/.*differ: [a-z]* \([0-9]*\).*/\1/p')
          off=${off:-1}
          from=$((off > 100 ? off - 100 : 0))
          echo "    first difference at byte $off"
          echo "    go:   $(dd if="$go/$f" bs=1 skip="$from" count=240 2>/dev/null | LC_ALL=C tr -c '[:print:]' '.')"
          echo "    rust: $(dd if="$rs/$f" bs=1 skip="$from" count=240 2>/dev/null | LC_ALL=C tr -c '[:print:]' '.')"
        fi
      fi
    done
  done
  exit 1
fi
