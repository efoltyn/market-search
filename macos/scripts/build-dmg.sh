#!/bin/bash
# Archives Market Search.app, has Apple notarize it (Developer ID, signed through Xcode's
# cloud-managed certificate for team 9UVCD3JA4V), and packs MarketSearch.dmg: the app and an
# Applications shortcut. The app can also run straight from the DMG.
#
#   scripts/build-dmg.sh             # version from project.yml, binary from that GitHub release
#   scripts/build-dmg.sh --upload    # also attach MarketSearch.dmg to the release
#   MS_HELPER=<binary> scripts/build-dmg.sh --dev    # unsigned Debug build for a local try
#
# Xcode must be signed in to the team, or set ASC_KEY_PATH, ASC_KEY_ID and ASC_ISSUER_ID
# (an App Store Connect API key) so -allowProvisioningUpdates can sign without the account.
set -euo pipefail
cd "$(dirname "$0")/.."
MODE="${1:-}"
VER=$(sed -n 's/^ *MARKETING_VERSION: "\(.*\)"/\1/p' project.yml)
TAG="v$VER"
B=build
xcodegen generate --quiet
AUTH=()
if [ -n "${ASC_KEY_PATH:-}" ]; then
  AUTH=(-authenticationKeyPath "$ASC_KEY_PATH" -authenticationKeyID "$ASC_KEY_ID" -authenticationKeyIssuerID "$ASC_ISSUER_ID")
fi

if [ "$MODE" = --dev ]; then
  xcodebuild -project MarketSearch.xcodeproj -scheme MarketSearch -configuration Debug \
    -derivedDataPath "$B/dev" CODE_SIGN_IDENTITY=- CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM= build -quiet
  echo "$PWD/$B/dev/Build/Products/Debug/Market Search.app"
  exit 0
fi

rm -rf "$B/MarketSearch.xcarchive" "$B/export" "$B/notarized" "$B/dmg" "$B/MarketSearch.dmg"
# The generic destination builds every arch in ARCHS; without it xcodebuild archives for this Mac's only.
xcodebuild -project MarketSearch.xcodeproj -scheme MarketSearch -configuration Release \
  -destination 'generic/platform=macOS' \
  -archivePath "$B/MarketSearch.xcarchive" -allowProvisioningUpdates ${AUTH[@]+"${AUTH[@]}"} archive -quiet
APP_BIN="$B/MarketSearch.xcarchive/Products/Applications/Market Search.app/Contents/MacOS/Market Search"
for arch in arm64 x86_64; do
  lipo -archs "$APP_BIN" | grep -qw "$arch" || { echo "the archived app has no $arch slice" >&2; exit 1; }
done
# destination=upload (ExportOptions.plist) signs with the Developer ID certificate and sends
# the app to Apple's notary service.
xcodebuild -exportArchive -archivePath "$B/MarketSearch.xcarchive" -exportOptionsPlist ExportOptions.plist \
  -exportPath "$B/export" -allowProvisioningUpdates ${AUTH[@]+"${AUTH[@]}"}
# The notarized, stapled app can be exported once Apple accepts it (usually a few minutes).
for i in $(seq 1 60); do
  if xcodebuild -exportNotarizedApp -archivePath "$B/MarketSearch.xcarchive" -exportPath "$B/notarized" \
       ${AUTH[@]+"${AUTH[@]}"} >"$B/export-notarized.log" 2>&1; then
    break
  fi
  [ "$i" = 60 ] && { cat "$B/export-notarized.log" >&2; echo "notarization did not finish in 30 minutes" >&2; exit 1; }
  sleep 30
done
APP="$B/notarized/Market Search.app"
spctl -a -vv "$APP" 2>&1 | tee /dev/stderr | grep -q 'Notarized Developer ID' || { echo "the app is not notarized" >&2; exit 1; }

mkdir -p "$B/dmg"
cp -R "$APP" "$B/dmg/"
ln -s /Applications "$B/dmg/Applications"
hdiutil create -quiet -volname "Market Search" -srcfolder "$B/dmg" -ov -format UDZO "$B/MarketSearch.dmg"
ls -lh "$B/MarketSearch.dmg"
if [ "$MODE" = --upload ]; then
  gh release upload "$TAG" "$B/MarketSearch.dmg" --repo efoltyn/market-search --clobber
  echo "https://github.com/efoltyn/market-search/releases/latest/download/MarketSearch.dmg"
fi
