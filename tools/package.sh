#!/bin/sh
# Pack the server binary and its licence files from a built image into
# dist/magnetite-sync-<version>-linux-<arch>.tar.gz. CI and the release
# workflow use it, so the archive holds exactly what the image ships.
#
# Usage: tools/package.sh <image> <version> <arch>
set -eu

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <image> <version> <arch>" >&2
  exit 2
fi
image=$1
version=$2
arch=$3
name="magnetite-sync-$version-linux-$arch"

rm -rf "dist/$name" "dist/$name.tar.gz"
mkdir -p "dist/$name"
id=$(docker create "$image")
trap 'docker rm "$id" >/dev/null' EXIT
docker cp "$id:/magnetite-sync" "dist/$name/magnetite-sync"
docker cp "$id:/licenses" "dist/$name/licenses"
cp README.md "dist/$name/"
tar -C dist -czf "dist/$name.tar.gz" "$name"
echo "dist/$name.tar.gz"
