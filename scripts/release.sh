#!/bin/sh
# Builds a prebuilt soundcheck for Apple Silicon Macs: the server, both readers and the Claude Code plugin in one
# tarball under target/package/. `--publish` also uploads it as the GitHub release v<version>; without it, nothing
# leaves this Mac.
set -e
cd "$(dirname "$0")/.."
PUBLISH=
if [ "$1" = "--publish" ]; then
  PUBLISH=1
fi
[ "$(uname -m)" = arm64 ] || {
  echo "build on an Apple Silicon Mac" >&2
  exit 1
}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
PLUGIN_VERSION=$(sed -n 's/^ *"version": "\(.*\)",*$/\1/p' plugin/.claude-plugin/plugin.json)
[ "$PLUGIN_VERSION" = "$VERSION" ] || {
  echo "plugin.json is $PLUGIN_VERSION but Cargo.toml is $VERSION: bump both" >&2
  exit 1
}
if [ -n "$PUBLISH" ]; then
  [ -z "$(git status --porcelain)" ] || {
    echo "commit first: a release is built from a clean tree" >&2
    exit 1
  }
  [ "$(git branch --show-current)" = main ] || {
    echo "release from main" >&2
    exit 1
  }
fi

cargo build --release --locked -p soundcheck
readers/ios/build.sh
readers/android/build.sh

NAME="soundcheck-$VERSION-macos-arm64"
OUT="target/package/$NAME"
rm -rf "$OUT" "$OUT.tar.gz"
mkdir -p "$OUT/bin" "$OUT/readers/ios" "$OUT/readers/android" "$OUT/.claude-plugin"
cp target/release/soundcheck "$OUT/bin/"
cp readers/ios/build/reader "$OUT/readers/ios/"
cp -R readers/ios/build/Frameworks "$OUT/readers/ios/"
rm -f "$OUT/readers/ios/Frameworks/.idb-ref"
cp readers/android/build/reader.apk "$OUT/readers/android/"
cp -R plugin "$OUT/"
cp .claude-plugin/marketplace.json "$OUT/.claude-plugin/"
cp README.md LICENSE "$OUT/"
echo "$VERSION $(git rev-parse HEAD)$(git diff --quiet HEAD || echo ' (uncommitted changes)')" >"$OUT/BUILD"
tar -czf "$OUT.tar.gz" -C "$OUT" .
echo "packaged $(pwd)/$OUT.tar.gz ($(du -h "$OUT.tar.gz" | cut -f1))"

if [ -n "$PUBLISH" ]; then
  gh release create "v$VERSION" "$OUT.tar.gz" --title "soundcheck $VERSION" \
    --notes "Unpack into ~/soundcheck and put ~/soundcheck/bin on PATH; see README.md."
fi
