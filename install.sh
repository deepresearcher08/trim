#!/usr/bin/env bash
set -euo pipefail

REPO="deepresearcher08/trim"
INSTALL_DIR="${HOME}/.local/bin"

mkdir -p "$INSTALL_DIR"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
      aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
      *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
    esac
    ;;
  Darwin)
    case "$ARCH" in
      x86_64) TARGET="x86_64-apple-darwin" ;;
      arm64) TARGET="aarch64-apple-darwin" ;;
      *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
    esac
    ;;
  *)
    echo "Unsupported OS: $OS"; exit 1 ;;
esac

ASSET="trim-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"

echo "Downloading trim (${TARGET})..."
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$URL" -o "${TMP_DIR}/${ASSET}"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "${TMP_DIR}/${ASSET}" "$URL"
else
  echo "Error: curl or wget is required" >&2
  exit 1
fi

tar -xzf "${TMP_DIR}/${ASSET}" -C "$TMP_DIR"
mv "${TMP_DIR}/trim" "${INSTALL_DIR}/trim"
chmod +x "${INSTALL_DIR}/trim"

echo "Successfully installed trim to ${INSTALL_DIR}/trim"
if [[ ":$PATH:" != *":${INSTALL_DIR}:"* ]]; then
  echo "Make sure ${INSTALL_DIR} is in your PATH (e.g., in ~/.bashrc or ~/.zshrc):"
  echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
fi
