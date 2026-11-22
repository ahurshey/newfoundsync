#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.
#
# Build the Newfoundsync Android TV app: android/build/newfoundsync-tv.apk
#
# Plain Android SDK tools, no Gradle: aapt2 (resources + manifest), javac, d8 (dex), zipalign,
# apksigner. Needs a JDK 17 (`javac` on PATH or JAVA_HOME) and an Android SDK with
# build-tools;35.0.0 and platforms;android-35 (ANDROID_SDK_ROOT, default ~/Android/Sdk).
#
# Signing: with a key kept OUTSIDE the repo (NFS_TV_KEYSTORE, default
# ~/.android/newfoundsync-tv.keystore), created on first build. Keep it: an update signed with a
# different key will not install over the old app. Its password is NFS_TV_KEYPASS (default
# "newfoundsync"); this is a sideloaded test app, not a store release.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
SDK=${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}
BT=$SDK/build-tools/35.0.0
JAR=$SDK/platforms/android-35/android.jar
OUT=$HERE/build
KS=${NFS_TV_KEYSTORE:-$HOME/.android/newfoundsync-tv.keystore}
PASS=${NFS_TV_KEYPASS:-newfoundsync}

[ -x "$BT/aapt2" ] || { echo "no Android build-tools 35.0.0 under $SDK" >&2; exit 1; }
[ -f "$JAR" ] || { echo "no Android platform 35 under $SDK" >&2; exit 1; }
if [ -n "${JAVA_HOME:-}" ]; then PATH=$JAVA_HOME/bin:$PATH; fi
command -v javac >/dev/null || { echo "no javac: install a JDK 17 or set JAVA_HOME" >&2; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT/gen" "$OUT/classes" "$OUT/dex"

echo "resources"
"$BT/aapt2" compile --dir "$HERE/res" -o "$OUT/res.zip"
"$BT/aapt2" link -o "$OUT/base.apk" -I "$JAR" --manifest "$HERE/AndroidManifest.xml" \
    --java "$OUT/gen" "$OUT/res.zip"

echo "compile"
# -source/-target 8 with android.jar as the boot classpath: compiles against Android's API, not the
# JDK's. core-lambda-stubs.jar supplies the lambda bootstrap android.jar leaves out; d8 desugars it.
find "$HERE/src" "$OUT/gen" -name '*.java' > "$OUT/sources.txt"
javac -source 8 -target 8 -Xlint:-options -encoding UTF-8 -bootclasspath "$JAR:$BT/core-lambda-stubs.jar" \
    -d "$OUT/classes" @"$OUT/sources.txt"

echo "dex"
find "$OUT/classes" -name '*.class' > "$OUT/classes.txt"
"$BT/d8" --release --min-api 24 --lib "$JAR" --output "$OUT/dex" @"$OUT/classes.txt"

echo "package"
python3 - "$OUT/base.apk" "$OUT/dex/classes.dex" <<'EOF'
import sys, zipfile
with zipfile.ZipFile(sys.argv[1], "a", zipfile.ZIP_DEFLATED) as z:
    z.write(sys.argv[2], "classes.dex")
EOF
"$BT/zipalign" -f -p 4 "$OUT/base.apk" "$OUT/aligned.apk"

if [ ! -f "$KS" ]; then
    echo "creating the signing key at $KS (keep it: updates must be signed with the same key)"
    mkdir -p "$(dirname "$KS")"
    keytool -genkeypair -keystore "$KS" -storepass "$PASS" -keypass "$PASS" -alias newfoundsync-tv \
        -keyalg RSA -keysize 3072 -validity 10000 -dname "CN=Newfoundsync TV" >/dev/null
fi

echo "sign"
"$BT/apksigner" sign --ks "$KS" --ks-pass "pass:$PASS" --key-pass "pass:$PASS" \
    --out "$OUT/newfoundsync-tv.apk" "$OUT/aligned.apk"
"$BT/apksigner" verify "$OUT/newfoundsync-tv.apk"
echo "built $OUT/newfoundsync-tv.apk"
