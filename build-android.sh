#!/usr/bin/env bash
# Builds a release APK inside Docker into the project root. Signs it when
# KEYSTORE_BASE64, KEY_ALIAS, KEY_PASSWORD and STORE_PASSWORD are set, in the
# environment or in .env.android (see .env.android.example).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

image=bamboo-mobile-lan-android-build
signing_vars=(KEYSTORE_BASE64 KEY_ALIAS KEY_PASSWORD STORE_PASSWORD)

if [[ -f .env.android ]]; then
  while IFS= read -r line || [[ -n $line ]]; do
    [[ $line =~ ^[[:space:]]*(#|$) ]] && continue
    key=${line%%=*}
    if [[ -z ${!key+x} ]]; then
      export "$line"
    fi
  done <.env.android
fi

docker build -t "$image" docker/android

container=$(docker create -i \
  -v bamboo-mobile-lan-android-cargo:/usr/local/cargo/registry \
  -v bamboo-mobile-lan-android-target:/build/src-tauri/target \
  -v bamboo-mobile-lan-android-gradle:/root/.gradle \
  -v bamboo-mobile-lan-android-pnpm:/root/.local/share/pnpm \
  "${signing_vars[@]/#/--env=}" \
  "$image" \
  bash -c 'tar -x --no-same-owner && docker/android/build-apk.sh')
trap 'docker rm -f "$container" >/dev/null' EXIT

# Ship tracked and untracked-but-not-ignored files, as a fresh checkout would have them.
git ls-files -z -co --exclude-standard \
  | tar --null --ignore-failed-read -T - -cf - \
  | docker start -ai "$container"

docker cp "$container:/out/." .
