#!/usr/bin/env bash
# Build libastra_hotkey.dylib for Apple Silicon AND Intel and fuse them into one
# universal dylib:  target/universal-apple-darwin/release/libastra_hotkey.dylib
#
# Needs: rustup targets aarch64-apple-darwin + x86_64-apple-darwin, Xcode CLT (lipo).
set -euo pipefail
cd "$(dirname "$0")/.."

for t in aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build --release --target "$t"
done

out=target/universal-apple-darwin/release
mkdir -p "$out"
lipo -create \
  target/aarch64-apple-darwin/release/libastra_hotkey.dylib \
  target/x86_64-apple-darwin/release/libastra_hotkey.dylib \
  -output "$out/libastra_hotkey.dylib"

lipo -info "$out/libastra_hotkey.dylib"
# The daemon resolves these one by one; a missing one silently loses a feature.
for arch in arm64 x86_64; do
  n=$(nm -gU -arch "$arch" "$out/libastra_hotkey.dylib" | grep -c ' _hotkey_')
  echo "$arch: $n hotkey_* exports"
  [ "$n" -eq 15 ] || { echo "expected 15 exports for $arch" >&2; exit 1; }
done
otool -D "$out/libastra_hotkey.dylib" | grep "^@" | sort -u   # install name: @rpath/libastra_hotkey.dylib
