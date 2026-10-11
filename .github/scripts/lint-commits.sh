#!/usr/bin/env bash
# Fails if any non-merge commit in the given range has a subject that isn't a Conventional Commit.
set -euo pipefail

re='^(feat|fix|perf|refactor|docs|test|build|ci|chore|style|revert)(\([^)]+\))?!?: .+'
bad=0
while IFS= read -r line; do
  sha=${line%% *}; subject=${line#* }
  if ! grep -Eq "$re" <<< "$subject"; then
    echo "::error::${sha:0:7} is not a Conventional Commit: $subject"
    bad=1
  fi
done < <(git log --no-merges --format='%H %s' "$1")
[ "$bad" = 0 ] && echo "All commits in $1 follow Conventional Commits."
exit "$bad"
