#!/usr/bin/env bash
# Prints the next release version from the Conventional Commits since the latest tag, or nothing if no release is due.
# feat → minor, fix/perf → patch, breaking (`type!:` or a `BREAKING CHANGE:` footer) → major (minor while on 0.x).
# A higher version set by hand in Cargo.toml wins.
set -euo pipefail

cargo=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
latest=$(git tag -l 'v[0-9]*.[0-9]*.[0-9]*' --sort=-v:refname | head -1 | sed 's/^v//')
if [ -z "$latest" ]; then echo "$cargo"; exit 0; fi

log=$(git log "v$latest..HEAD" --format='%s%n%b')
type='[a-z]+(\([^)]*\))?'
if grep -Eq "^$type!:|^BREAKING[ -]CHANGE:" <<< "$log"; then bump=major
elif grep -Eq '^feat(\([^)]*\))?:' <<< "$log"; then bump=minor
elif grep -Eq '^(fix|perf)(\([^)]*\))?:' <<< "$log"; then bump=patch
else bump=
fi

IFS=. read -r ma mi pa <<< "$latest"
case "$bump" in
  major) if [ "$ma" = 0 ]; then next="0.$((mi + 1)).0"; else next="$((ma + 1)).0.0"; fi ;;
  minor) next="$ma.$((mi + 1)).0" ;;
  patch) next="$ma.$mi.$((pa + 1))" ;;
  *) next= ;;
esac

if [ "$(printf '%s\n%s\n' "$cargo" "$latest" | sort -V | tail -1)" != "$latest" ] && \
   [ "$(printf '%s\n%s\n' "$cargo" "${next:-0}" | sort -V | tail -1)" = "$cargo" ]; then
  next=$cargo
fi
echo "$next"
