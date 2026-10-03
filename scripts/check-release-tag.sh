#!/usr/bin/env bash
# Release gate: fail unless the pushed tag is exactly "v" + the package
# version in Cargo.toml, so a tag can never publish a binary that reports
# a different version. Also reports whether the version is a SemVer
# prerelease; when $GITHUB_OUTPUT is set, it writes `version` and
# `prerelease` (true/false) there as step outputs.
#
# Usage: scripts/check-release-tag.sh <tag>
# Run from the repository root; needs cargo and jq.
set -euo pipefail

tag="${1:?usage: $0 <tag>}"

# cargo metadata reads the manifest the same way cargo does, so this is
# immune to formatting, comments, or other `version =` keys in Cargo.toml.
version=$(cargo metadata --no-deps --format-version 1 |
  jq -r '.packages[] | select(.name == "m3u-viewer") | .version' |
  tr -d '\r')

if [ -z "$version" ]; then
  echo "::error::could not read the m3u-viewer version from Cargo.toml"
  exit 1
fi

if [ "$tag" != "v$version" ]; then
  echo "::error::tag $tag does not match Cargo.toml version $version (expected v$version)"
  exit 1
fi

# SemVer prerelease: a "-" in the version before any "+build" metadata,
# e.g. 1.0.0-rc1 (but not 1.0.0+build-5).
case "${version%%+*}" in
  *-*) prerelease=true ;;
  *) prerelease=false ;;
esac

echo "tag $tag matches Cargo.toml version $version (prerelease=$prerelease)"

if [ -n "${GITHUB_OUTPUT:-}" ]; then
  {
    echo "version=$version"
    echo "prerelease=$prerelease"
  } >>"$GITHUB_OUTPUT"
fi
