#!/usr/bin/env bash
#
# Bump the package version, commit, tag, and push a release.
#
# Pushing a `v*` tag is what triggers .github/workflows/release.yml (build
# sdist+wheel, attach to the GitHub release, publish to PyPI), so this script
# only lands the version bump and the tag — atomically, so a failed push can
# never leave the branch ahead of a missing tag.
#
# Usage:
#   scripts/release.sh <major|minor|patch>   # bump a component of the current version
#   scripts/release.sh <X.Y.Z>               # set an explicit version
#   scripts/release.sh patch --dry-run       # print the plan, change nothing
#   scripts/release.sh minor --no-verify     # skip the `uv build` gate
#
# Idempotent recovery: if pyproject.toml is already at the requested version
# (e.g. a previous run committed the bump but failed before tagging), it skips
# the bump and just creates+pushes the missing tag on HEAD.
#
set -euo pipefail

usage() {
  echo "usage: $(basename "$0") <major|minor|patch|X.Y.Z> [--dry-run] [--no-verify]" >&2
  exit 2
}
die() { echo "error: $*" >&2; exit 1; }

[ $# -ge 1 ] || usage
bump="$1"; shift

dry_run=false
verify=true
for arg in "$@"; do
  case "$arg" in
    --dry-run)   dry_run=true ;;
    --no-verify) verify=false ;;
    *) usage ;;
  esac
done

cd "$(git rev-parse --show-toplevel)" || die "not inside a git repository"
branch="$(git symbolic-ref --quiet --short HEAD)" \
  || die "detached HEAD; checkout a branch before releasing"

# Current version = first line-start `version = "X.Y.Z"` (the [project] table).
current="$(awk -F'"' '/^version = / { print $2; exit }' pyproject.toml)"
[[ "$current" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "cannot parse current version '$current'"
IFS=. read -r maj min pat <<<"$current"

case "$bump" in
  major) new="$((maj + 1)).0.0" ;;
  minor) new="${maj}.$((min + 1)).0" ;;
  patch) new="${maj}.${min}.$((pat + 1))" ;;
  *)
    [[ "$bump" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage
    new="$bump" ;;
esac

tag="v${new}"
need_bump=true
[ "$new" = "$current" ] && need_bump=false

echo "current : ${current}"
echo "release : ${new}  (tag ${tag}, branch ${branch})"
$need_bump || echo "note    : already at ${new}; tagging HEAD without a new commit"

# If the tag exists, only proceed when it already points at HEAD (recovery: just
# (re)push it). A tag on some other commit, or alongside a pending bump, is a
# conflict the human must resolve.
head="$(git rev-parse HEAD)"
tag_exists=false
if existing="$(git rev-parse -q --verify "refs/tags/${tag}^{commit}" 2>/dev/null)"; then
  [ "$existing" = "$head" ] || die "tag ${tag} already exists on ${existing:0:9} (not HEAD)"
  $need_bump && die "tag ${tag} exists but pyproject.toml is ${current}; resolve manually"
  tag_exists=true
fi

if $dry_run; then
  echo "[dry-run] would:"
  $need_bump && echo "  - bump pyproject.toml + lgit/__init__.py -> ${new}, sync uv.lock, commit 'release: bumped version to ${new}'"
  $tag_exists && echo "  - reuse existing tag ${tag}" || echo "  - create annotated tag ${tag}"
  echo "  - atomically push ${branch} + ${tag} to origin"
  exit 0
fi

if $need_bump; then
  # The bump commit must capture a clean, intentional tree.
  git diff --quiet && git diff --cached --quiet \
    || die "working tree is dirty; commit or stash before releasing"

  # Fail before tagging if the build is broken (a pushed tag publishes at once).
  if $verify; then
    echo "verifying build (uv build)..."
    uv build --quiet || die "uv build failed; not releasing"
  fi

  # Rewrite only the first line-start `version = "..."` (the [project] version).
  perl -i -pe 'if (!$d && s/^version = "[^"]*"/version = "'"$new"'"/) { $d = 1 }' pyproject.toml
  [ "$(awk -F'"' '/^version = / { print $2; exit }' pyproject.toml)" = "$new" ] \
    || die "failed to update pyproject.toml"
  # Keep the importlib-metadata fallback in lgit/__init__.py in step.
  perl -i -pe 's/^(\s*__version__ = ")[^"]*(")/${1}'"$new"'${2}/' lgit/__init__.py
  grep -q "__version__ = \"${new}\"" lgit/__init__.py \
    || die "failed to update lgit/__init__.py fallback"
  uv lock --quiet   # keep uv.lock's own entry in step

  git add pyproject.toml uv.lock lgit/__init__.py
  git commit -m "release: bumped version to ${new}"
  head="$(git rev-parse HEAD)"
fi

$tag_exists || git tag -a "${tag}" -m "Release ${tag}"

# Never push a branch without its tag: assert the tag resolves to HEAD, then push
# both refs atomically (all-or-nothing).
[ "$(git rev-parse -q --verify "refs/tags/${tag}^{commit}")" = "$head" ] \
  || die "tag ${tag} does not point at HEAD; aborting before push"
git push --atomic origin "HEAD:refs/heads/${branch}" "refs/tags/${tag}"

echo
echo "pushed ${tag} on ${branch} → Release workflow will build artifacts and publish to PyPI:"
echo "  https://github.com/can1357/llm-git/actions/workflows/release.yml"
