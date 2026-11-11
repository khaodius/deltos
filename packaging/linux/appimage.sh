#!/usr/bin/env bash
# Builds the Deltos AppImage from a release binary. The release workflow runs it on Ubuntu
# 22.04; it also works locally after `cargo build --release`.
#
#   packaging/linux/appimage.sh VERSION [BINARY] [OUTPUT_DIR]
#
# appimagetool is downloaded at a pinned release and checked against its SHA-256 before it
# runs, so the image is always built by the same tool. It needs desktop-file-validate from the
# desktop-file-utils package. The image bundles only Deltos itself, like the .tar.gz: it needs
# the system's Vulkan, X11 or Wayland and fontconfig libraries, which every current desktop
# distribution has.

set -euo pipefail

readonly appimagetool_version="1.9.0"
readonly appimagetool_sha256="46fdd785094c7f6e545b61afcfb0f3d98d8eab243f644b4b17698c01d06083d1"
readonly appimagetool_url="https://github.com/AppImage/appimagetool/releases/download/${appimagetool_version}/appimagetool-x86_64.AppImage"

version="${1:?usage: appimage.sh VERSION [BINARY] [OUTPUT_DIR]}"
binary="${2:-target/release/deltos}"
output_dir="${3:-dist}"
root=$(cd "$(dirname "$0")/../.." && pwd)

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

[ -x "$binary" ] || fail "$binary is not an executable; build with cargo build --release first"
command -v desktop-file-validate > /dev/null ||
  fail "desktop-file-validate is missing; install desktop-file-utils"

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

tool="$workdir/appimagetool"
curl --fail --location --silent --show-error --retry 3 --output "$tool" "$appimagetool_url" ||
  fail "could not download appimagetool $appimagetool_version"
printf '%s  %s\n' "$appimagetool_sha256" "$tool" | sha256sum --check --quiet - ||
  fail "appimagetool does not match its pinned SHA-256"
chmod 755 "$tool"

appdir="$workdir/Deltos.AppDir"
install -D -m 755 "$binary" "$appdir/usr/bin/deltos"
install -D -m 644 "$root/packaging/linux/deltos.desktop" "$appdir/usr/share/applications/deltos.desktop"
install -m 644 "$root/packaging/linux/deltos.desktop" "$appdir/deltos.desktop"
for size in 16 24 32 48 64 128 256 512; do
  install -D -m 644 "$root/assets/icons/png/deltos-$size.png" \
    "$appdir/usr/share/icons/hicolor/${size}x${size}/apps/deltos.png"
done
install -m 644 "$root/assets/icons/png/deltos-256.png" "$appdir/deltos.png"
ln -s deltos.png "$appdir/.DirIcon"
install -m 755 "$root/packaging/linux/AppRun" "$appdir/AppRun"

mkdir -p "$output_dir"
output="$output_dir/deltos-${version}-x86_64.AppImage"
# The tool is itself an AppImage; extracting it first means FUSE is not needed on the builder.
APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 "$tool" --no-appstream "$appdir" "$output"
chmod 755 "$output"
ls -l "$output"
