#!/bin/sh
set -eu
if [ "$#" -ne 2 ]; then
  echo "usage: dist.sh <version> <target>" >&2
  exit 2
fi
VERSION="$1"
TARGET="$2"
STAGE="dist/gitforest"
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@"
  else
    shasum -a 256 "$@" | sed -E 's/^SHA256 \((.*)\) = ([0-9a-f]+)$/\2  \1/'
  fi
}
rm -rf dist
mkdir -p "$STAGE/bin" "$STAGE/share/gitforest"
cp target/release/gitforest "$STAGE/bin/"
cp target/release/git-remote-gitforest "$STAGE/bin/"
cp target/release/gtfo "$STAGE/bin/" 2>/dev/null || true
cp target/release/gitforest-mcp "$STAGE/bin/" 2>/dev/null || true
cp -r crates/gt-web/static "$STAGE/share/gitforest/" 2>/dev/null || true
head -40 README.md > "$STAGE/README.md"
printf '%s\n' "$VERSION" > "$STAGE/VERSION"
(cd "$STAGE" && sha256 bin/* > SHA256SUMS)
TARBALL="gitforest-$TARGET.tar.gz"
tar czf "$TARBALL" -C dist gitforest
sha256 "$TARBALL" > "$TARBALL.sha256"
