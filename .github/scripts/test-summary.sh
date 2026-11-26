#!/usr/bin/env bash
# Usage: test-summary.sh <title> <cargo test log>
# Writes the test counts and the failed tests to the job summary.
title="$1"; log="$2"
count() { # sum one field of the "test result:" lines
  grep -h "^test result:" "$log" 2>/dev/null | sed -E "s/.* ([0-9]+) $1.*/\1/;t;d" | awk '{s+=$1} END {print s+0}'
}
{
  echo "## Tests: $title"
  echo
  echo "| Passed | Failed | Ignored |"
  echo "|---|---|---|"
  echo "| $(count passed) | $(count failed) | $(count ignored) |"
  echo
  echo "Failed tests:"
  echo '```'
  grep -h -E "^test .* FAILED$|panicked at" "$log" 2>/dev/null | head -n 40 || true
  echo '```'
} >> "$GITHUB_STEP_SUMMARY"
