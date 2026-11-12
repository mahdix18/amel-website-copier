#!/usr/bin/env bash
# Builds the release binaries for this machine and packs them for
# distribution: dist/mistercopy-<version>-<target>.tar.gz (.zip on Windows)
# plus a .sha256 checksum next to it.
#
#   scripts/package.sh                   # lean build
#   FEATURES=browser scripts/package.sh  # with Chromium rendering
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
target=$(rustc -vV | sed -n 's/^host: //p')
features=${FEATURES:-}

build_args=(--release --locked -p mc-cli -p mc-gui)
if [[ -n $features ]]; then
  build_args+=(--features "mc-cli/$features,mc-gui/$features")
fi
cargo build "${build_args[@]}"

name="mistercopy-$version-$target${features:+-$features}"
exe=""
[[ $target == *windows* ]] && exe=".exe"

mkdir -p dist
stage="dist/$name"
rm -rf "$stage"
mkdir "$stage"
cp "target/release/mistercopy$exe" "target/release/mistercopy-gui$exe" README.md "$stage/"

cd dist
if [[ $target == *windows* ]]; then
  archive="$name.zip"
  7z a -tzip "$archive" "$name" >/dev/null
else
  archive="$name.tar.gz"
  tar czf "$archive" "$name"
fi
if command -v sha256sum >/dev/null; then
  sha256sum "$archive" >"$archive.sha256"
else
  shasum -a 256 "$archive" >"$archive.sha256"
fi
rm -rf "$name"
echo "dist/$archive"
