#!/bin/sh
# Build and install clipt for the current user (~/.local).
set -eu
cd "$(dirname "$0")"
prefix="${PREFIX:-$HOME/.local}"
# Release tarballs ship the binary next to this script; checkouts build it.
if [ -x ./clipt ] && [ ! -f Cargo.toml ]; then
  bin=./clipt
else
  cargo build --release
  bin=target/release/clipt
fi
install -Dm755 "$bin" "$prefix/bin/clipt"
# KWin matches the desktop file's Exec against the caller's executable path.
desktop=data/clipt.desktop
[ -f "$desktop" ] || desktop=clipt.desktop
sed "s|^Exec=clipt|Exec=$prefix/bin/clipt|" "$desktop" > /tmp/clipt.desktop.$$
install -Dm644 /tmp/clipt.desktop.$$ "$prefix/share/applications/clipt.desktop"
rm -f /tmp/clipt.desktop.$$
echo "installed $prefix/bin/clipt"
