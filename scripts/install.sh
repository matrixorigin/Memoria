#!/usr/bin/env sh
# Install memoria binary from GitHub releases.
# Usage:
#   curl -sSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.sh | sh
#   curl -sSL ... | sh -s -- -v v0.1.0-rc1
#   curl -sSL ... | sh -s -- -y              # skip confirmation
#   curl -sSL ... | sh -s -- -d ~/.local/bin  # custom directory
#
# Options:
#   -v, --version TAG   Version to install (default: latest release)
#   -d, --dir DIR       Install directory (default: /usr/local/bin, sudo if needed)
#   -y, --yes           Skip confirmation prompt
#   -n, --dry-run       Print download URL and exit
#
# Env:
#   MEMORIA_REPO        GitHub repo (default: matrixorigin/Memoria)
#   MEMORIA_VERSION     Version tag (default: latest)
#   MEMORIA_GHPROXY     ghproxy base URL (default: https://ghfast.top, auto-detected)

set -eu

# ── Colors ──────────────────────────────────────────────────────────

BOLD="$(tput bold 2>/dev/null || printf '')"
GREEN="$(tput setaf 2 2>/dev/null || printf '')"
YELLOW="$(tput setaf 3 2>/dev/null || printf '')"
BLUE="$(tput setaf 4 2>/dev/null || printf '')"
RED="$(tput setaf 1 2>/dev/null || printf '')"
NC="$(tput sgr0 2>/dev/null || printf '')"

info()  { printf '%s\n' "${BOLD}>${NC} $*"; }
warn()  { printf '%s\n' "${YELLOW}! $*${NC}"; }
error() { printf '%s\n' "${RED}x $*${NC}" >&2; }
ok()    { printf '%s\n' "${GREEN}✓${NC} $*"; }

# ── Banner ──────────────────────────────────────────────────────────

cat << "EOF"

███╗   ███╗███████╗███╗   ███╗ ██████╗ ██████╗ ██╗ █████╗ 
████╗ ████║██╔════╝████╗ ████║██╔═══██╗██╔══██╗██║██╔══██╗
██╔████╔██║█████╗  ██╔████╔██║██║   ██║██████╔╝██║███████║
██║╚██╔╝██║██╔══╝  ██║╚██╔╝██║██║   ██║██╔══██╗██║██╔══██║
██║ ╚═╝ ██║███████╗██║ ╚═╝ ██║╚██████╔╝██║  ██║██║██║  ██║
╚═╝     ╚═╝╚══════╝╚═╝     ╚═╝ ╚═════╝ ╚═╝  ╚═╝╚═╝╚═╝  ╚═╝
            Memoria - Secure · Auditable · Programmable Memory
EOF

# ── Prerequisites ───────────────────────────────────────────────────

if ! command -v curl >/dev/null 2>&1; then
  error "curl is required but not found"
  exit 1
fi

# ── Defaults ────────────────────────────────────────────────────────

REPO="${MEMORIA_REPO:-matrixorigin/Memoria}"
VERSION="${MEMORIA_VERSION:-}"
INSTALL_DIR=""
DRY_RUN=false
FORCE=false

# ── Platform detection ──────────────────────────────────────────────

detect_target() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)
  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) arch="" ;;
  esac
  case "$os" in
    linux)
      [ "$arch" = "x86_64" ] && printf "x86_64-unknown-linux-musl" && return
      [ "$arch" = "aarch64" ] && printf "aarch64-unknown-linux-musl" && return
      ;;
    darwin)
      [ "$arch" = "x86_64" ] && printf "x86_64-apple-darwin" && return
      [ "$arch" = "aarch64" ] && printf "aarch64-apple-darwin" && return
      ;;
  esac
  printf ""
}

# ── Writability test ────────────────────────────────────────────────

test_writeable() {
  path="${1}/._memoria_write_test"
  if touch "${path}" 2>/dev/null; then
    rm -f "${path}"
    return 0
  fi
  return 1
}

# ── Sudo elevation ─────────────────────────────────────────────────

elevate_priv() {
  if ! command -v sudo >/dev/null 2>&1; then
    error "Need write access to ${INSTALL_DIR} but 'sudo' not found"
    info "Either run as root, or use: -d ~/.local/bin"
    exit 1
  fi
  warn "Elevated permissions required to install to ${INSTALL_DIR}"
  if ! sudo -v; then
    error "Superuser not granted, aborting"
    exit 1
  fi
}

