#!/usr/bin/env bash
# Build DropboxSyncFinderSync.appex (Finder Sync extension). Requires full Xcode (not only CLT).
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "Skipping Finder appex build (macOS only)."
  exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

PROJECT="DropboxSyncFinderSync.xcodeproj"
SCHEME="DropboxSyncFinderSync"
CONFIG="${CONFIGURATION:-Release}"
DERIVED="${ROOT}/build/DerivedData"

if ! command -v xcodebuild >/dev/null 2>&1; then
  echo "xcodebuild not found. Install Xcode and run: sudo xcode-select -s /Applications/Xcode.app/Contents/Developer" >&2
  exit 1
fi

mkdir -p "$DERIVED"

echo "Building ${SCHEME}.appex (${CONFIG})…"

# Same team as the main app: set APPLE_TEAM_ID (also used by Tauri notarization) or DEVELOPMENT_TEAM.
TEAM_ARGS=()
if [[ -n "${APPLE_TEAM_ID:-}" ]]; then
  TEAM_ARGS+=(DEVELOPMENT_TEAM="$APPLE_TEAM_ID")
  echo "Using DEVELOPMENT_TEAM from APPLE_TEAM_ID."
elif [[ -n "${DEVELOPMENT_TEAM:-}" ]]; then
  TEAM_ARGS+=(DEVELOPMENT_TEAM="$DEVELOPMENT_TEAM")
  echo "Using DEVELOPMENT_TEAM from environment."
fi

# -scheme is required with -derivedDataPath on recent Xcode; keeps all output under native/.../build/.
# With `set -u`, a plain "${TEAM_ARGS[@]}" can fail when the array is empty (macOS Bash 3.2).
xcodebuild \
  -project "$PROJECT" \
  -scheme "$SCHEME" \
  -configuration "$CONFIG" \
  -derivedDataPath "$DERIVED" \
  -destination "generic/platform=macOS" \
  build \
  CODE_SIGNING_ALLOWED="${CODE_SIGNING_ALLOWED:-YES}" \
  "${TEAM_ARGS[@]+"${TEAM_ARGS[@]}"}"

OUT="${DERIVED}/Build/Products/${CONFIG}/${SCHEME}.appex"
echo "Built: $OUT"
if [[ -d "$OUT" ]]; then
  mkdir -p "${ROOT}/build"
  rm -rf "${ROOT}/build/${SCHEME}.appex"
  /usr/bin/ditto "$OUT" "${ROOT}/build/${SCHEME}.appex"
  echo "Copied to: ${ROOT}/build/${SCHEME}.appex"
fi
