#!/bin/sh
# Builds "Intel SIAM.app" into target/release/bundle/ and, with --install, copies it to /Applications.
#
#   tools/package/macos.sh [--install]
set -eu

cd "$(dirname "$0")/../.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
APP="target/release/bundle/Intel SIAM.app"

cargo build --release

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/intel-siam "$APP/Contents/MacOS/intel-siam"
cp assets/icon.icns "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>Intel SIAM</string>
	<key>CFBundleDisplayName</key><string>Intel SIAM</string>
	<key>CFBundleIdentifier</key><string>io.github.rogerznts.intel-siam</string>
	<key>CFBundleExecutable</key><string>intel-siam</string>
	<key>CFBundleIconFile</key><string>AppIcon</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>$VERSION</string>
	<key>CFBundleVersion</key><string>$VERSION</string>
	<key>LSMinimumSystemVersion</key><string>11.0</string>
	<key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
	<key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
plutil -lint "$APP/Contents/Info.plist" >/dev/null
codesign --force --sign - "$APP"

if [ "${1:-}" = "--install" ]; then
	rm -rf "/Applications/Intel SIAM.app"
	cp -R "$APP" /Applications/
	touch "/Applications/Intel SIAM.app"
	echo "installed /Applications/Intel SIAM.app"
else
	echo "built $APP"
fi
