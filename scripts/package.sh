#!/usr/bin/env bash
# Builds the release binaries and packs them for distribution in dist/:
#
#   amel-<version>-<target>[-browser].tar.gz   (.zip on Windows)
#   amel-<version>-<target>[-browser].deb      on Linux, with a menu entry
#   amel-<version>-<target>[-browser].dmg      on macOS, with the .app
#
# each with a .sha256 checksum next to it.
#
#   scripts/package.sh                                 # lean build, this machine
#   FEATURES=browser scripts/package.sh                # with Chromium rendering
#   TARGET=x86_64-apple-darwin scripts/package.sh      # another target
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
host=$(rustc -vV | sed -n 's/^host: //p')
target=${TARGET:-$host}
features=${FEATURES:-}

build_args=(--release --locked -p mc-cli -p mc-gui --target "$target")
if [[ -n $features ]]; then
  build_args+=(--features "mc-cli/$features,mc-gui/$features")
fi
cargo build "${build_args[@]}"

bin="target/$target/release"
name="amel-$version-$target${features:+-$features}"
exe=""
[[ $target == *windows* ]] && exe=".exe"

mkdir -p dist
made=()

checksum() {
  (
    cd dist
    if command -v sha256sum >/dev/null; then
      sha256sum "$1" >"$1.sha256"
    else
      shasum -a 256 "$1" >"$1.sha256"
    fi
  )
  made+=("dist/$1")
}

# The archive: both binaries and the README.
stage="dist/$name"
rm -rf "$stage"
mkdir "$stage"
cp "$bin/amel$exe" "$bin/amel-gui$exe" README.md "$stage/"
if [[ $target == *windows* ]]; then
  if command -v 7z >/dev/null; then
    (cd dist && 7z a -tzip "$name.zip" "$name" >/dev/null)
  else
    (cd dist && powershell -NoProfile -Command "Compress-Archive -Path '$name' -DestinationPath '$name.zip' -Force")
  fi
  checksum "$name.zip"
else
  (cd dist && tar czf "$name.tar.gz" "$name")
  checksum "$name.tar.gz"
fi
rm -rf "$stage"

# Linux: a Debian package with a menu entry and icon. The lean and browser
# builds install the same files, so each package conflicts with the other.
if [[ $target == *linux* ]] && command -v dpkg-deb >/dev/null; then
  case $target in
    x86_64-*) arch=amd64 ;;
    aarch64-*) arch=arm64 ;;
    *) arch="" ;;
  esac
  if [[ -n $arch ]]; then
    if [[ -n $features ]]; then
      package="amel-$features" other="amel"
    else
      package="amel" other="amel-browser"
    fi
    root="dist/$name-deb"
    rm -rf "$root"
    install -Dm755 "$bin/amel" "$root/usr/bin/amel"
    install -Dm755 "$bin/amel-gui" "$root/usr/bin/amel-gui"
    install -Dm644 assets/amel.desktop "$root/usr/share/applications/amel.desktop"
    install -Dm644 assets/icon.png "$root/usr/share/pixmaps/amel.png"
    install -Dm644 README.md "$root/usr/share/doc/amel/README.md"
    mkdir -p "$root/DEBIAN"
    cat >"$root/DEBIAN/control" <<EOF
Package: $package
Version: $version
Architecture: $arch
Maintainer: Mahdi Jouini
Section: web
Priority: optional
Depends: libc6, libxkbcommon0, libxkbcommon-x11-0, libx11-xcb1, libwayland-client0, libvulkan1, libfontconfig1, libfreetype6
Conflicts: $other
Replaces: $other
Description: Amel Website Copier
 Copies websites for offline browsing: a browser-impersonating HTTP
 client, link rewriting, resume and update, and a desktop app.
EOF
    dpkg-deb --root-owner-group --build "$root" "dist/$name.deb" >/dev/null
    rm -rf "$root"
    checksum "$name.deb"
  fi
fi

# macOS: "Amel Website Copier.app" in a disk image, with the command-line
# tool next to it.
if [[ $target == *apple-darwin* ]] && command -v hdiutil >/dev/null; then
  app_name="Amel Website Copier"
  dmg_root="dist/$name-dmg"
  app="$dmg_root/$app_name.app"
  rm -rf "$dmg_root"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  cp "$bin/amel-gui" "$app/Contents/MacOS/amel-gui"
  cp "$bin/amel" "$dmg_root/amel"
  iconset="dist/amel.iconset"
  rm -rf "$iconset"
  mkdir "$iconset"
  for size in 16 32 128 256 512; do
    sips -z $size $size assets/icon.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) assets/icon.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$iconset" -o "$app/Contents/Resources/amel.icns"
  rm -rf "$iconset"
  cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$app_name</string>
  <key>CFBundleDisplayName</key><string>$app_name</string>
  <key>CFBundleIdentifier</key><string>com.amel.websitecopier</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleExecutable</key><string>amel-gui</string>
  <key>CFBundleIconFile</key><string>amel</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
  # Unsigned builds still need an ad-hoc signature to run on Apple Silicon.
  codesign --force --deep --sign - "$app"
  ln -s /Applications "$dmg_root/Applications"
  rm -f "dist/$name.dmg"
  hdiutil create -volname "$app_name" -srcfolder "$dmg_root" -ov -format UDZO "dist/$name.dmg" >/dev/null
  rm -rf "$dmg_root"
  checksum "$name.dmg"
fi

printf '%s\n' "${made[@]}"
