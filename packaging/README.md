# Building the ComicCrate AppImage

This folder contains everything needed to build a Linux AppImage from the
source tree.

## Requirements

- A 64-bit Linux system (x86_64 or aarch64)
- Rust and Cargo: <https://rustup.rs/>
- C/C++ compiler, `make`, `pkg-config`, and `curl`
- Optional but recommended: `patchelf` (lets the script bundle shared
  libraries for portability) and `desktop-file-utils`
- Optional: `zenity` for the **Select Files** / **Add folder** dialogs. Drag
  and drop works without it.

Install build tools:

```sh
# Debian / Ubuntu
sudo apt update
sudo apt install build-essential pkg-config curl patchelf desktop-file-utils zenity

# Fedora
sudo dnf install gcc gcc-c++ make pkgconf-pkg-config curl patchelf desktop-file-utils zenity

# Arch
sudo pacman -S --needed base-devel pkgconf curl patchelf desktop-file-utils zenity
```

Install Rust if needed:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

## Build

From the project root:

```sh
bash packaging/build-appimage.sh
```

The finished file is written to:

```text
dist/ComicCrate-<version>-<arch>.AppImage
```

Make it executable and run it:

```sh
chmod +x dist/ComicCrate-*.AppImage
./dist/ComicCrate-*.AppImage
```

The build script downloads `linuxdeploy` and `appimagetool` into
`~/.cache/comiccrate-appimage` on first use. If FUSE is unavailable, they are
run with `APPIMAGE_EXTRACT_AND_RUN=1`, so no system-wide AppImage support is
required for building.

## Notes

- The AppImage is built for the machine architecture it is compiled on.
- The AppImage includes the GUI (`comic-crate`) and the console companion
  (`comic-crate-cli`) under `usr/bin`.
- If you build on a very new distribution, bundle libraries with `patchelf`
  installed so the AppImage is more portable to older distributions.
- File dialogs use `zenity` on Linux. Without it, use drag and drop or the CLI
  companion.
