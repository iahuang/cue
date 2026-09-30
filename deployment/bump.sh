#!/usr/bin/env bash
# Bumps cue's version, commits Cargo.toml + Cargo.lock, tags v<version>, and
# pushes main and the tag to origin (which triggers the release workflow).
#
#   deployment/bump.sh major|minor|patch
#
# Requires a clean worktree on main, level with origin/main. Asks before
# changing anything.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
manifest=crates/cue/Cargo.toml
cd "$root"

die() { echo "error: $*" >&2; exit 1; }

case ${1:-} in
  major|minor|patch) part=$1 ;;
  *) echo "usage: $0 major|minor|patch" >&2; exit 2 ;;
esac

branch=$(git symbolic-ref --short HEAD 2>/dev/null || true)
[[ $branch == main ]] || die "not on main (on ${branch:-detached HEAD})"
[[ -z $(git status --porcelain --untracked-files=no) ]] || die "worktree has uncommitted changes"

git fetch --quiet --tags origin main
read -r behind ahead < <(git rev-list --left-right --count origin/main...HEAD)
((behind == 0)) || die "main is $behind commit(s) behind origin/main; pull first"

old=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$manifest" | head -n1)
[[ $old =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] || die "can't parse version '$old' in $manifest"
major=${BASH_REMATCH[1]} minor=${BASH_REMATCH[2]} patch=${BASH_REMATCH[3]}
case $part in
  major) new=$((major + 1)).0.0 ;;
  minor) new=$major.$((minor + 1)).0 ;;
  patch) new=$major.$minor.$((patch + 1)) ;;
esac
tag=v$new

git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists"

echo "cue $old -> $new"
((ahead == 0)) || { echo "also pushing $ahead unpushed commit(s):"; git log --oneline origin/main..HEAD; }
read -r -p "Commit, tag $tag, and push to origin (starts a release)? [y/N] " answer
[[ $answer == [yY] ]] || { echo "aborted"; exit 1; }

# Undo the edits if anything fails before the commit lands.
trap 'git checkout -- "$manifest" Cargo.lock' EXIT

tmp=$(mktemp)
awk -v new="$new" '!done && /^version = "/ { $0 = "version = \"" new "\""; done = 1 } 1' "$manifest" >"$tmp"
cat "$tmp" >"$manifest" && rm "$tmp"

cargo update --workspace --quiet
cargo metadata --locked --format-version 1 >/dev/null  # fails if the lockfile is still stale
git diff --quiet Cargo.lock && die "Cargo.lock didn't change; expected cue's version in it to update"

git commit --quiet -m "$tag" -- "$manifest" Cargo.lock
trap - EXIT

git tag -a "$tag" -m "$tag"
git push --atomic origin main "$tag"
echo "pushed $tag"