# ── PATH detection & shell config hints ─────────────────────────────

check_path() {
  dir="$1"
  case ":${PATH}:" in
    *:"${dir}":*) return 0 ;;
  esac
  return 1
}

print_path_hint() {
  dir="$1"
  printf '\n'
  warn "${dir} is not in your PATH"
  info "Add it by running one of:"
  printf '\n'

  shell_name="$(basename "${SHELL:-sh}")"
  case "$shell_name" in
    zsh)
      info "  ${BLUE}echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.zshrc && source ~/.zshrc${NC}"
      ;;
    bash)
      info "  ${BLUE}echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.bashrc && source ~/.bashrc${NC}"
      ;;
    fish)
      info "  ${BLUE}fish_add_path ${dir}${NC}"
      ;;
    *)
      info "  ${BLUE}echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.bashrc${NC}  (bash)"
      info "  ${BLUE}echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.zshrc${NC}   (zsh)"
      info "  ${BLUE}fish_add_path ${dir}${NC}                              (fish)"
      ;;
  esac
  printf '\n'
  info "Then run ${BLUE}memoria init -i${NC} in your project directory to start the setup wizard"
}

# ── Confirmation ────────────────────────────────────────────────────

confirm() {
  if [ "$FORCE" = true ]; then return 0; fi
  printf "%s " "$* ${BOLD}[y/N]${NC}"
  read -r yn < /dev/tty || return 1
  case "$yn" in
    [Yy]*) return 0 ;;
    *) return 1 ;;
  esac
}

INIT_TOOL=""
INIT_API_URL=""
INIT_TOKEN=""

# ── Parse args ──────────────────────────────────────────────────────

while [ $# -gt 0 ]; do
  case "$1" in
    -v|--version)   VERSION="$2"; shift 2 ;;
    -d|--dir)       INSTALL_DIR="$2"; shift 2 ;;
    -y|--yes)       FORCE=true; shift ;;
    -n|--dry-run)   DRY_RUN=true; shift ;;
    --tool)         INIT_TOOL="$2"; shift 2 ;;
    --api-url)      INIT_API_URL="$2"; shift 2 ;;
    --token)        INIT_TOKEN="$2"; shift 2 ;;
    -h|--help)
      printf "Usage: install.sh [options]\n\n"
      printf "  -v, --version TAG   Version to install (default: latest)\n"
      printf "  -d, --dir DIR       Install directory (default: /usr/local/bin)\n"
      printf "  -y, --yes           Skip confirmation prompt\n"
      printf "  -n, --dry-run       Print download URL and exit\n"
      printf "  --tool TOOL         Auto-init after install (kiro|cursor|claude|codex)\n"
      printf "  --api-url URL       Memoria API URL for auto-init\n"
      printf "  --token TOKEN       Memoria API token for auto-init\n"
      printf "  -h, --help          Show this help\n"
      exit 0
      ;;
    *) shift ;;
  esac
done

# ── Resolve target & URL ────────────────────────────────────────────

TARGET=$(detect_target)
if [ -z "$TARGET" ]; then
  error "Unsupported platform: $(uname -s) $(uname -m)"
  exit 1
fi

TAG="${VERSION:-latest}"
ASSET="memoria-${TARGET}.tar.gz"
if [ "$TAG" = "latest" ]; then
  GH_URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
  GH_SUM_URL="https://github.com/${REPO}/releases/latest/download/SHA256SUMS.txt"
else
  GH_URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"
  GH_SUM_URL="https://github.com/${REPO}/releases/download/${TAG}/SHA256SUMS.txt"
fi
GHPROXY="${MEMORIA_GHPROXY:-https://ghfast.top}"

if [ "$DRY_RUN" = true ]; then
  echo "URL: $GH_URL"
  exit 0
fi

# ── Resolve install directory ───────────────────────────────────────

if [ -z "$INSTALL_DIR" ]; then
  INSTALL_DIR=/usr/local/bin
fi

# ── Check existing installation ─────────────────────────────────────

SKIP_DOWNLOAD=false
# Auto-confirm when all init params are provided
if [ -n "$INIT_TOOL" ] && [ -n "$INIT_API_URL" ] && [ -n "$INIT_TOKEN" ]; then
  FORCE=true
