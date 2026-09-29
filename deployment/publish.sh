#!/usr/bin/env bash
# Uploads packaged archives (see build.sh) to R2 as a version, then points
# `latest` at it and uploads install.sh.
#
#   deployment/publish.sh <version> [dist-dir] [--force]
#   deployment/publish.sh <version> --check     # only fail if already published
#
# Config comes from the environment, or deployment/.env when present (see
# .env.example): PUBLIC_URL, R2_BUCKET, R2_PREFIX, and either RCLONE_REMOTE or
# R2_ACCOUNT_ID / R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY.
set -euo pipefail

PLATFORMS="darwin-arm64 linux-x64"

here=$(cd "$(dirname "$0")" && pwd)
die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

version='' dist=$here/dist force=0 check_only=0
for arg in "$@"; do
  case $arg in
    --force) force=1 ;;
    --check) check_only=1 ;;
    -*) die "unknown option: $arg" ;;
    *) if [ -z "$version" ]; then version=${arg#v}; else dist=$arg; fi ;;
  esac
done
[ -n "$version" ] || die "usage: $0 <version> [dist-dir] [--force | --check]"

if [ -f "$here/.env" ]; then
  set -a; . "$here/.env"; set +a
fi
: "${PUBLIC_URL:?not set}" "${R2_BUCKET:?not set}" "${R2_PREFIX:?not set}"
PUBLIC_URL=${PUBLIC_URL%/}

# Asks the public URL rather than the bucket, which needs no credentials. The
# .sha256 is probed because Cloudflare does not cache its 404s.
if [ $force = 0 ] && curl -fsI "$PUBLIC_URL/v$version/cue-linux-x64.tar.gz.sha256" >/dev/null; then
  die "v$version is already published (bump crates/cue/Cargo.toml, or --force)"
fi
[ $check_only = 1 ] && exit 0

for platform in $PLATFORMS; do
  for file in "cue-$platform.tar.gz" "cue-$platform.tar.gz.sha256"; do
    [ -f "$dist/$file" ] || die "missing $dist/$file"
  done
done

command -v rclone >/dev/null || die "rclone not found"
if [ -n "${RCLONE_REMOTE:-}" ]; then
  remote=$RCLONE_REMOTE
else
  : "${R2_ACCOUNT_ID:?not set}" "${R2_ACCESS_KEY_ID:?not set}" "${R2_SECRET_ACCESS_KEY:?not set}"
  export RCLONE_CONFIG_CUER2_TYPE=s3
  export RCLONE_CONFIG_CUER2_PROVIDER=Cloudflare
  export RCLONE_CONFIG_CUER2_ENDPOINT="https://$R2_ACCOUNT_ID.r2.cloudflarestorage.com"
  export RCLONE_CONFIG_CUER2_ACCESS_KEY_ID=$R2_ACCESS_KEY_ID
  export RCLONE_CONFIG_CUER2_SECRET_ACCESS_KEY=$R2_SECRET_ACCESS_KEY
  # Bucket-scoped tokens cannot check or create buckets.
  export RCLONE_CONFIG_CUER2_NO_CHECK_BUCKET=true
  remote=cuer2
fi
dest=$remote:$R2_BUCKET/$R2_PREFIX

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
printf '%s\n' "$version" > "$tmp/latest"
sed "s|@PUBLIC_URL@|$PUBLIC_URL|g" "$here/install.sh" > "$tmp/install.sh"

step "uploading v$version to $dest"
# Versioned files never change; `latest` and install.sh must not be cached.
immutable='Cache-Control: public, max-age=31536000, immutable'
text='Content-Type: text/plain; charset=utf-8'
for platform in $PLATFORMS; do
  archive=cue-$platform.tar.gz
  rclone copyto "$dist/$archive" "$dest/v$version/$archive" --header-upload "$immutable"
  rclone copyto "$dist/$archive.sha256" "$dest/v$version/$archive.sha256" \
    --header-upload "$immutable" --header-upload "$text"
done
# Last, so installs never see a version whose files are still uploading.
rclone copyto "$tmp/install.sh" "$dest/install.sh" --header-upload 'Cache-Control: no-cache' --header-upload "$text"
rclone copyto "$tmp/latest" "$dest/latest" --header-upload 'Cache-Control: no-cache' --header-upload "$text"

step "verifying"
served=$(curl -fsSL "$PUBLIC_URL/latest")
[ "$served" = "$version" ] || die "$PUBLIC_URL/latest serves '$served', expected '$version'"
for platform in $PLATFORMS; do
  expected=$(cut -d' ' -f1 "$dist/cue-$platform.tar.gz.sha256")
  actual=$(curl -fsSL "$PUBLIC_URL/v$version/cue-$platform.tar.gz" | shasum -a 256 | cut -d' ' -f1)
  [ "$expected" = "$actual" ] || die "cue-$platform.tar.gz at $PUBLIC_URL does not match the upload"
done

echo
echo "published cue v$version. install with:"
echo "  curl -fsSL $PUBLIC_URL/install.sh | bash"
