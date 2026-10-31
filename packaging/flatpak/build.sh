#!/bin/sh
# Build the Flatpak and install it for the current user.
#   packaging/flatpak/build.sh           build + install
#   packaging/flatpak/build.sh --bundle  also write clipt.flatpak
# Needs flatpak-builder: `flatpak install --user flathub org.flatpak.Builder`
# (used automatically if the native one isn't installed) and the SDKs:
#   flatpak install --user flathub org.freedesktop.Sdk//25.08 \
#     org.freedesktop.Sdk.Extension.rust-stable//25.08
set -e
cd "$(dirname "$0")"
if command -v flatpak-builder >/dev/null; then fb=flatpak-builder; else fb="flatpak run org.flatpak.Builder"; fi
$fb --user --install --force-clean --repo=repo --state-dir=.flatpak-builder build io.github.jasondyck.clipt.yml
if [ "${1:-}" = --bundle ]; then
  flatpak build-bundle repo clipt.flatpak io.github.jasondyck.clipt
  echo "wrote $(pwd)/clipt.flatpak"
fi
