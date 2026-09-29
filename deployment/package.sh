#!/usr/bin/env bash
# Packages a built cue binary as <out-dir>/cue-<platform>.tar.gz and its .sha256.
#
#   deployment/package.sh <platform> <binary> [out-dir]
#
# Platforms are what install.sh asks for: darwin-arm64, linux-x64.
set -euo pipefail

[ $# -ge 2 ] || { echo "usage: $0 <platform> <binary> [out-dir]" >&2; exit 2; }
platform=$1 binary=$2
out=${3:-$(cd "$(dirname "$0")" && pwd)/dist}
archive=cue-$platform.tar.gz

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp "$binary" "$stage/cue"
chmod 755 "$stage/cue"

mkdir -p "$out"
if tar --version | grep -q bsdtar; then
  # No AppleDouble files or macOS xattrs, which GNU tar warns about on extract.
  COPYFILE_DISABLE=1 tar --no-xattrs --no-mac-metadata --uid 0 --gid 0 \
    -czf "$out/$archive" -C "$stage" cue
else
  tar --owner=0 --group=0 --numeric-owner -czf "$out/$archive" -C "$stage" cue
fi
(cd "$out" && shasum -a 256 "$archive" > "$archive.sha256")
echo "$archive  $(du -h "$out/$archive" | cut -f1)"
