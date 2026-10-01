#!/bin/sh
# Prints the Homebrew formula for <version>, filled from the dv-<version>-<target>.tar.gz.sha256
# files in <dist-dir>. Fails, printing nothing, when a checksum is missing or a placeholder
# stays unfilled.
#
# usage: render-formula.sh <version> <dist-dir>
set -eu

[ $# -eq 2 ] || { echo "usage: $0 <version> <dist-dir>" >&2; exit 2; }
version=$1 dist=$2
template="$(dirname "$0")/../homebrew/dv.rb.tmpl"
targets="aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl"

# The 64-hex SHA-256 from a `shasum` line, or a failure.
checksum() {
  file="$dist/dv-$version-$1.tar.gz.sha256"
  [ -f "$file" ] || { echo "missing $file" >&2; return 1; }
  sha=$(cut -d' ' -f1 "$file")
  echo "$sha" | grep -Eqx '[0-9a-f]{64}' || { echo "bad checksum in $file" >&2; return 1; }
  echo "$sha"
}

script="s/@VERSION@/$version/g"
for target in $targets; do
  placeholder="@SHA_$(echo "$target" | tr 'a-z-' 'A-Z_')@"
  script="$script;s/$placeholder/$(checksum "$target")/"
done

formula=$(sed "$script" "$template")
if echo "$formula" | grep -Eq '@[A-Z0-9_]+@'; then
  echo "unfilled placeholders remain" >&2
  exit 1
fi
echo "$formula"
