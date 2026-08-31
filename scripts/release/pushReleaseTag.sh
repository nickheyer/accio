#!/usr/bin/env bash
# Cut the next release: find the highest v* tag, bump it, sync the
# workspace version in Cargo.toml, commit, tag, and push. The tag push
# is what ships - .github/workflows/release.yml builds everything from it.
#
#   pushReleaseTag.sh              # patch: v1.0.8 -> v1.0.9
#   pushReleaseTag.sh --minor      # v1.0.8 -> v1.1.0
#   pushReleaseTag.sh --major      # v1.0.8 -> v2.0.0
#   pushReleaseTag.sh --set 2.5.0  # exactly v2.5.0
#   pushReleaseTag.sh --dry-run    # print the plan, touch nothing
set -euo pipefail

usage() { sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; }

BUMP=patch
SET=''
DRY=false
while [ $# -gt 0 ]; do
  case "$1" in
    --major | major) BUMP=major ;;
    --minor | minor) BUMP=minor ;;
    --patch | patch) BUMP=patch ;;
    --set)
      SET="${2:?--set needs a version}"
      shift
      ;;
    --dry-run | -n) DRY=true ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "pushReleaseTag: unknown argument '$1'" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

cd "$(git rev-parse --show-toplevel)"
git symbolic-ref -q HEAD > /dev/null || {
  echo "pushReleaseTag: detached HEAD - check out a branch first" >&2
  exit 1
}

git fetch --tags --quiet origin || echo "warning: could not fetch tags from origin; using local tags" >&2

BASE=$(git tag --list 'v[0-9]*' | sed 's/^v//' | sort -V | tail -1)
BASE=${BASE:-0.0.0}
BASE=${BASE%%-*}
[[ $BASE =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "pushReleaseTag: highest tag v$BASE is not X.Y.Z" >&2
  exit 1
}

if [ -n "$SET" ]; then
  NEW=$SET
  [[ $NEW =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
    echo "pushReleaseTag: --set '$SET' is not X.Y.Z" >&2
    exit 1
  }
else
  IFS=. read -r MA MI PA <<< "$BASE"
  case "$BUMP" in
    major) NEW="$((MA + 1)).0.0" ;;
    minor) NEW="$MA.$((MI + 1)).0" ;;
    patch) NEW="$MA.$MI.$((PA + 1))" ;;
  esac
fi
TAG="v$NEW"

git rev-parse -q --verify "refs/tags/$TAG" > /dev/null && {
  echo "pushReleaseTag: $TAG already exists locally" >&2
  exit 1
}
[ -n "$(git ls-remote --tags origin "$TAG" 2> /dev/null)" ] && {
  echo "pushReleaseTag: $TAG already exists on origin" >&2
  exit 1
}

echo "pushReleaseTag: v$BASE -> $TAG"

if $DRY; then
  echo "  would set the workspace version in Cargo.toml to $NEW and resync Cargo.lock"
  echo "  would commit as 'release: $TAG', tag $TAG, and push HEAD + tag to origin"
  ! git diff --cached --quiet && echo "  note: currently staged changes would ride the release commit"
  exit 0
fi

# Backup suffix keeps the in place edit portable across BSD and GNU sed
sed -i.bak "s/^version = \".*\"/version = \"$NEW\"/" Cargo.toml
rm -f Cargo.toml.bak
grep -q "^version = \"$NEW\"" Cargo.toml || {
  echo "pushReleaseTag: could not set the workspace version in Cargo.toml" >&2
  exit 1
}
cargo update -w -q

git add Cargo.toml Cargo.lock
if git diff --cached --quiet; then
  echo "nothing to commit (version already at $NEW); tagging HEAD as-is"
else
  git commit -q -m "release: $TAG"
  echo "committed release: $TAG"
fi

if [ -n "$(git status --porcelain)" ]; then
  echo "warning: unstaged/untracked changes left behind - they are NOT part of $TAG:" >&2
  git status --porcelain | sed 's/^/    /' >&2
fi

git tag -a "$TAG" -m "accio $NEW"
git push origin HEAD "refs/tags/$TAG"
echo "pushed $TAG - release.yml is building it"
