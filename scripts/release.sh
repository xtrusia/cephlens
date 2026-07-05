#!/usr/bin/env bash
# Bump the version, commit, tag, and push to trigger the cargo-dist release.
# The default bump is patch (x.y.Z+1); minor/major/explicit are opt-in.
#
#   scripts/release.sh          # 0.1.0 -> 0.1.1  (patch, default)
#   scripts/release.sh minor    # 0.1.1 -> 0.2.0
#   scripts/release.sh major    # 0.2.0 -> 1.0.0
#   scripts/release.sh 2.3.4     # explicit version
#   scripts/release.sh patch -y  # skip the push confirmation
set -euo pipefail
cd "$(dirname "$0")/.."

bump="patch"
assume_yes=0
for arg in "$@"; do
  case "$arg" in
    -y | --yes) assume_yes=1 ;;
    patch | minor | major) bump="$arg" ;;
    [0-9]*.[0-9]*.[0-9]*) bump="$arg" ;;
    *)
      echo "usage: $0 [patch|minor|major|X.Y.Z] [-y]" >&2
      exit 1
      ;;
  esac
done

if [ -n "$(git status --porcelain)" ]; then
  echo "error: working tree is not clean; commit or stash first" >&2
  exit 1
fi

current=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "([^"]+)".*/\1/')
IFS='.' read -r major minor patch <<<"$current"
case "$bump" in
  major)
    major=$((major + 1))
    minor=0
    patch=0
    ;;
  minor)
    minor=$((minor + 1))
    patch=0
    ;;
  patch) patch=$((patch + 1)) ;;
  *) IFS='.' read -r major minor patch <<<"$bump" ;;
esac
new="$major.$minor.$patch"
tag="v$new"
echo "release: $current -> $new  ($tag)"

# rewrite the first (package) version line, portably
awk -v v="$new" '/^version = "/ && !done {sub(/"[^"]+"/, "\"" v "\""); done=1} {print}' \
  Cargo.toml >Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml

cargo check --quiet # refresh Cargo.lock and sanity-check the build before tagging

git add Cargo.toml Cargo.lock
git commit -s -m "release: $tag"
git tag -a "$tag" -m "$tag"

if [ "$assume_yes" -ne 1 ]; then
  read -r -p "push $tag and start the release build? [y/N] " ans
  case "$ans" in
    [Yy]*) ;;
    *)
      echo "not pushed. undo with: git tag -d $tag && git reset --hard HEAD~1"
      exit 0
      ;;
  esac
fi

git push origin HEAD
git push origin "$tag"
echo "released: https://github.com/xtrusia/cephlens/releases/tag/$tag"
