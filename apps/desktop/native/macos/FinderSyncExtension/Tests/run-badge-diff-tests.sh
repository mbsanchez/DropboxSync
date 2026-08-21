#!/bin/bash
# Compiles and runs the BadgeDiff checks (DBSYNC-84).
#
# There is no Swift test target here and adding one is out of scope (DBSYNC-72), so this
# builds the real BadgeDiff.swift together with its checks and runs the result. Exits
# non-zero if any check fails, so it is usable as a gate.
#
#   ./Tests/run-badge-diff-tests.sh
#
# Note `swiftc`, not `swift`: `swift a.swift b.swift` compiles the second file without
# putting it in scope for the first, and reports success while running nothing at all.
set -euo pipefail

cd "$(dirname "$0")/.."

OUT=$(mktemp -d)/badge-diff-tests
trap 'rm -rf "$(dirname "$OUT")"' EXIT

xcrun swiftc -o "$OUT" BadgeDiff.swift Tests/BadgeDiffTests.swift
"$OUT"
