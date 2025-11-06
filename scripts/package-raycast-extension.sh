#!/usr/bin/env bash
set -euo pipefail

# Package the Arrowhead Raycast extension into a .raycast archive.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXT_ROOT="$ROOT_DIR/integrations/raycast-extension"
DIST_DIR="$EXT_ROOT/dist"
BUILD_DIR="$EXT_ROOT/build"
ARCHIVE_NAME="arrowhead-search.raycast"
OUTPUT_PATH="$DIST_DIR/$ARCHIVE_NAME"

if [[ ! -f "$EXT_ROOT/package.json" ]]; then
  echo "Missing Raycast extension manifest at $EXT_ROOT/package.json" >&2
  exit 1
fi

mkdir -p "$DIST_DIR"

pushd "$EXT_ROOT" >/dev/null

cleanup() {
  rm -rf "$EXT_ROOT/node_modules" "$BUILD_DIR" "$tmpdir"
}
tmpdir="$(mktemp -d)"
trap cleanup EXIT

if command -v npm >/dev/null 2>&1; then
  npm ci >/dev/null
else
  echo "npm is required to package the Raycast extension" >&2
  exit 1
fi

npm run build >/dev/null

if [[ ! -d "$BUILD_DIR" ]]; then
  echo "Raycast build output not found at $BUILD_DIR" >&2
  exit 1
fi

if [[ -f "$EXT_ROOT/raycast-env.d.ts" ]]; then
  cp "$EXT_ROOT/raycast-env.d.ts" "$BUILD_DIR/"
fi

rm -f "$OUTPUT_PATH"

cp -R "$BUILD_DIR"/. "$tmpdir"/
pushd "$tmpdir" >/dev/null
zip -qr "$OUTPUT_PATH" .
popd >/dev/null

popd >/dev/null

echo "Packaged Raycast extension at $OUTPUT_PATH"
