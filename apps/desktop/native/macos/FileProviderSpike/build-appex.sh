#!/usr/bin/env bash
# Build FileProviderSpike.appex — DBSYNC-79 SPIKE, DELETE WITH THIS DIRECTORY.
#
# Adapted from ../FinderSyncExtension/build-appex.sh. The signing settings there are not
# boilerplate: each one was learned from a failure, and they are reproduced (not reinvented)
# because Tauri does NOT re-sign what it copies from bundle.macOS.files — whatever this script
# produces reaches Apple unchanged.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "Skipping File Provider spike appex build (macOS only)."
  exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

PROJECT="FileProviderSpike.xcodeproj"
SCHEME="FileProviderSpike"
CONFIG="${CONFIGURATION:-Release}"
DERIVED="${ROOT}/build/DerivedData"

if ! command -v xcodebuild >/dev/null 2>&1; then
  echo "xcodebuild not found. Install Xcode and run: sudo xcode-select -s /Applications/Xcode.app/Contents/Developer" >&2
  exit 1
fi

mkdir -p "$DERIVED"
echo "Building ${SCHEME}.appex (${CONFIG})…"

# See the Finder Sync script for why each of these exists:
#  - INJECT_BASE_ENTITLEMENTS=NO keeps get-task-allow out, which notarization refuses.
#  - Manual signing, because automatic resolves a "Mac Development" certificate that a
#    Developer ID-only account never has.
#  - --timestamp, because Xcode signs with --timestamp=none and notarization requires a
#    secure timestamp on every nested bundle.
SIGN_ARGS=(CODE_SIGN_INJECT_BASE_ENTITLEMENTS=NO)

ENTITLEMENTS="${ROOT}/FileProviderSpike.entitlements"
if [[ -f "$ENTITLEMENTS" ]]; then
  SIGN_ARGS+=(CODE_SIGN_ENTITLEMENTS="$ENTITLEMENTS")
fi

TEAM="${APPLE_TEAM_ID:-${DEVELOPMENT_TEAM:-}}"
APPEX_IDENTITY="${APPEX_CODE_SIGN_IDENTITY:-Developer ID Application}"
if [[ -n "$TEAM" ]]; then
  SIGN_ARGS+=(
    DEVELOPMENT_TEAM="$TEAM"
    CODE_SIGN_STYLE=Manual
    CODE_SIGN_IDENTITY="$APPEX_IDENTITY"
    OTHER_CODE_SIGN_FLAGS="--timestamp"
  )
  echo "Signing appex as '${APPEX_IDENTITY}' for team ${TEAM}."
else
  echo "No APPLE_TEAM_ID/DEVELOPMENT_TEAM set: ad-hoc signing (fine for development, not notarizable)."
fi

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

# Same guard as the Finder Sync appex: @objc(FileProviderSpike) replaces the Swift-mangled
# runtime name, so the plist's NSExtensionPrincipalClass must match the exported ObjC symbol
# exactly. Nothing else catches a drift — NSClassFromString would just return nil at load.
if [[ -d "$OUT" ]]; then
  DECLARED=$(/usr/libexec/PlistBuddy -c "Print :NSExtension:NSExtensionPrincipalClass" "$OUT/Contents/Info.plist" 2>/dev/null || true)
  EMITTED=$(nm -a "$OUT/Contents/MacOS/${SCHEME}" 2>/dev/null | sed -n 's/.*_OBJC_CLASS_\$_//p' | head -1)
  if [[ -z "$EMITTED" || "$DECLARED" != "$EMITTED" ]]; then
    echo "ERROR: NSExtensionPrincipalClass '${DECLARED}' does not match the exported ObjC class ('${EMITTED:-none}')." >&2
    exit 1
  fi
  echo "Principal class OK: ${DECLARED}"
fi

# THE STEP WHOSE ABSENCE FAILED QA. tauri.conf.json reads build/<SCHEME>.appex; without this
# copy the bundler is pointed at a path nothing ever creates, and the config still parses
# perfectly — which is exactly why "the config parses" was not evidence that this worked.
if [[ -d "$OUT" ]]; then
  mkdir -p "${ROOT}/build"
  rm -rf "${ROOT}/build/${SCHEME}.appex"
  /usr/bin/ditto "$OUT" "${ROOT}/build/${SCHEME}.appex"
  echo "Copied to: ${ROOT}/build/${SCHEME}.appex"
fi
