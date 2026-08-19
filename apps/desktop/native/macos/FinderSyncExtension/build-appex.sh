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
# This signature is FINAL: Tauri does not re-sign it. `copy_custom_files_to_bundle()` (tauri-bundler 2.8.1,
# bundle/macos/app.rs) places the appex from bundle.macOS.files into Contents/PlugIns/ without adding it to
# the bundler's sign_paths, and the codesign invocation itself — which lives in tauri-macos-sign 2.3.3, not
# in the bundler — carries no --deep. Whatever is wrong here reaches Apple unchanged.
#
# CODE_SIGN_INJECT_BASE_ENTITLEMENTS is set unconditionally, NOT only when signing for real: `xcodebuild
# build` otherwise injects com.apple.security.get-task-allow, a debugger-attach entitlement that notarization
# refuses and that defeats part of Hardened Runtime. Keeping it out of the ad-hoc path too means the appex
# never carries it, whichever way it was built — otherwise exporting APPLE_SIGNING_IDENTITY without
# APPLE_TEAM_ID would produce a Developer ID host wrapping an ad-hoc appex that still has it, and the
# bundler's notarize_auth() accepts an ASC API key without APPLE_TEAM_ID, so that could be submitted.
SIGN_ARGS=(CODE_SIGN_INJECT_BASE_ENTITLEMENTS=NO)
TEAM="${APPLE_TEAM_ID:-${DEVELOPMENT_TEAM:-}}"
APPEX_IDENTITY="${APPEX_CODE_SIGN_IDENTITY:-Developer ID Application}"
if [[ -n "$TEAM" ]]; then
  SIGN_ARGS+=(
    DEVELOPMENT_TEAM="$TEAM"
    CODE_SIGN_STYLE=Manual
    CODE_SIGN_IDENTITY="$APPEX_IDENTITY"
    # Xcode signs build products with --timestamp=none; notarization requires a secure TSA timestamp on every
    # nested bundle. Only meaningful with a real identity — an ad-hoc signature cannot be timestamped.
    OTHER_CODE_SIGN_FLAGS="--timestamp"
  )
  echo "Signing appex as '${APPEX_IDENTITY}' for team ${TEAM} (hardened runtime, secure timestamp, no injected entitlements)."
else
  if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
    echo "WARNING: APPLE_SIGNING_IDENTITY is set but APPLE_TEAM_ID is not." >&2
    echo "         The host app will be signed for real while this appex stays ad-hoc, which cannot be" >&2
    echo "         notarized. Export APPLE_TEAM_ID as well." >&2
  fi
  echo "No APPLE_TEAM_ID/DEVELOPMENT_TEAM set: ad-hoc signing (fine for development and CI, not notarizable)."
fi

# -scheme is required with -derivedDataPath on recent Xcode; keeps all output under native/.../build/.
# With `set -u`, a plain "${SIGN_ARGS[@]}" can fail when the array is empty (macOS Bash 3.2).
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
