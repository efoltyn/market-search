#!/bin/sh
# Puts the universal market-search binary in the app's Contents/Helpers and
# signs it with the app's identity and the hardened runtime, so the app's own
# signature (made after this phase) seals it and notarization accepts it.
#
# The binary comes from the GitHub release v$MARKETING_VERSION (both macOS
# builds, checked against the release's SHA256SUMS, joined with lipo), so the
# app ships exactly what `curl | sh` installs.
#
# MS_HELPER: use this local binary instead (a test build); skips the download
# MS_VERSION: release tag to embed (default: v$MARKETING_VERSION)
set -eu

DEST="$TARGET_BUILD_DIR/$CONTENTS_FOLDER_PATH/Helpers"
mkdir -p "$DEST"
rm -f "$DEST/market-search"

if [ -n "${MS_HELPER:-}" ]; then
  cp "$MS_HELPER" "$DEST/market-search"
else
  TAG=${MS_VERSION:-"v$MARKETING_VERSION"}
  BASE="https://github.com/efoltyn/market-search/releases/download/$TAG"
  WORK="$DERIVED_FILE_DIR/market-search-$TAG"
  mkdir -p "$WORK"
  curl -fsSL "$BASE/SHA256SUMS" -o "$WORK/SHA256SUMS"
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    curl -fsSL "$BASE/market-search-$t" -o "$WORK/market-search-$t"
    want=$(grep " market-search-$t\$" "$WORK/SHA256SUMS" | awk '{print $1}')
    got=$(shasum -a 256 "$WORK/market-search-$t" | awk '{print $1}')
    [ -n "$want" ] && [ "$want" = "$got" ] || { echo "error: checksum mismatch for market-search-$t ($TAG)" >&2; exit 1; }
  done
  lipo -create "$WORK/market-search-aarch64-apple-darwin" "$WORK/market-search-x86_64-apple-darwin" \
    -output "$DEST/market-search"
fi
chmod 755 "$DEST/market-search"

IDENTITY=${EXPANDED_CODE_SIGN_IDENTITY:-}
[ -n "$IDENTITY" ] || IDENTITY=-
TIMESTAMP=--timestamp=none
if [ "$IDENTITY" != - ] && [ "${CONFIGURATION:-}" = Release ]; then TIMESTAMP=--timestamp; fi
codesign --force --sign "$IDENTITY" --options runtime $TIMESTAMP \
  --identifier com.eliterminal.market-search.cli "$DEST/market-search"
