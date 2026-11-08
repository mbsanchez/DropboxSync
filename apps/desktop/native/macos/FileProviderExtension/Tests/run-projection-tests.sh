#!/bin/bash
# Compiles and runs the projection checks (DBSYNC-95 slice 2b): the real ProjectionModel.swift and
# ProjectionStore.swift with their checks, against a temporary WAL database built from projection-schema.sql.
# Exits non-zero if any check fails, so it is usable as a gate. Same shape as
# FinderSyncExtension/Tests/run-badge-diff-tests.sh; `swiftc`, not `swift`, for the reason given there.
set -euo pipefail

cd "$(dirname "$0")/.."

OUT=$(mktemp -d)/projection-tests
trap 'rm -rf "$(dirname "$OUT")"' EXIT

xcrun swiftc -suppress-warnings -o "$OUT" ProjectionModel.swift ProjectionStore.swift Exchange.swift \
  Tests/ProjectionModelTests.swift Tests/ProjectionStoreTests.swift Tests/ExchangeTests.swift
"$OUT" Tests/projection-schema.sql
