#!/bin/bash
# Sign macOS binaries and app bundles with the Developer ID and have Apple
# notarise them, in one submission.
# Usage: macos-sign.sh [--wait WAIT] PATH..., WAIT being how long to wait for
# Apple (notarytool's --timeout, default 10m). An accepted app bundle gets
# the ticket stapled, so it opens offline the first time too. Reads the
# MACOS_* secrets from the environment; docs/macos-plan.md lists them.
set -euo pipefail
wait=10m
if [ "${1:-}" = --wait ]; then
  wait=$2
  shift 2
fi
paths=("$@")

# A throwaway keychain holding only the certificate.
kc="$RUNNER_TEMP/signing.keychain-db"
kcpw=$(openssl rand -hex 16)
security create-keychain -p "$kcpw" "$kc"
trap 'security delete-keychain "$kc"; rm -f "$RUNNER_TEMP/notary.p8"' EXIT
security set-keychain-settings -lut 1800 "$kc"
security unlock-keychain -p "$kcpw" "$kc"
printf '%s' "$MACOS_CERT_P12" | base64 --decode > "$RUNNER_TEMP/cert.p12"
security import "$RUNNER_TEMP/cert.p12" -k "$kc" -P "$MACOS_CERT_PASSWORD" -T /usr/bin/codesign
rm "$RUNNER_TEMP/cert.p12"
security set-key-partition-list -S apple-tool:,apple: -s -k "$kcpw" "$kc" > /dev/null
security list-keychains -d user -s "$kc"
# Notarisation needs the hardened runtime and a secure timestamp. The
# runtime refuses the microphone unless the entitlements allow it; signing
# the app gives its executable the same ones.
entitlements="$(dirname "$0")/../../packaging/macos/clipt.entitlements"
for p in "${paths[@]}"; do
  codesign --force --options runtime --timestamp --entitlements "$entitlements" --keychain "$kc" --sign "Developer ID Application" "$p"
  codesign --verify --strict --verbose=2 "$p"
done

printf '%s\n' "$MACOS_NOTARY_KEY" > "$RUNNER_TEMP/notary.p8"
mkdir "$RUNNER_TEMP/notarise"
for p in "${paths[@]}"; do ditto "$p" "$RUNNER_TEMP/notarise/$(basename "$p")"; done
ditto -c -k "$RUNNER_TEMP/notarise" "$RUNNER_TEMP/clipt.zip"
rm -rf "$RUNNER_TEMP/notarise"
# Apple's queue can take far longer than a job should wait, but a bare
# binary's notarisation is looked up online when it first runs, so the
# build counts as notarised once Apple accepts it. (A bundle's stapled
# ticket only comes with an accepted submission.) Wait a while, then hand
# it over with the submission's id; the notary workflow can check it later.
auth=(--key "$RUNNER_TEMP/notary.p8" --key-id "$MACOS_NOTARY_KEY_ID" --issuer "$MACOS_NOTARY_ISSUER")
xcrun notarytool submit "$RUNNER_TEMP/clipt.zip" "${auth[@]}" --output-format json > "$RUNNER_TEMP/notary.json"
id=$(plutil -extract id raw "$RUNNER_TEMP/notary.json")
echo "notarisation submission: $id"
xcrun notarytool wait "$id" "${auth[@]}" --timeout "$wait" || true
xcrun notarytool info "$id" "${auth[@]}" --output-format json > "$RUNNER_TEMP/notary.json"
status=$(plutil -extract status raw "$RUNNER_TEMP/notary.json")
echo "notarisation status: $status"
case "$status" in
  Accepted)
    for p in "${paths[@]}"; do
      if [[ $p == *.app ]]; then xcrun stapler staple "$p"; fi
    done
    ;;
  "In Progress") echo "::warning::Apple is still notarising submission $id; the build will pass Gatekeeper once it's accepted" ;;
  *)
    xcrun notarytool log "$id" "${auth[@]}"
    exit 1
    ;;
esac
