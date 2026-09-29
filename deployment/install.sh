#!/usr/bin/env bash
# Installs cue: curl -fsSL @PUBLIC_URL@/install.sh | bash
#
# Environment:
#   CUE_VERSION   version to install (default: latest)
#   CUE_INSTALL   install root; the binary goes in $CUE_INSTALL/bin (default: ~/.cue)
#   CUE_BASE_URL  download root (default: where this script was published)
set -euo pipefail

base_url=${CUE_BASE_URL:-@PUBLIC_URL@}
case $base_url in
  @*) echo "error: set CUE_BASE_URL (this copy of install.sh was not published)" >&2; exit 1 ;;
esac

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) platform=darwin-arm64 ;;
  Linux-x86_64 | Linux-amd64) platform=linux-x64 ;;
  *) echo "error: no cue build for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

version=${CUE_VERSION:-$(curl -fsSL "$base_url/latest")}
version=${version#v}
bin_dir=${CUE_INSTALL:-$HOME/.cue}/bin
archive=cue-$platform.tar.gz
url=$base_url/v$version/$archive

if [ -x "$bin_dir/cue" ] && [ "$("$bin_dir/cue" --version 2>/dev/null)" = "cue $version" ]; then
  echo "cue $version is already installed at $bin_dir/cue"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "downloading cue $version ($platform)"
curl -fSL --progress-bar "$url" -o "$tmp/$archive"
expected=$(curl -fsSL "$url.sha256" | cut -d' ' -f1)
if command -v sha256sum >/dev/null; then
  actual=$(sha256sum "$tmp/$archive" | cut -d' ' -f1)
else
  actual=$(shasum -a 256 "$tmp/$archive" | cut -d' ' -f1)
fi
if [ "$expected" != "$actual" ]; then
  echo "error: checksum mismatch for $archive" >&2
  exit 1
fi

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$bin_dir"
# Replace rather than overwrite, so a running cue keeps its (unlinked) binary.
mv -f "$tmp/cue" "$bin_dir/cue"
chmod +x "$bin_dir/cue"
echo "installed cue $version to $bin_dir/cue"

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *)
    case ${SHELL##*/} in
      zsh) rc='~/.zshrc' ;;
      bash) rc='~/.bashrc' ;;
      fish) rc='~/.config/fish/config.fish' ;;
      *) rc='your shell profile' ;;
    esac
    echo
    echo "$bin_dir is not on your PATH. Add this to $rc:"
    if [ "${SHELL##*/}" = fish ]; then
      echo "  fish_add_path $bin_dir"
    else
      echo "  export PATH=\"$bin_dir:\$PATH\""
    fi
    ;;
esac
