#!/usr/bin/env bash
#
# Cut a release tag that matches Cargo workspace version (or set a new one).
#
#   ./scripts/tag-release.sh              # tag v$(Cargo.toml version), push
#   ./scripts/tag-release.sh 0.1.1        # bump Cargo.toml + package.json, commit, tag, push
#   ./scripts/tag-release.sh 0.1.1 --no-push
#
# GitHub Actions (.github/workflows/release.yml) builds multi-arch binaries and
# publishes the GitHub Release when the tag lands.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

NO_PUSH=0
VERSION_ARG=""
for a in "$@"; do
  case "$a" in
    --no-push) NO_PUSH=1 ;;
    -h|--help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *)
      if [ -n "$VERSION_ARG" ]; then
        echo "error: unexpected argument: $a" >&2
        exit 1
      fi
      VERSION_ARG="$a"
      ;;
  esac
done

die() { echo "error: $*" >&2; exit 1; }
note() { echo "==> $*"; }

command -v git >/dev/null || die "git is required"
git rev-parse --is-inside-work-tree >/dev/null 2>&1 || die "not a git repo"

current_version() {
  # [workspace.package] version = "x.y.z"
  sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1
}

if [ -n "$VERSION_ARG" ]; then
  VER="${VERSION_ARG#v}"
  [[ "$VER" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-].*)?$ ]] || die "version must look like 0.1.1 (got $VERSION_ARG)"
  CUR="$(current_version)"
  if [ "$CUR" != "$VER" ]; then
    note "bumping workspace version $CUR → $VER"
    # macOS and GNU sed both accept -i.bak
    sed -i.bak "s/^version = \"$CUR\"/version = \"$VER\"/" Cargo.toml
    rm -f Cargo.toml.bak
    if [ -f web/package.json ]; then
      sed -i.bak "s/\"version\": \"$CUR\"/\"version\": \"$VER\"/" web/package.json
      rm -f web/package.json.bak
    fi
    git add Cargo.toml web/package.json 2>/dev/null || git add Cargo.toml
    if ! git diff --cached --quiet; then
      git commit -m "Release v${VER}"
    fi
  else
    note "Cargo.toml already at $VER"
  fi
else
  VER="$(current_version)"
  [ -n "$VER" ] || die "could not read version from Cargo.toml"
  note "using Cargo.toml version $VER"
fi

TAG="v${VER}"

if git rev-parse "$TAG" >/dev/null 2>&1; then
  die "tag $TAG already exists (delete it first if you meant to re-cut)"
fi

if [ -n "$(git status --porcelain)" ]; then
  die "working tree is dirty; commit or stash before tagging"
fi

note "creating annotated tag $TAG"
git tag -a "$TAG" -m "Release $TAG"

if [ "$NO_PUSH" -eq 1 ]; then
  note "tag created locally only (--no-push). Publish with:"
  echo "  git push origin HEAD $TAG"
  exit 0
fi

note "pushing branch and tag (triggers Release workflow)"
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
git push origin "$BRANCH"
git push origin "$TAG"
note "done — watch: https://github.com/$(git remote get-url origin | sed -E 's#.*github.com[:/](.+)(\.git)?#\1#' | sed 's/\.git$//')/actions"
