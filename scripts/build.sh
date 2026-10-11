#!/bin/sh
# Build rec, assemble dist/Rec.app, and code-sign it with a stable identity so
# macOS privacy grants (screen/mic/camera) survive rebuilds.
set -eu
cd "$(dirname "$0")/.."
PROFILE="${PROFILE:-release}"
# The repo is synced between machines (Mutagen): keep build output outside it.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/rec-rs-target}"
if [ "$PROFILE" = release ]; then cargo build --release; else cargo build; fi
APP=dist/Rec.app
STAGE=dist/.Rec.app.new
# Build the bundle beside the old one and swap it in atomically: overwriting a
# running binary in place makes macOS kill an in-progress recording.
rm -rf "$STAGE"
mkdir -p "$STAGE/Contents/MacOS"
cp support/Info.plist "$STAGE/Contents/Info.plist"
cp "$CARGO_TARGET_DIR/$PROFILE/rec" "$STAGE/Contents/MacOS/rec"
IDENTITY="${REC_SIGN_IDENTITY:-$(security find-identity -v -p codesigning | awk -F'"' '/Apple Development|Developer ID Application/ {print $2; exit}')}"
if [ -z "$IDENTITY" ]; then
  echo "warning: no signing identity found; ad-hoc signing (permissions reset on every rebuild)" >&2
  IDENTITY=-
fi
codesign --force --sign "$IDENTITY" --identifier com.heyfabrika.rec "$STAGE"
codesign --verify --strict "$STAGE"
mkdir -p "$APP/Contents/MacOS"
cp "$STAGE/Contents/Info.plist" "$APP/Contents/Info.plist.new" && mv -f "$APP/Contents/Info.plist.new" "$APP/Contents/Info.plist"
mv -f "$STAGE/Contents/MacOS/rec" "$APP/Contents/MacOS/rec"
rm -rf "$APP/Contents/_CodeSignature"
mv "$STAGE/Contents/_CodeSignature" "$APP/Contents/_CodeSignature"
rm -rf "$STAGE"
codesign --verify --strict "$APP"
ln -sf "Rec.app/Contents/MacOS/rec" dist/rec
echo "built $APP (signed: $IDENTITY)"
echo "run:   dist/rec --help"
