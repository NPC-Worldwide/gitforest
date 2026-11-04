#!/bin/bash
set -e

BUCKET="https://storage.googleapis.com/gitforest-executables"


# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

if [ "$OS" = "linux" ]; then
    if [ "$ARCH" = "x86_64" ]; then
        ASSET="gitforest-linux-x64.tar.gz"
        PLATFORM="linux-x64"
    else
        echo "Unsupported architecture: $ARCH"
        exit 1
    fi
elif [ "$OS" = "darwin" ]; then
    if [ "$ARCH" = "arm64" ] || [ "$ARCH" = "aarch64" ]; then
        ASSET="gitforest-macos-arm64.tar.gz"
        PLATFORM="macos-arm64"
    else
        echo "Unsupported architecture: $ARCH"
        exit 1
    fi
else
    echo "Unsupported OS: $OS"
    exit 1
fi

# Read the latest version from the release manifest (used for display only;
# the download URL below is a stable per-platform path).
LATEST=""
URL="$BUCKET/$PLATFORM/$ASSET"
if command -v curl >/dev/null 2>&1; then
    LATEST=$(curl -fsSL "$BUCKET/manifest.json" 2>/dev/null | grep '"version"' | head -1 | sed -E 's/.*"([^"]+)".*/\1/' || true)
fi
if [ -z "$LATEST" ]; then
    LATEST="latest"
fi

echo "Installing gitforest $LATEST for $OS/$ARCH..."
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL "$URL" -o "$TMPDIR/$ASSET"

tar xzf "$TMPDIR/$ASSET" -C "$TMPDIR"

# Install binaries
mkdir -p "$HOME/.local/bin"
cp "$TMPDIR/gitforest/bin/gitforest" "$HOME/.local/bin/"
cp "$TMPDIR/gitforest/bin/git-remote-gitforest" "$HOME/.local/bin/"
if [ -f "$TMPDIR/gitforest/bin/gtfo" ]; then
    cp "$TMPDIR/gitforest/bin/gtfo" "$HOME/.local/bin/"
fi
if [ -f "$TMPDIR/gitforest/bin/gitforest-mcp" ]; then
    cp "$TMPDIR/gitforest/bin/gitforest-mcp" "$HOME/.local/bin/"
fi

# Link the desktop app onto PATH when it is installed outside a package
# (the .deb handles this via its after-install script).
if [ -x /opt/gitforest/gitforest-desktop ]; then
    ln -sf /opt/gitforest/gitforest-desktop "$HOME/.local/bin/gitforest-desktop"
    echo "Linked gitforest-desktop onto PATH."
fi

# Ensure PATH contains ~/.local/bin
if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    echo "export PATH=\"\$HOME/.local/bin:\$PATH\"" >> "$HOME/.bashrc"
    echo "Added ~/.local/bin to PATH in .bashrc. Run 'source ~/.bashrc' or restart your shell."
fi

echo "gitforest $LATEST installed."

echo "Run: gitforest --help"
echo "Alias: gtfo --help"
