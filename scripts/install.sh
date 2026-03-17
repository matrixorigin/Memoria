#!/usr/bin/env bash
# Install memoria binary from GitHub releases.
# Usage:
#   curl -sSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.sh | bash
#   curl -sSL ... | bash -s -- -v v0.1.0-rc1
#   MEMORIA_VERSION=v0.1.0-rc1 curl -sSL ... | bash
#
# Options:
#   -v, --version TAG   Version to install (default: latest release)
#   -d, --dir DIR       Install binary to DIR (default: /usr/local/bin or ~/.local/bin)
#   -n, --dry-run       Print download URL and exit
#
# Env:
#   MEMORIA_REPO        GitHub repo (default: matrixorigin/Memoria)
#   MEMORIA_VERSION     Version tag (default: latest)

set -e

cat << "EOF"

███╗   ███╗███████╗███╗   ███╗ ██████╗ ██████╗ ██╗ █████╗ 
████╗ ████║██╔════╝████╗ ████║██╔═══██╗██╔══██╗██║██╔══██╗
██╔████╔██║█████╗  ██╔████╔██║██║   ██║██████╔╝██║███████║
██║╚██╔╝██║██╔══╝  ██║╚██╔╝██║██║   ██║██╔══██╗██║██╔══██║
██║ ╚═╝ ██║███████╗██║ ╚═╝ ██║╚██████╔╝██║  ██║██║██║  ██║
╚═╝     ╚═╝╚══════╝╚═╝     ╚═╝ ╚═════╝ ╚═╝  ╚═╝╚═╝╚═╝  ╚═╝
            Memoria - Secure · Auditable · Programmable Memory
EOF

if ! command -v curl >/dev/null 2>&1; then
  echo "error: curl is required" >&2
  exit 1
fi

REPO="${MEMORIA_REPO:-matrixorigin/Memoria}"
VERSION="${MEMORIA_VERSION:-}"
INSTALL_DIR=""
DRY_RUN=false

# Map (os, arch) -> Rust target triple used in release artifacts
get_target() {
  local os arch
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)
  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) arch="" ;;
  esac
  case "$os" in
    linux)
      [[ "$arch" == "x86_64" ]] && echo "x86_64-unknown-linux-gnu" && return
      [[ "$arch" == "aarch64" ]] && echo "aarch64-unknown-linux-gnu" && return
      ;;
    darwin)
      [[ "$arch" == "x86_64" ]] && echo "x86_64-apple-darwin" && return
      [[ "$arch" == "aarch64" ]] && echo "aarch64-apple-darwin" && return
      ;;
  esac
  echo ""
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -v|--version) VERSION="$2"; shift 2 ;;
    -d|--dir)     INSTALL_DIR="$2"; shift 2 ;;
    -n|--dry-run) DRY_RUN=true; shift ;;
    *) shift ;;
  esac
done

TARGET=$(get_target)
if [[ -z "$TARGET" ]]; then
  echo "error: unsupported platform $(uname -s) $(uname -m)" >&2
  exit 1
fi

# Use "latest" so GitHub redirects to the latest release; or a specific tag
TAG="${VERSION:-latest}"
ASSET="memoria-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"

if $DRY_RUN; then
  echo "URL: $URL"
  exit 0
fi

if [[ -z "$INSTALL_DIR" ]]; then
  if [[ -w /usr/local/bin ]]; then
    INSTALL_DIR=/usr/local/bin
  else
    INSTALL_DIR="${HOME}/.local/bin"
    mkdir -p "$INSTALL_DIR"
  fi
fi

echo "Installing memoria ${TAG} (${TARGET}) to ${INSTALL_DIR}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
echo "Downloading: $URL"
curl -fL# -o "$TMP/$ASSET" "$URL"
# Verify checksum if SHA256SUMS.txt exists for this release
SUM_URL="https://github.com/${REPO}/releases/download/${TAG}/SHA256SUMS.txt"
if curl -sSLf -o "$TMP/SHA256SUMS.txt" "$SUM_URL" 2>/dev/null; then
  (cd "$TMP" && grep -F "$ASSET" SHA256SUMS.txt | (sha256sum -c 2>/dev/null || shasum -a 256 -c 2>/dev/null)) || { echo "error: checksum verification failed" >&2; exit 1; }
fi
tar -xzf "$TMP/$ASSET" -C "$TMP"
mkdir -p "$INSTALL_DIR"
cp "$TMP/memoria" "$INSTALL_DIR/memoria"
chmod +x "$INSTALL_DIR/memoria"
echo "Installed: $INSTALL_DIR/memoria"
if ! command -v memoria >/dev/null 2>&1; then
  echo "Note: ensure ${INSTALL_DIR} is in your PATH"
fi
