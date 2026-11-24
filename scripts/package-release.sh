#!/bin/bash
set -euo pipefail
umask 077
export COPYFILE_DISABLE=1

if [[ $(uname -s) != Darwin || $(uname -m) != arm64 ]]; then
  echo "Package Know Things on an Apple Silicon Mac." >&2
  exit 1
fi

repo=$(cd "$(dirname "$0")/.." && pwd)
releases="$repo/.local/releases"
mkdir -p "$releases"
scratch=$(mktemp -d "$releases/.build.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
docker_context=${KB_DOCKER_CONTEXT:-desktop-linux}

# Build both binaries from the same copy of the intended release files.
cd "$repo"
git ls-files --cached --others --exclude-standard -z -- \
  .dockerignore .gitignore AGENTS.md CLAUDE.md Cargo.toml Cargo.lock Dockerfile \
  README.md compose.yaml config docs scripts src tests > "$scratch/source-files"
tar -cf "$scratch/source.tar" --null -T "$scratch/source-files"
mkdir "$scratch/source"
tar -xf "$scratch/source.tar" -C "$scratch/source"
cd "$scratch/source"

version=$(cargo metadata --locked --no-deps --format-version 1 | jq -er \
  '.packages[] | select(.name == "know-things") | .version')
name="know-things-$version-apple-silicon"
image="know-things:$version-${scratch##*.}"
bundle="$scratch/$name"
(
  find . -type f -exec shasum -a 256 {} + | LC_ALL=C sort -k2
) > "$scratch/SOURCE-SHA256SUMS"
mkdir -p "$bundle/bin"

KNOW_THINGS_BUILD_VERSION="$version" cargo build --locked --release \
  --target aarch64-apple-darwin --target-dir "$scratch/target"
install -m 755 "$scratch/target/aarch64-apple-darwin/release/know-things" "$bundle/bin/know-things"
test "$("$bundle/bin/know-things" --version)" = "know-things $version"

docker --context "$docker_context" build --platform linux/arm64 \
  --build-arg "KB_BUILD_VERSION=$version" --tag "$image" .
platform=$(docker --context "$docker_context" image inspect "$image" \
  --format '{{.Os}}/{{.Architecture}}')
test "$platform" = "linux/arm64"
docker --context "$docker_context" image save --output "$bundle/image.tar" "$image"
printf '%s\n' "$image" > "$bundle/IMAGE"

tar -xf "$scratch/source.tar" -C "$bundle"
mv "$scratch/SOURCE-SHA256SUMS" "$bundle/"
cd "$bundle"
shasum -a 256 -c SOURCE-SHA256SUMS >/dev/null

mkdir "$scratch/artifacts"
cd "$scratch"
archive="$name.tar.gz"
tar -czf "artifacts/$archive" "$name"
cd artifacts
shasum -a 256 "$archive" > "$archive.sha256"
shasum -a 256 -c "$archive.sha256"
cd "$releases"
output=$(mktemp -d "$releases/$name.XXXXXX")
mv "$scratch/artifacts/$archive" "$scratch/artifacts/$archive.sha256" "$output/"
printf 'Release files are in %s\n' "$output"
