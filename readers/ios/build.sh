#!/bin/sh
# Builds the reader against FBControlCore and FBSimulatorControl, built here from the idb fork AXe pins
# (needs XcodeGen: brew install xcodegen). The frameworks build once per IDB_REF, in about a minute.
set -e
cd "$(dirname "$0")"
command -v xcodegen >/dev/null || {
  echo "XcodeGen is needed to build idb's frameworks: brew install xcodegen" >&2
  exit 1
}
# AXe 1.8.0's pinned idb (scripts/build.sh DEFAULT_IDB_GIT_REF), which carries its fixes for new Xcodes.
IDB_REF=604c51013438f0c3603b720a05a44b7c5b8f286d
IDB=build/idb
FW=build/Frameworks
mkdir -p build
if [ ! -d "$IDB/PrivateHeaders" ] || [ "$(git -C "$IDB" rev-parse HEAD)" != "$IDB_REF" ]; then
  rm -rf "$IDB" && mkdir -p "$IDB"
  (cd "$IDB" && git init -q && git remote add origin https://github.com/cameroncooke/idb.git &&
    git fetch -q --depth 1 origin "$IDB_REF" && git checkout -q FETCH_HEAD)
fi
if [ "$(cat "$FW/.idb-ref" 2>/dev/null)" != "$IDB_REF" ]; then
  (cd "$IDB" && xcodegen generate --quiet)
  for scheme in FBControlCore XCTestBootstrap FBSimulatorControl; do
    echo "building $scheme"
    # idb's own warnings are many and not ours to fix, so the log is shown only on failure.
    xcodebuild -quiet -project "$IDB/FBSimulatorControl.xcodeproj" -scheme "$scheme" -configuration Release \
      -destination generic/platform=macOS -derivedDataPath build/derived build \
      ARCHS=arm64 ONLY_ACTIVE_ARCH=NO GCC_TREAT_WARNINGS_AS_ERRORS=NO SWIFT_TREAT_WARNINGS_AS_ERRORS=NO \
      >build/xcodebuild.log 2>&1 || {
      tail -40 build/xcodebuild.log >&2
      exit 1
    }
  done
  rm -rf "$FW" && mkdir -p "$FW"
  cp -R build/derived/Build/Products/Release/*.framework "$FW/"
  echo "$IDB_REF" >"$FW/.idb-ref"
fi
# The reader reports this on /ping and the driver compares it with reader.build-id, so an older one is replaced.
BUILD_ID=$({ cat Reader.swift; echo "$IDB_REF"; } | shasum -a 256 | cut -c1-12)
echo "let readerBuildID = \"$BUILD_ID\"" >build/BuildID.swift
H="$IDB/PrivateHeaders"
swiftc -O -swift-version 5 -parse-as-library -target arm64-apple-macos14 \
  -F "$FW" -framework FBControlCore -framework FBSimulatorControl \
  -I "$H" -I "$H/AccessibilityPlatformTranslation" -I "$H/AXRuntime" -I "$H/CoreSimDeviceIO" \
  -I "$H/CoreSimulator" -I "$H/CoreSimulatorUtilities" -I "$H/SimulatorKit" \
  -Xlinker -rpath -Xlinker @executable_path/Frameworks \
  Reader.swift build/BuildID.swift -o build/reader
echo "$BUILD_ID" >build/reader.build-id
echo "built $(pwd)/build/reader ($BUILD_ID)"
