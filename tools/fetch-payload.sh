#!/usr/bin/env bash
# Fetch the official TempleOS V5.03 ISO into payload/ and verify it against
# payload/TempleOS.ISO.pin (size + MD5, and SHA-256 once recorded).
#
# On the first successful fetch the SHA-256 is written into the pin file;
# commit that change. After that, any mismatch is a hard failure.
#
# Usage: tools/fetch-payload.sh [URL-or-local-path ...]
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
pin="$root/payload/TempleOS.ISO.pin"
dest="$root/payload/TempleOS.ISO"

get() { sed -n "s/^$1=//p" "$pin"; }
want_size="$(get size)"
want_md5="$(get md5)"
want_sha="$(get sha256)"

sources=("$@")
if [ ${#sources[@]} -eq 0 ]; then
  sources=(
    "https://templeos.org/Downloads/TempleOS.ISO"
    "https://archive.org/download/TempleOS_ISO_Archive/TempleOS.ISO"
  )
fi

verify() {
  local f="$1" size md5 sha
  size="$(stat -c %s "$f")"
  md5="$(md5sum "$f" | cut -d' ' -f1)"
  sha="$(sha256sum "$f" | cut -d' ' -f1)"
  if [ "$size" != "$want_size" ]; then
    echo "  size $size != $want_size"; return 1
  fi
  if [ "$md5" != "$want_md5" ]; then
    echo "  md5 $md5 != $want_md5"; return 1
  fi
  if [ -n "$want_sha" ] && [ "$sha" != "$want_sha" ]; then
    echo "  sha256 $sha != $want_sha"; return 1
  fi
  echo "$sha"
}

if [ -f "$dest" ] && out="$(verify "$dest")"; then
  echo "payload/TempleOS.ISO already present and verified (sha256 ${out##*$'\n'})"
  exit 0
fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
for src in "${sources[@]}"; do
  echo "trying $src"
  if [ -f "$src" ]; then
    cp "$src" "$tmp"
  elif ! curl -fL --retry 3 -o "$tmp" "$src"; then
    echo "  download failed"; continue
  fi
  if out="$(verify "$tmp")"; then
    sha="${out##*$'\n'}"
    mv "$tmp" "$dest"
    trap - EXIT
    if [ -z "$want_sha" ]; then
      sed -i "s/^sha256=.*/sha256=$sha/" "$pin"
      echo "recorded sha256=$sha in payload/TempleOS.ISO.pin; commit it"
    fi
    echo "payload/TempleOS.ISO verified"
    exit 0
  fi
  echo "$out"
done
echo "no source produced a matching TempleOS.ISO" >&2
exit 1
