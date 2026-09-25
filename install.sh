#!/bin/sh
# oriel installer for Linux (Omarchy/Arch, Ubuntu, Fedora, ...) and a pointer for Windows.
#   curl -fsSL https://raw.githubusercontent.com/gorpnostic/z4-oriel/master/install.sh | sh
# Re-running it (or `oriel update`) updates to the latest release.
set -eu

REPO="gorpnostic/z4-oriel"
BIN_DIR="${ORIEL_BIN_DIR:-$HOME/.local/bin}"

say() { printf '\033[1;33m::\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) ;;
  MINGW*|MSYS*|CYGWIN*) die "on Windows run this in PowerShell instead:  irm https://raw.githubusercontent.com/$REPO/master/install.ps1 | iex" ;;
  *) die "no prebuilt oriel for $os yet — build it from source: cargo install --git https://github.com/$REPO" ;;
esac

if [ "$arch" != "x86_64" ]; then
  say "no prebuilt binary for $arch; building from source (needs Rust)"
  command -v cargo >/dev/null 2>&1 || die "install Rust first (https://rustup.rs), then re-run"
  cargo install --git "https://github.com/$REPO" --locked --root "$HOME/.local"
  say "installed to $HOME/.local/bin/oriel"
  exit 0
fi

url="https://github.com/$REPO/releases/latest/download/oriel-linux-x86_64.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "downloading oriel"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$url" -o "$tmp/oriel.tar.gz" || die "download failed: $url"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$tmp/oriel.tar.gz" "$url" || die "download failed: $url"
else
  die "needs curl or wget"
fi
tar -xzf "$tmp/oriel.tar.gz" -C "$tmp"
mkdir -p "$BIN_DIR"
# replace atomically so a running oriel keeps working until it exits
install -m 755 "$tmp/oriel" "$BIN_DIR/oriel.new"
mv -f "$BIN_DIR/oriel.new" "$BIN_DIR/oriel"

# music playback needs the ALSA library (every desktop distro has it; minimal installs may not)
if ! ldconfig -p 2>/dev/null | grep -q libasound.so.2; then
  say "note: music playback needs libasound (Arch: sudo pacman -S alsa-lib · Ubuntu: sudo apt install libasound2)"
fi

version="$("$BIN_DIR/oriel" --version 2>/dev/null || echo oriel)"
say "installed $version to $BIN_DIR/oriel"
case ":$PATH:" in
  *":$BIN_DIR:"*) say "run it with: oriel" ;;
  *) say "add $BIN_DIR to your PATH, e.g.:  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc" ;;
esac
say "a Nerd Font makes the icons show (Omarchy has one already)"
