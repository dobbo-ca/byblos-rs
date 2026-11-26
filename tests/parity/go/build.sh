#!/bin/sh
# tests/parity/go/build.sh <binary>: builds the Go oracle. It copies oracle/, upstream/ and deps/
# to a temporary directory, applies patches/byblos-determinism.patch to the copy of upstream/ and
# patches/pdf-go-determinism.patch to the copy of deps/pdf-go (docs/parity.md lists every hunk), and
# builds there, so upstream/ and deps/pdf-go stay verbatim. The oracle needs the patched hooks
# (byblos.SetTimeNow, pdfcpu.TimeNow, pdfcpu.RandReader) and does not build against the plain
# snapshots. Nothing is fetched: every module is a directory replace in oracle/go.mod.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
case $1 in
/*) out=$1 ;;
*) out=$(pwd)/$1 ;;
esac
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cp -R "$here/upstream" "$here/deps" "$here/oracle" "$tmp/"
(cd "$tmp/upstream" && git apply -p1 "$here/patches/byblos-determinism.patch")
(cd "$tmp/deps/pdf-go" && git apply -p1 "$here/patches/pdf-go-determinism.patch")
cd "$tmp/oracle"
GOFLAGS=-mod=mod GOPROXY=off GOTOOLCHAIN=local CGO_ENABLED=0 go build -o "$out" .
