#!/bin/sh
set -eu
if [ "$#" -ne 2 ]; then
  echo "usage: dist.sh <version> <target>" >&2
  exit 2
fi
VERSION="$1"
TARGET="$2"
STAGE="dist/gitforest"
case "$TARGET" in
windows-*) EXT=".exe"; ARCHIVE="zip" ;;
*) EXT=""; ARCHIVE="tar.gz" ;;
esac
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@"
  else
    shasum -a 256 "$@" | sed -E 's/^SHA256 \((.*)\) = ([0-9a-f]+)$/\2  \1/'
  fi
}
rm -rf dist
mkdir -p "$STAGE/bin" "$STAGE/share/gitforest"
cp "target/release/gitforest$EXT" "$STAGE/bin/"
cp "target/release/git-remote-gitforest$EXT" "$STAGE/bin/"
cp "target/release/gtfo$EXT" "$STAGE/bin/" 2>/dev/null || true
cp "target/release/gitforest-mcp$EXT" "$STAGE/bin/" 2>/dev/null || true
cp -r crates/gt-web/static "$STAGE/share/gitforest/" 2>/dev/null || true
head -40 README.md > "$STAGE/README.md"
printf '%s\n' "$VERSION" > "$STAGE/VERSION"
(cd "$STAGE" && sha256 bin/* > SHA256SUMS)
if [ "$ARCHIVE" = "zip" ]; then
  ARCHIVE_FILE="gitforest-$TARGET.zip"
  if command -v python3 >/dev/null 2>&1; then PY=python3; else PY=python; fi
  "$PY" - "$ARCHIVE_FILE" <<'EOF'
import sys
import zipfile
import os
out = sys.argv[1]
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
    for root, _, files in os.walk("dist/gitforest"):
        for f in files:
            p = os.path.join(root, f)
            z.write(p, os.path.relpath(p, "dist"))
EOF
else
  ARCHIVE_FILE="gitforest-$TARGET.tar.gz"
  tar czf "$ARCHIVE_FILE" -C dist gitforest
fi
sha256 "$ARCHIVE_FILE" > "$ARCHIVE_FILE.sha256"
