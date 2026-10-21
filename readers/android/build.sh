#!/bin/sh
# Builds reader.apk with the plain SDK tools (no Gradle): javac, d8, aapt2, zipalign, apksigner.
set -e
cd "$(dirname "$0")"
SDK="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
BT="$SDK/build-tools/35.0.0"
JAR="$SDK/platforms/android-35/android.jar"
export JAVA_HOME="${JAVA_HOME:-$(/usr/libexec/java_home -v 17)}"
rm -rf build && mkdir -p build/classes build/dex
"$JAVA_HOME/bin/javac" --release 17 -classpath "$JAR" -d build/classes $(find src -name '*.java')
"$BT/d8" --lib "$JAR" --min-api 34 --output build/dex $(find build/classes -name '*.class')
"$BT/aapt2" link -o build/unsigned.apk --manifest AndroidManifest.xml -I "$JAR"
(cd build/dex && zip -q ../unsigned.apk classes.dex)
"$BT/zipalign" -f 4 build/unsigned.apk build/aligned.apk
"$BT/apksigner" sign --ks "$HOME/.android/debug.keystore" --ks-pass pass:android --key-pass pass:android \
  --out build/reader.apk build/aligned.apk
echo "built $(pwd)/build/reader.apk"
