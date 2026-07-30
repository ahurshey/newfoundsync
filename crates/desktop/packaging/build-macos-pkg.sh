#!/bin/bash
# Build a macOS installer package (.pkg) for Apple Silicon.
#
# Ships ONE payload two ways, matching how the Linux packages are split:
#   /Applications/Newfoundsync.app      -- the GUI, double-clickable, in the Dock/Launchpad
#   /usr/local/bin/newfoundsync         -- symlink to the same binary, for --headless / CLI use
# One binary, so the two can never drift out of sync (unlike the .deb/.rpm split, where both
# packages own /usr/bin/newfoundsync and have to Conflict).
set -x
VER=0.0.6
SRC="$HOME/nfs-src"
ROOT=/tmp/nfspkg/root
APP="$ROOT/Applications/Newfoundsync.app"
OUT="$HOME/Newfoundsync-${VER}-arm64.pkg"

rm -rf /tmp/nfspkg
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$ROOT/usr/local/bin"
cp "$SRC/target/release/newfoundsync" "$APP/Contents/MacOS/newfoundsync"
chmod 755 "$APP/Contents/MacOS/newfoundsync"

# Icon: .icns from the 512px brand PNG (sips + iconutil both ship with macOS). Non-fatal.
ICONSET=/tmp/nfspkg/icon.iconset
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s "$SRC/branding/icon-512.png" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null 2>&1
  d=$((s*2))
  sips -z $d $d "$SRC/branding/icon-512.png" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null 2>&1
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Newfoundsync.icns" 2>&1 || echo "ICNS-SKIPPED"

# Two separate TCC gates, and they are NOT interchangeable:
#   NSAudioCaptureUsageDescription  -> "System Audio Recording", what a CoreAudio process tap needs.
#                                      This is the one that matters now that capture is a tap.
#   NSMicrophoneUsageDescription    -> the ordinary mic gate. Still declared because the fallback
#                                      path can open a virtual loopback device, and macOS treats
#                                      ANY audio input device as microphone access.
# Missing the right key does not produce an error dialog -- the tap just yields silence forever.
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>Newfoundsync</string>
  <key>CFBundleDisplayName</key>       <string>Newfoundsync</string>
  <key>CFBundleIdentifier</key>        <string>ca.newfoundsync.Newfoundsync</string>
  <key>CFBundleVersion</key>           <string>${VER}</string>
  <key>CFBundleShortVersionString</key><string>${VER}</string>
  <key>CFBundleExecutable</key>        <string>newfoundsync</string>
  <key>CFBundleIconFile</key>          <string>Newfoundsync</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>LSMinimumSystemVersion</key>    <string>11.0</string>
  <key>NSHighResolutionCapable</key>   <true/>
  <key>NSAudioCaptureUsageDescription</key>
  <string>Newfoundsync captures this Mac's system audio so listeners on your local network can hear it in sync. It does not record the microphone.</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>Newfoundsync records a virtual loopback audio device to share this Mac's sound with listeners on your local network. It refuses to record the built-in microphone.</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>Newfoundsync serves audio to browsers on your local network.</string>
</dict>
</plist>
PLIST

ln -s /Applications/Newfoundsync.app/Contents/MacOS/newfoundsync "$ROOT/usr/local/bin/newfoundsync"

pkgbuild --root "$ROOT" \
         --identifier ca.newfoundsync.Newfoundsync \
         --version "$VER" \
         --install-location / \
         "$OUT"
echo "PKGBUILD-RC=$?"
ls -lh "$OUT"
echo "--- payload ---"
pkgutil --payload-files "$OUT"
echo "MAC-PKG-DONE"