fi
if command -v memoria >/dev/null 2>&1; then
  INSTALLED_VERSION="$(memoria --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
  if [ -n "$INSTALLED_VERSION" ]; then
    if [ "$TAG" = "latest" ] || [ "$INSTALLED_VERSION" = "$TAG" ] || [ "$INSTALLED_VERSION" = "${TAG#v}" ]; then
      ok "memoria v${INSTALLED_VERSION} already installed"
      SKIP_DOWNLOAD=true
      INSTALL_DIR="$(dirname "$(command -v memoria)")"
    fi
  fi
fi

# ── Show plan & confirm ────────────────────────────────────────────

if [ "$SKIP_DOWNLOAD" = false ]; then
  printf '\n'
  info "${BOLD}Version${NC}:   ${GREEN}${TAG}${NC}"
  info "${BOLD}Platform${NC}:  ${GREEN}${TARGET}${NC}"
  info "${BOLD}Directory${NC}: ${GREEN}${INSTALL_DIR}${NC}"
  printf '\n'

  if ! confirm "Install memoria?"; then
    info "Aborted"
    exit 0
  fi
fi

# ── Download ────────────────────────────────────────────────────────

if [ "$SKIP_DOWNLOAD" = false ]; then

# Determine sudo requirement
SUDO=""
if ! test_writeable "$INSTALL_DIR" 2>/dev/null; then
  if [ ! -d "$INSTALL_DIR" ]; then
    if ! mkdir -p "$INSTALL_DIR" 2>/dev/null; then
      elevate_priv
      SUDO="sudo"
      $SUDO mkdir -p "$INSTALL_DIR"
    fi
  else
    elevate_priv
    SUDO="sudo"
  fi
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

info "Downloading ${BLUE}${GH_URL}${NC}"
if ! curl -fL# --max-time 10 -o "$TMP/$ASSET" "$GH_URL" 2>/dev/null; then
  warn "Direct download failed, retrying via proxy: ${GHPROXY}"
  URL="${GHPROXY}/${GH_URL}"
  SUM_URL="${GHPROXY}/${GH_SUM_URL}"
  info "Downloading ${BLUE}${URL}${NC}"
  curl -fL# -o "$TMP/$ASSET" "$URL" || {
    error "Download failed — check that version '${TAG}' exists"
    info "Available releases: ${BLUE}https://github.com/${REPO}/releases${NC}"
    exit 1
  }
else
  URL="$GH_URL"
  SUM_URL="$GH_SUM_URL"
fi

# ── Verify checksum ─────────────────────────────────────────────────

if curl -sSLf -o "$TMP/SHA256SUMS.txt" "$SUM_URL" 2>/dev/null; then
  if (cd "$TMP" && grep -F "$ASSET" SHA256SUMS.txt | (sha256sum -c 2>/dev/null || shasum -a 256 -c 2>/dev/null)); then
    ok "Checksum verified"
  else
    error "Checksum verification failed"
    exit 1
  fi
fi

# ── Install ─────────────────────────────────────────────────────────

tar -xzf "$TMP/$ASSET" -C "$TMP"
$SUDO rm -f "$INSTALL_DIR/memoria"
$SUDO cp "$TMP/memoria" "$INSTALL_DIR/memoria"
$SUDO chmod +x "$INSTALL_DIR/memoria"

printf '\n'
ok "Installed ${GREEN}memoria${NC} to ${GREEN}${INSTALL_DIR}/memoria${NC}"

fi # end SKIP_DOWNLOAD

# ── Auto-init ────────────────────────────────────────────────────────

if [ -n "$INIT_TOOL" ] && [ -n "$INIT_API_URL" ] && [ -n "$INIT_TOKEN" ]; then
  printf '\n'
  info "Running: memoria init --tool ${INIT_TOOL} --api-url ${INIT_API_URL} --token ***"
  "$INSTALL_DIR/memoria" init \
    --tool "$INIT_TOOL" \
    --api-url "$INIT_API_URL" \
    --token "$INIT_TOKEN" \
    --force
elif [ -n "$INIT_TOOL" ]; then
  printf '\n'
  info "Running: memoria init -i --tool ${INIT_TOOL}"
  "$INSTALL_DIR/memoria" init -i --tool "$INIT_TOOL"
fi

# ── PATH check ──────────────────────────────────────────────────────

if ! check_path "$INSTALL_DIR"; then
  print_path_hint "$INSTALL_DIR"
elif [ -z "$INIT_TOOL" ]; then
  printf '\n'
  info "Next: run ${BLUE}memoria init -i${NC} in your project directory to start the setup wizard"
fi
