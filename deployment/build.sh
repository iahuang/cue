#!/usr/bin/env bash
# Builds a release binary of cue for this machine, exactly as CI does, and
# packages it as <out-dir>/cue-<platform>.tar.gz plus its .sha256.
#
#   deployment/build.sh [out-dir]    (default: deployment/dist)
#
# Hosts are the release platforms: macOS arm64, and Linux x86_64. Linux builds
# are static (musl) and need cargo-zigbuild: Zig's bundled musl is new enough
# for Zig's std (statx, musl 1.2.5), distro musl-tools is not. Both need the
# Rust target installed (`rustup target add <target>`) and Zig 0.16.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
out=${1:-$here/dist}
mkdir -p "$out"
out=$(cd "$out" && pwd)

die() { echo "error: $*" >&2; exit 1; }

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) platform=darwin-arm64 target=aarch64-apple-darwin build=build ;;
  Linux-x86_64) platform=linux-x64 target=x86_64-unknown-linux-musl build=zigbuild ;;
  *) die "no release build for $(uname -s) $(uname -m)" ;;
esac

cd "$root"
pkgid=$(cargo pkgid -p cue)
version=${pkgid##*[#@]}
cargo "$build" --locked --profile dist -p cue --target "$target"
bin=target/$target/dist/cue

case $platform in
  darwin-*)
    # Apple Silicon refuses to run unsigned code. The linker signs ad hoc, but
    # stripping can invalidate that, so sign again.
    codesign --force --sign - "$bin"
    codesign --verify "$bin"
    ;;
  linux-*)
    file "$bin" | grep -q static || { file "$bin"; die "not statically linked"; }
    ;;
esac
reported=$("$bin" --version)
[ "$reported" = "cue $version" ] || die "binary reports '$reported', expected 'cue $version'"

archive=cue-$platform.tar.gz
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp "$bin" "$stage/cue"
if [ "$platform" = darwin-arm64 ]; then
  # No AppleDouble files or macOS xattrs, which GNU tar warns about on extract.
  COPYFILE_DISABLE=1 tar --no-xattrs --no-mac-metadata --uid 0 --gid 0 \
    -czf "$out/$archive" -C "$stage" cue
else
  tar --owner=0 --group=0 --numeric-owner -czf "$out/$archive" -C "$stage" cue
fi
(cd "$out" && shasum -a 256 "$archive" > "$archive.sha256")
echo "cue $version ($platform): $out/$archive  $(du -h "$out/$archive" | cut -f1)"
