#!/usr/bin/env bash
# Fills the package templates in packaging/ for one published release. It downloads the
# release files to compute their SHA-256 hashes, then writes the results to OUT_DIR:
#
#   OUT_DIR/winget/manifests/t/TajbinKhan/Cloudfairy/VERSION/*.yaml
#   OUT_DIR/homebrew/Casks/cloudfairy.rb
#   OUT_DIR/aur/PKGBUILD
#
# Usage: packaging/render.sh VERSION [OUT_DIR]     (VERSION may start with "v")
set -euo pipefail

version="${1:?usage: packaging/render.sh VERSION [OUT_DIR]}"
version="${version#v}"
out="${2:-packaging/out}"
here="$(cd "$(dirname "$0")" && pwd)"
repo="tajbinkhan/cloudfairy"
release="https://github.com/$repo/releases/download/v$version"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

hash_of() {
  echo "Hashing $1" >&2
  curl -fsSL --retry 3 -o "$tmp/download" "$1"
  sha256sum "$tmp/download" | cut -d' ' -f1
}

exe_sha="$(hash_of "$release/Cloudfairy_${version}_x64-setup.exe")"
dmg_sha="$(hash_of "$release/Cloudfairy_${version}_universal.dmg")"
deb_sha="$(hash_of "$release/Cloudfairy_${version}_amd64.deb")"
license_sha="$(hash_of "https://raw.githubusercontent.com/$repo/v$version/LICENSE")"

fill() {
  mkdir -p "$(dirname "$2")"
  sed -e "s/@VERSION@/$version/g" \
      -e "s/@EXE_SHA256@/$exe_sha/g" \
      -e "s/@DMG_SHA256@/$dmg_sha/g" \
      -e "s/@DEB_SHA256@/$deb_sha/g" \
      -e "s/@LICENSE_SHA256@/$license_sha/g" \
      "$1" > "$2"
  echo "Wrote $2" >&2
}

winget="$out/winget/manifests/t/TajbinKhan/Cloudfairy/$version"
for f in "$here"/winget/*.yaml; do
  fill "$f" "$winget/$(basename "$f")"
done
fill "$here/homebrew/cloudfairy.rb" "$out/homebrew/Casks/cloudfairy.rb"
fill "$here/aur/PKGBUILD" "$out/aur/PKGBUILD"
