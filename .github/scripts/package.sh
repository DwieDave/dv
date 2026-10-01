#!/bin/sh
# Packages a release binary as <out>/dv-<version>-<target>.tar.gz plus a .sha256 beside it.
# The archive holds dv, the licenses and the README at its root (the Homebrew formula
# installs "dv" from there).
#
# usage: package.sh <target> <version> <binary> <out-dir>
set -eu

[ $# -eq 4 ] || { echo "usage: $0 <target> <version> <binary> <out-dir>" >&2; exit 2; }
target=$1 version=$2 binary=$3 out=$4
root=$(cd "$(dirname "$0")/../.." && pwd)
archive="dv-$version-$target.tar.gz"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp "$binary" "$stage/dv"
chmod 755 "$stage/dv"
cp "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$root/README.md" "$stage/"

mkdir -p "$out"
tar -czf "$out/$archive" -C "$stage" dv LICENSE-MIT LICENSE-APACHE README.md
(cd "$out" && shasum -a 256 "$archive" > "$archive.sha256")
echo "$out/$archive"
