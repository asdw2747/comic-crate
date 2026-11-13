#!/usr/bin/env bash
# Build a self-contained ComicCrate AppImage on Linux.
#
# Usage:
#   ./packaging/build-appimage.sh
#
# Output:
#   dist/ComicCrate-<version>-<arch>.AppImage

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(
  sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1
)"
if [[ -z "$VERSION" ]]; then
  echo "error: could not read the package version from Cargo.toml" >&2
  exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64 | aarch64) ;;
  *)
    echo "error: unsupported architecture: $ARCH" >&2
    echo "       AppImage builds are supported here for x86_64 and aarch64." >&2
    exit 1
    ;;
esac

APPDIR="$ROOT/build/appimage/ComicCrate.AppDir"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/comiccrate-appimage"
OUTPUT_DIR="$ROOT/dist"
OUTPUT="$OUTPUT_DIR/ComicCrate-${VERSION}-${ARCH}.AppImage"

mkdir -p "$CACHE_DIR" "$OUTPUT_DIR"

missing=()
for tool in cargo cc c++ curl; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    missing+=("$tool")
  fi
done
if ((${#missing[@]} > 0)); then
  echo "error: missing build tools: ${missing[*]}" >&2
  echo >&2
  echo "Install a Rust toolchain and C/C++ build tools first:" >&2
  echo "  Debian/Ubuntu: sudo apt install build-essential pkg-config curl" >&2
  echo "  Fedora:        sudo dnf install gcc gcc-c++ make curl pkgconf-pkg-config" >&2
  echo "  Arch:          sudo pacman -S --needed base-devel curl pkgconf" >&2
  echo >&2
  echo "Then install Rust from https://rustup.rs/ if cargo is still missing." >&2
  exit 1
fi

download() {
  # download <name> <url...>
  local name="$1"
  shift
  local destination="$CACHE_DIR/$name"

  if [[ -x "$destination" ]]; then
    printf '%s\n' "$destination"
    return 0
  fi

  local url
  local last_error=""
  for url in "$@"; do
    echo "Downloading $name..." >&2
    rm -f "$destination.part"
    if curl --location --fail --retry 3 --output "$destination.part" "$url"; then
      mv "$destination.part" "$destination"
      chmod +x "$destination"
      printf '%s\n' "$destination"
      return 0
    fi
    last_error="$url"
  done

  echo "error: could not download $name (last URL: $last_error)" >&2
  return 1
}

echo "Building ComicCrate $VERSION for $ARCH..."
cargo build --release --locked

echo "Preparing AppDir..."
rm -rf "$APPDIR"
mkdir -p \
  "$APPDIR/usr/bin" \
  "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/metainfo" \
  "$APPDIR/usr/share/icons/hicolor/256x256/apps"

install -m 755 "$ROOT/target/release/comic-crate" "$APPDIR/usr/bin/comic-crate"
install -m 755 "$ROOT/target/release/comic-crate-cli" "$APPDIR/usr/bin/comic-crate-cli"
install -m 644 "$ROOT/assets/icon.png" \
  "$APPDIR/usr/share/icons/hicolor/256x256/apps/comiccrate.png"
install -m 644 "$ROOT/packaging/comiccrate.desktop" \
  "$APPDIR/usr/share/applications/comiccrate.desktop"
install -m 644 "$ROOT/packaging/comiccrate.appdata.xml" \
  "$APPDIR/usr/share/metainfo/comiccrate.appdata.xml"
install -m 755 "$ROOT/packaging/AppRun" "$APPDIR/AppRun"

ln -sfn "usr/share/icons/hicolor/256x256/apps/comiccrate.png" "$APPDIR/comiccrate.png"
ln -sfn "usr/share/applications/comiccrate.desktop" "$APPDIR/comiccrate.desktop"

# linuxdeploy copies required shared libraries into the AppDir. It is helpful
# for portability, but the AppImage still works on this machine without it.
if command -v patchelf >/dev/null 2>&1; then
  if LINUXDEPLOY="$(
    download "linuxdeploy-${ARCH}.AppImage" \
      "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-${ARCH}.AppImage"
  )"; then
    echo "Bundling shared libraries..."
    if ! APPIMAGE_EXTRACT_AND_RUN=1 "$LINUXDEPLOY" \
      --appdir "$APPDIR" \
      --executable "$APPDIR/usr/bin/comic-crate" \
      --desktop-file "$APPDIR/usr/share/applications/comiccrate.desktop" \
      --icon-file "$APPDIR/usr/share/icons/hicolor/256x256/apps/comiccrate.png"; then
      echo "warning: linuxdeploy failed; continuing without bundled libraries" >&2
    fi
  fi
else
  echo "note: patchelf not found; skipping shared-library bundling." >&2
  echo "      Install it for a more portable AppImage: sudo apt install patchelf" >&2
fi

echo "Building AppImage..."
APPIMAGETOOL="$(
  download "appimagetool-${ARCH}.AppImage" \
    "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${ARCH}.AppImage" \
    "https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-${ARCH}.AppImage"
)"
APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$ARCH" "$APPIMAGETOOL" "$APPDIR" "$OUTPUT"
chmod +x "$OUTPUT"

echo
echo "Created: $OUTPUT"
echo "Run it with:"
echo "  \"$OUTPUT\""
