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
#
# Supplying a team is NOT enough on its own. The project ships CODE_SIGN_STYLE=Automatic, and automatic
# signing resolves the *development* certificate for a build action: with a team set and no "Mac Development"
# certificate in the keychain, xcodebuild fails with
#   error: No signing certificate "Mac Development" found
# A Developer ID-only account (the distribution model for this app) never has that certificate. So whenever a
# team is present we switch to manual signing and name the identity explicitly.
SIGN_ARGS=()
TEAM="${APPLE_TEAM_ID:-${DEVELOPMENT_TEAM:-}}"
APPEX_IDENTITY="${APPEX_CODE_SIGN_IDENTITY:-Developer ID Application}"
if [[ -n "$TEAM" ]]; then
  SIGN_ARGS+=(
    DEVELOPMENT_TEAM="$TEAM"
    CODE_SIGN_STYLE=Manual
    CODE_SIGN_IDENTITY="$APPEX_IDENTITY"
  )
  echo "Signing appex as '${APPEX_IDENTITY}' for team ${TEAM}."
  # Xcode signs build products with --timestamp=none. A secure timestamp is mandatory for notarization, so
  # the appex must be re-signed with --timestamp before submission (see DBSYNC-72 Slice 4 / finding F4).
  echo "NOTE: no secure timestamp is applied here; re-sign with --timestamp before notarizing."
else
  echo "No APPLE_TEAM_ID/DEVELOPMENT_TEAM set: ad-hoc signing (fine for development and CI, not notarizable)."
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
  "${SIGN_ARGS[@]+"${SIGN_ARGS[@]}"}"

OUT="${DERIVED}/Build/Products/${CONFIG}/${SCHEME}.appex"
echo "Built: $OUT"
if [[ -d "$OUT" ]]; then
  mkdir -p "${ROOT}/build"
  rm -rf "${ROOT}/build/${SCHEME}.appex"
  /usr/bin/ditto "$OUT" "${ROOT}/build/${SCHEME}.appex"
  echo "Copied to: ${ROOT}/build/${SCHEME}.appex"
fi
