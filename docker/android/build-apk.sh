#!/usr/bin/env bash
# Runs inside the image built from docker/android/Dockerfile; build-android.sh is the entry point.
set -euo pipefail

pnpm install --frozen-lockfile
pnpm tauri android build --apk --ci

version=$(node -p "require('./src-tauri/tauri.conf.json').version")
unsigned=src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk

mkdir -p /out
if [[ -n ${KEYSTORE_BASE64:-} ]]; then
  keystore=$(mktemp)
  base64 -d <<<"$KEYSTORE_BASE64" >"$keystore"
  apksigner sign \
    --ks "$keystore" \
    --ks-key-alias "$KEY_ALIAS" \
    --ks-pass env:STORE_PASSWORD \
    --key-pass env:KEY_PASSWORD \
    --out "/out/bamboo-mobile-$version.apk" \
    "$unsigned"
else
  cp "$unsigned" "/out/bamboo-mobile-$version-unsigned.apk"
fi
