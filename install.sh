#!/bin/sh
# Build and install glint for the current user (~/.local).
set -eu
cd "$(dirname "$0")"
prefix="${PREFIX:-$HOME/.local}"
# Release tarballs ship the binary next to this script; checkouts build it.
if [ -x ./glint ] && [ ! -f Cargo.toml ]; then
  bin=./glint
else
  cargo build --release
  bin=target/release/glint
fi
install -Dm755 "$bin" "$prefix/bin/glint"
# KWin matches the desktop file's Exec against the caller's executable path.
desktop=data/glint.desktop
[ -f "$desktop" ] || desktop=glint.desktop
sed "s|^Exec=glint|Exec=$prefix/bin/glint|" "$desktop" > /tmp/glint.desktop.$$
install -Dm644 /tmp/glint.desktop.$$ "$prefix/share/applications/glint.desktop"
rm -f /tmp/glint.desktop.$$
echo "installed $prefix/bin/glint"
