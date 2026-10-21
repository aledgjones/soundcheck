#!/bin/sh
# Builds axe-reader against the FB frameworks that Homebrew's axe ships (brew install cameroncooke/axe/axe).
# Their Swift interfaces import Apple's private simulator modules, whose headers come from the idb fork AXe pins.
set -e
cd "$(dirname "$0")"
FW="$(brew --prefix axe)/libexec/Frameworks"
# AXe 1.8.0's pinned idb (scripts/build.sh DEFAULT_IDB_GIT_REF); bump together with axe.
IDB_REF=604c51013438f0c3603b720a05a44b7c5b8f286d
IDB=build/idb
mkdir -p build
if [ ! -d "$IDB/PrivateHeaders" ]; then
  rm -rf "$IDB" && mkdir -p "$IDB"
  (cd "$IDB" && git init -q && git remote add origin https://github.com/cameroncooke/idb.git &&
    git fetch -q --depth 1 origin "$IDB_REF" && git checkout -q FETCH_HEAD)
fi
H="$IDB/PrivateHeaders"
# Homebrew's frameworks were built by an older Swift, so the compiler reads their .swiftinterface, which writes every
# type as `FBSimulatorControl.X`; that resolves to the class FBSimulatorControl, not the module (a known Swift issue).
# Compile against a shadow copy with the module prefixes stripped; link and run against the real frameworks.
SHADOW=build/fw
rm -rf "$SHADOW" && mkdir -p "$SHADOW"
for f in "$FW"/*.framework; do
  name=$(basename "$f" .framework)
  cp -R "$f" "$SHADOW/"
  for i in "$SHADOW/$name.framework/Modules/$name.swiftmodule"/*.swiftinterface; do
    [ -f "$i" ] && sed -i '' -E "s/(^|[^A-Za-z_])$name\.([A-Z][A-Za-z0-9_]*)/\1\2/g" "$i" &&
      perl -pi -e 's/\b([A-Z]\w+)\.\1\b/$1/g' "$i"
  done
  rm -f "$SHADOW/$name.framework/Modules/$name.swiftmodule"/*.swiftmodule
done
swiftc -O -swift-version 5 -parse-as-library -target arm64-apple-macos14 \
  -F "$SHADOW" -framework FBControlCore -framework FBSimulatorControl \
  -I "$H" -I "$H/AccessibilityPlatformTranslation" -I "$H/AXRuntime" -I "$H/CoreSimDeviceIO" \
  -I "$H/CoreSimulator" -I "$H/CoreSimulatorUtilities" -I "$H/SimulatorKit" \
  -Xlinker -rpath -Xlinker "$FW" \
  Reader.swift -o build/axe-reader
echo "built $(pwd)/build/axe-reader"
