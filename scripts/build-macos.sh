#!/bin/sh
set -eu
cd -- "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
export MACOSX_DEPLOYMENT_TARGET=12.0
if [ "$(uname -s)" != Darwin ]; then echo '请在 macOS 上构建。' >&2; exit 1; fi
./scripts/build-icons.sh
cargo build --locked --release --manifest-path native/Cargo.toml "$@"
MINOR_APP="dist/Minorminor.app"
MINOR_BUILD="native/target/release/minorminor"
if [ -n "${CARGO_TARGET_DIR:-}" ]; then MINOR_BUILD="$CARGO_TARGET_DIR/release/minorminor"; fi
if [ ! -f "$MINOR_BUILD" ]; then echo '未找到本机 release 二进制，请检查构建目标。' >&2; exit 1; fi
mkdir -p "$MINOR_APP/Contents/MacOS" "$MINOR_APP/Contents/Resources/vendor/mole" "$MINOR_APP/Contents/Resources/scripts"
cp "$MINOR_BUILD" "$MINOR_APP/Contents/MacOS/Minorminor"
cp native/Info.plist "$MINOR_APP/Contents/Info.plist"
MINOR_ICON_HASH=$(shasum -a 256 native/icons/icon.icns | cut -c 1-12)
MINOR_ICON_NAME="Minorminor-$MINOR_ICON_HASH.icns"
rm -f "$MINOR_APP/Contents/Resources/Minorminor.icns" "$MINOR_APP/Contents/Resources"/Minorminor-*.icns
cp native/icons/icon.icns "$MINOR_APP/Contents/Resources/$MINOR_ICON_NAME"
/usr/libexec/PlistBuddy -c "Set :CFBundleIconFile $MINOR_ICON_NAME" "$MINOR_APP/Contents/Info.plist"
cp vendor/mole/mole vendor/mole/LICENSE "$MINOR_APP/Contents/Resources/vendor/mole/"
ditto vendor/mole/bin "$MINOR_APP/Contents/Resources/vendor/mole/bin"
ditto vendor/mole/lib "$MINOR_APP/Contents/Resources/vendor/mole/lib"
cp scripts/check-clean-target.sh "$MINOR_APP/Contents/Resources/scripts/"
cp UPSTREAM.json LICENSE "$MINOR_APP/Contents/Resources/"
cp public/fonts/OFL.txt "$MINOR_APP/Contents/Resources/IBM-Plex-OFL.txt"
chmod +x "$MINOR_APP/Contents/MacOS/Minorminor"
codesign --force --sign - "$MINOR_APP/Contents/MacOS/Minorminor"
codesign --force --sign - "$MINOR_APP"
codesign --verify --deep --strict "$MINOR_APP"
touch "$MINOR_APP"
MINOR_LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
if [ -x "$MINOR_LSREGISTER" ]; then "$MINOR_LSREGISTER" -f "$MINOR_APP"; fi
echo "已构建：$PWD/$MINOR_APP"
