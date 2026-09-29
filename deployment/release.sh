#!/usr/bin/env bash
# Releases from this Mac: builds aarch64 macOS natively and x86_64 Linux in
# Docker (static musl), then publishes with publish.sh. Releases normally go
# through .github/workflows/release.yml (push a v* tag); this is the fallback,
# and with --dry-run a way to test builds locally.
#
#   deployment/release.sh [--dry-run] [--force] [--allow-dirty]
#
#   --dry-run      build and package into deployment/dist, but upload nothing
#   --force        overwrite a version that is already published
#   --allow-dirty  release with uncommitted changes
#
# The version is the `cue` crate's version in crates/cue/Cargo.toml.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
dist=$here/dist

dry_run=0 force=0 allow_dirty=0
for arg in "$@"; do
  case $arg in
    --dry-run) dry_run=1 ;;
    --force) force=1 ;;
    --allow-dirty) allow_dirty=1 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

[ "$(uname -s)-$(uname -m)" = Darwin-arm64 ] || die "run this on an Apple Silicon Mac"
command -v docker >/dev/null || die "docker not found"
docker info >/dev/null 2>&1 || die "docker daemon is not running"

if [ $allow_dirty = 0 ] && [ -n "$(git -C "$root" status --porcelain)" ]; then
  die "working tree has uncommitted changes (--allow-dirty to release anyway)"
fi

pkgid=$(cd "$root" && cargo pkgid -p cue)
version=${pkgid##*[#@]}
publish_flags=()
[ $force = 1 ] && publish_flags+=(--force)
# Fail before a long build rather than after it.
[ $dry_run = 1 ] || "$here/publish.sh" "$version" --check ${publish_flags[@]+"${publish_flags[@]}"}

echo "releasing cue v$version ($(git -C "$root" rev-parse --short HEAD))"
rm -rf "$dist"

step "building aarch64-apple-darwin"
(cd "$root" && cargo build --locked --profile dist -p cue --target aarch64-apple-darwin)
mac_bin=$root/target/aarch64-apple-darwin/dist/cue
# Apple Silicon refuses to run unsigned code. The linker signs ad hoc, but
# stripping can invalidate that, so sign again.
codesign --force --sign - "$mac_bin"
codesign --verify "$mac_bin"
"$mac_bin" --help >/dev/null

step "building x86_64-unknown-linux-musl (docker)"
# Built from stdin so .env never enters a build context.
docker build -t cue-linux-builder - < "$here/Dockerfile.linux"
linux_bin=target/docker/x86_64-unknown-linux-musl/dist/cue
docker run --rm \
  -v "$root":/src \
  -v cue-cargo-registry:/usr/local/cargo/registry \
  -v cue-zig-cache:/root/.cache/zig \
  -e CARGO_TARGET_DIR=/src/target/docker \
  cue-linux-builder sh -euc "
    cargo zigbuild --locked --profile dist -p cue --target x86_64-unknown-linux-musl
    file $linux_bin | grep -q 'x86-64.*static' || { file $linux_bin; echo 'not a static x86-64 binary' >&2; exit 1; }
  "
# Smoke test on x86_64. Rosetta runs finished binaries fine; it is building
# under it that breaks (see Dockerfile.linux).
docker run --rm --platform linux/amd64 -v "$root/$linux_bin":/cue:ro alpine /cue --help >/dev/null

step "packaging"
"$here/package.sh" darwin-arm64 "$mac_bin" "$dist"
"$here/package.sh" linux-x64 "$root/$linux_bin" "$dist"

if [ $dry_run = 1 ]; then
  step "dry run: artifacts are in deployment/dist"
  exit 0
fi

"$here/publish.sh" "$version" "$dist" ${publish_flags[@]+"${publish_flags[@]}"}
