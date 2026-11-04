#!/bin/sh
# Pack a release build of clipt.exe as clipt-TAG-x86_64-windows.zip, with a
# .sha256 beside it. The release workflow runs this, and CI's windows job
# runs it too, so a broken step shows up before a release is tagged.
#   .github/scripts/pack-windows.sh v0.7.0
set -eu
name="clipt-$1-x86_64-windows"
mkdir -p "$name"
cp target/release/clipt.exe README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE "$name/"
"$name/clipt.exe" --version
7z a "$name.zip" "$name" >/dev/null
sha256sum "$name.zip" > "$name.zip.sha256"
