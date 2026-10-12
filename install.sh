#!/bin/sh
# Ogma installation script
set -e

REPO="john--kane/ogma"
GITHUB_URL="https://github.com/$REPO"

# Detect OS
OS="$(uname -s)"
case "$OS" in
    Linux)
        OGMA_OS="ubuntu-24.04"
        ;;
    Darwin)
        # Check if it's Apple Silicon or Intel
        ARCH="$(uname -m)"
        if [ "$ARCH" = "arm64" ]; then
            OGMA_OS="macos-14"
        else
            OGMA_OS="macos-15-intel"
        fi
        ;;
    *)
        echo "Error: Unsupported OS $OS"
        exit 1
        ;;
esac

# For now, we'll try to get the latest release tag.
# If no releases yet, this might need adjustment to point to a specific known version or artifacts.
# The issue mentions "github build output", which usually refers to Releases.

# Detect version
echo "Detecting version..."
# If there's a specific tag passed as an argument, use it. Otherwise try to find latest.
if [ -n "$1" ]; then
    VERSION="$1"
else
    # Try to get latest release tag
    VERSION=$(curl -s https://api.github.com/repos/$REPO/releases/latest | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
fi

if [ -z "$VERSION" ]; then
    echo "Could not detect latest release version."
    echo "Usage: curl -fsSL https://.../install.sh | bash -s -- <version>"
    exit 1
fi

RELEASE_NAME="ogma-${OGMA_OS}-preview"
FILENAME="${RELEASE_NAME}.tar.gz"
DOWNLOAD_URL="${GITHUB_URL}/releases/download/${VERSION}/${FILENAME}"

INSTALL_DIR="${HOME}/.ogma"
BIN_DIR="${INSTALL_DIR}/bin"

echo "Installing Ogma ${VERSION} to ${INSTALL_DIR}..."

mkdir -p "${INSTALL_DIR}"
# We'll download to a temporary file
TEMP_FILE=$(mktemp)

echo "Downloading ${DOWNLOAD_URL}..."
curl -fL "${DOWNLOAD_URL}" -o "${TEMP_FILE}"

echo "Extracting..."
# The archive contains a directory named ${RELEASE_NAME}
tar -xzf "${TEMP_FILE}" -C "${INSTALL_DIR}"

# Remove old bin if exists and link the new one
rm -rf "${BIN_DIR}"
mkdir -p "${BIN_DIR}"
ln -s "${INSTALL_DIR}/${RELEASE_NAME}/ogma" "${BIN_DIR}/ogma"

# Also link the worker if it's needed in the same PATH, 
# although usually it's called by the main binary.
# Based on check_layout in verify_preview.py, both are in the root of the package.
ln -s "${INSTALL_DIR}/${RELEASE_NAME}/ogma-worker" "${BIN_DIR}/ogma-worker"

rm "${TEMP_FILE}"

echo "Ogma installed successfully!"
echo ""
echo "Please add the following to your shell profile (e.g., ~/.bashrc or ~/.zshrc):"
echo "  export PATH=\"${BIN_DIR}:\$PATH\""
echo ""
echo "Then restart your terminal or run 'source <your-profile-file>'."
