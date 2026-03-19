#!/usr/bin/env sh
# Install the Memoria CLI from GitHub releases and optionally prepare MatrixOne via mo_ctl.
# Usage:
#   curl -sSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.sh | sh
#   curl -sSL ... | sh -s -- --ensure-matrixone
#   curl -sSL ... | sh -s -- --matrixone-only --ensure-matrixone

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

# ── Defaults ────────────────────────────────────────────────────────

REPO="${MEMORIA_REPO:-matrixorigin/Memoria}"
VERSION="${MEMORIA_VERSION:-}"
INSTALL_DIR=""
DRY_RUN=false
FORCE=false

MATRIXONE_MODE="${MATRIXONE_MODE:-check}"
MATRIXONE_ONLY=false
INSTALL_SYSTEM_DEPS=false
MATRIXONE_VERSION="${MATRIXONE_VERSION:-main}"
MATRIXONE_DEPLOY_MODE="${MATRIXONE_DEPLOY_MODE:-docker}"
MATRIXONE_DATA_DIR="${MATRIXONE_DATA_DIR:-$HOME/.local/share/matrixone}"
MATRIXONE_DB_URL="${MEMORIA_DB_URL:-mysql://root:111@127.0.0.1:6001/memoria}"
MOCTL_INSTALL_URL="${MOCTL_INSTALL_URL:-https://raw.githubusercontent.com/matrixorigin/mo_ctl_standalone/main/deploy/local/install.sh}"
MO_PATH="${MO_PATH:-}"

SUDO=""
TMP=""
DB_HOST=""
DB_PORT=""

# ── Generic helpers ────────────────────────────────────────────────

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    error "Missing required command: $1"
    exit 1
  }
}

can_cmd() {
  command -v "$1" >/dev/null 2>&1
}

is_root() {
  [ "$(id -u)" -eq 0 ]
}

run_privileged() {
  if is_root; then
    "$@"
    return
  fi
  if can_cmd sudo; then
    sudo "$@"
    return
  fi
  error "Need elevated privileges to run: $*"
  exit 1
}

confirm() {
  if [ "$FORCE" = true ]; then
    return 0
  fi
  printf "%s " "$* ${BOLD}[y/N]${NC}"
  read -r yn < /dev/tty || return 1
  case "$yn" in
    [Yy]*) return 0 ;;
    *) return 1 ;;
  esac
}

detect_target() {
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"
  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) arch="" ;;
  esac
  case "$os" in
    linux)
      [ "$arch" = "x86_64" ] && printf "x86_64-unknown-linux-gnu" && return
      [ "$arch" = "aarch64" ] && printf "aarch64-unknown-linux-gnu" && return
      ;;
    darwin)
      [ "$arch" = "x86_64" ] && printf "x86_64-apple-darwin" && return
      [ "$arch" = "aarch64" ] && printf "aarch64-apple-darwin" && return
      ;;
  esac
  printf ""
}

test_writeable() {
  path="${1}/._memoria_write_test"
  if touch "${path}" 2>/dev/null; then
    rm -f "${path}"
    return 0
  fi
  return 1
}

elevate_priv() {
  if ! can_cmd sudo; then
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
}

print_next_hint() {
  printf '\n'
  info "Next: run ${BLUE}memoria init -i${NC} in your project directory to start the setup wizard"
}

detect_package_manager() {
  if can_cmd apt-get; then
    printf 'apt'
    return
  fi
  if can_cmd brew; then
    printf 'brew'
    return
  fi
  printf ''
}

install_mysql_client_pkg() {
  pkg_manager="$(detect_package_manager)"
  case "$pkg_manager" in
    apt)
      info "Installing MySQL client with apt"
      run_privileged apt-get update
      run_privileged apt-get install -y default-mysql-client
      ;;
    brew)
      info "Installing MySQL client with Homebrew"
      brew install mysql-client
      ;;
    *)
      return 1
      ;;
  esac
}

install_wget_pkg() {
  pkg_manager="$(detect_package_manager)"
  case "$pkg_manager" in
    apt)
      info "Installing wget with apt"
      run_privileged apt-get update
      run_privileged apt-get install -y wget
      ;;
    brew)
      info "Installing wget with Homebrew"
      brew install wget
      ;;
    *)
      return 1
      ;;
  esac
}

print_mysql_client_hint() {
  pkg_manager="$(detect_package_manager)"
  printf '\n'
  warn "MatrixOne's official mo_ctl flow expects a MySQL client on the machine."
  case "$pkg_manager" in
    apt)
      info "Install it with: ${BLUE}sudo apt-get update && sudo apt-get install -y default-mysql-client${NC}"
      ;;
    brew)
      info "Install it with: ${BLUE}brew install mysql-client${NC}"
      ;;
    *)
      info "Install a MySQL 8.0.30+ client, then rerun this installer."
      ;;
  esac
}

print_wget_hint() {
  pkg_manager="$(detect_package_manager)"
  printf '\n'
  warn "The official mo_ctl installer currently requires wget."
  case "$pkg_manager" in
    apt)
      info "Install it with: ${BLUE}sudo apt-get update && sudo apt-get install -y wget${NC}"
      ;;
    brew)
      info "Install it with: ${BLUE}brew install wget${NC}"
      ;;
    *)
      info "Install wget, then rerun with ${BLUE}--ensure-matrixone${NC}."
      ;;
  esac
}

print_docker_hint() {
  printf '\n'
  warn "Docker is required for the default mo_ctl deployment mode."
  info "Install Docker Desktop / Docker Engine, then rerun with ${BLUE}--ensure-matrixone${NC}"
  info "Or point Memoria to MatrixOne Cloud via ${BLUE}MEMORIA_DB_URL${NC}"
}

print_moctl_hint() {
  printf '\n'
  info "Official MatrixOne quick path (mo_ctl):"
  info "  ${BLUE}curl -fsSL ${MOCTL_INSTALL_URL} | sh${NC}"
  info "  ${BLUE}mo_ctl set_conf MO_DEPLOY_MODE=${MATRIXONE_DEPLOY_MODE}${NC}"
  if [ "${MATRIXONE_DEPLOY_MODE}" = "docker" ]; then
    info "  ${BLUE}mo_ctl set_conf MO_CONTAINER_DATA_HOST_PATH=${MATRIXONE_DATA_DIR}${NC}"
  elif [ -n "${MO_PATH}" ]; then
    info "  ${BLUE}mo_ctl set_conf MO_PATH=${MO_PATH}${NC}"
  fi
  info "  ${BLUE}mo_ctl deploy ${MATRIXONE_VERSION}${NC}"
  info "  ${BLUE}mo_ctl start${NC}"
}

parse_db_host_port() {
  raw="${1#*://}"
  raw="${raw#*@}"
  raw="${raw%%/*}"

  case "$raw" in
    *:*)
      DB_HOST="${raw%%:*}"
      DB_PORT="${raw##*:}"
      ;;
    *)
      DB_HOST="$raw"
      DB_PORT="3306"
      ;;
  esac

  [ -n "${DB_HOST}" ] || DB_HOST="127.0.0.1"
  [ -n "${DB_PORT}" ] || DB_PORT="3306"
}

check_tcp() {
  host="$1"
  port="$2"

  if can_cmd nc; then
    nc -z "$host" "$port" >/dev/null 2>&1
    return $?
  fi

  if can_cmd node; then
    node -e "const net=require('node:net');const socket=net.connect({host:process.argv[1],port:Number(process.argv[2])});socket.setTimeout(1500);const done=(code)=>{socket.destroy();process.exit(code)};socket.on('connect',()=>done(0));socket.on('timeout',()=>done(1));socket.on('error',()=>done(1));" "$host" "$port" >/dev/null 2>&1
    return $?
  fi

  return 2
}

install_moctl() {
  need_cmd curl
  if ! can_cmd wget; then
    if [ "${INSTALL_SYSTEM_DEPS}" = true ]; then
      install_wget_pkg || {
        print_wget_hint
        exit 1
      }
    else
      print_wget_hint
      exit 1
    fi
  fi
  tmp_script="$(mktemp)"
  trap 'rm -f "$tmp_script"' EXIT HUP INT TERM

  info "Installing mo_ctl via the official installer"
  curl -fsSL -o "${tmp_script}" "${MOCTL_INSTALL_URL}" || {
    error "Failed to download mo_ctl installer"
    exit 1
  }
  bash "${tmp_script}"
  rm -f "${tmp_script}"
  trap '[ -n "$TMP" ] && rm -rf "$TMP"' EXIT HUP INT TERM
}

ensure_matrixone_ready() {
  parse_db_host_port "${MATRIXONE_DB_URL}"

  if check_tcp "${DB_HOST}" "${DB_PORT}"; then
    ok "MatrixOne is reachable at ${DB_HOST}:${DB_PORT}"
    return 0
  fi

  if [ "$?" -eq 2 ]; then
    warn "Could not probe ${DB_HOST}:${DB_PORT} because neither nc nor node is available."
  else
    warn "MatrixOne is not reachable at ${DB_HOST}:${DB_PORT}"
  fi

  if [ "${MATRIXONE_MODE}" = "skip" ]; then
    return 0
  fi

  if [ "${MATRIXONE_MODE}" = "check" ]; then
    info "Run this installer again with ${BLUE}--ensure-matrixone${NC} to let it prepare MatrixOne when possible."
    print_moctl_hint
    return 0
  fi

  if ! can_cmd mysql; then
    if [ "${INSTALL_SYSTEM_DEPS}" = true ]; then
      install_mysql_client_pkg || {
        print_mysql_client_hint
        exit 1
      }
    else
      print_mysql_client_hint
      exit 1
    fi
  fi

  if [ "${MATRIXONE_DEPLOY_MODE}" = "docker" ] && ! can_cmd docker; then
    print_docker_hint
    exit 1
  fi

  if ! can_cmd mo_ctl; then
    install_moctl
  fi

  can_cmd mo_ctl || {
    error "mo_ctl is still not in PATH after installation"
    print_moctl_hint
    exit 1
  }

  info "Configuring MatrixOne deployment via mo_ctl"
  mo_ctl set_conf MO_DEPLOY_MODE="${MATRIXONE_DEPLOY_MODE}"
  if [ "${MATRIXONE_DEPLOY_MODE}" = "docker" ]; then
    mkdir -p "${MATRIXONE_DATA_DIR}"
    mo_ctl set_conf MO_CONTAINER_DATA_HOST_PATH="${MATRIXONE_DATA_DIR}"
  elif [ "${MATRIXONE_DEPLOY_MODE}" = "git" ]; then
    if [ -z "${MO_PATH}" ]; then
      error "MATRIXONE_DEPLOY_MODE=git requires MO_PATH to be set"
      exit 1
    fi
    mo_ctl set_conf MO_PATH="${MO_PATH}"
  fi

  info "Deploying MatrixOne ${MATRIXONE_VERSION}"
  mo_ctl deploy "${MATRIXONE_VERSION}"

  info "Starting MatrixOne"
  mo_ctl start

  attempts=0
  while [ "${attempts}" -lt 20 ]; do
    if check_tcp "${DB_HOST}" "${DB_PORT}"; then
      ok "MatrixOne is ready at ${DB_HOST}:${DB_PORT}"
      return 0
    fi
    attempts=$((attempts + 1))
    sleep 3
  done

  warn "MatrixOne start was triggered, but ${DB_HOST}:${DB_PORT} did not become reachable in time."
  if can_cmd mo_ctl; then
    warn "Current mo_ctl status:"
    mo_ctl status || true
  fi
  exit 1
}

usage() {
  printf "Usage: install.sh [options]\n\n"
  printf "Memoria binary options:\n"
  printf "  -v, --version TAG             Version to install (default: latest)\n"
  printf "  -d, --dir DIR                 Install directory (default: /usr/local/bin)\n"
  printf "  -y, --yes                     Skip confirmation prompts\n"
  printf "  -n, --dry-run                 Print download URL and exit\n"
  printf "\nMatrixOne options:\n"
  printf "      --ensure-matrixone        Install or repair MatrixOne via mo_ctl when needed\n"
  printf "      --skip-matrixone          Skip MatrixOne readiness checks entirely\n"
  printf "      --matrixone-only          Only check/install MatrixOne; skip Memoria binary install\n"
  printf "      --matrixone-version REF   MatrixOne version/ref for mo_ctl deploy (default: main)\n"
  printf "      --matrixone-deploy-mode M MatrixOne deploy mode: docker or git (default: docker)\n"
  printf "      --matrixone-data-dir DIR  Host data dir for mo_ctl docker deploy\n"
  printf "      --db-url URL              MatrixOne DSN to verify (default: MEMORIA_DB_URL or local default)\n"
  printf "      --install-system-deps     Install MySQL client automatically when supported\n"
  printf "  -h, --help                    Show this help\n"
}

# ── Prerequisites ───────────────────────────────────────────────────

need_cmd curl

# ── Parse args ──────────────────────────────────────────────────────

while [ $# -gt 0 ]; do
  case "$1" in
    -v|--version) VERSION="$2"; shift 2 ;;
    -d|--dir) INSTALL_DIR="$2"; shift 2 ;;
    -y|--yes) FORCE=true; shift ;;
    -n|--dry-run) DRY_RUN=true; shift ;;
    --ensure-matrixone) MATRIXONE_MODE="ensure"; shift ;;
    --skip-matrixone) MATRIXONE_MODE="skip"; shift ;;
    --matrixone-only) MATRIXONE_ONLY=true; shift ;;
    --matrixone-version) MATRIXONE_VERSION="$2"; shift 2 ;;
    --matrixone-deploy-mode) MATRIXONE_DEPLOY_MODE="$2"; shift 2 ;;
    --matrixone-data-dir) MATRIXONE_DATA_DIR="$2"; shift 2 ;;
    --db-url) MATRIXONE_DB_URL="$2"; shift 2 ;;
    --install-system-deps) INSTALL_SYSTEM_DEPS=true; shift ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      error "Unknown option: $1"
      usage
      exit 1
      ;;
  esac
done

case "${MATRIXONE_MODE}" in
  check|ensure|skip) ;;
  *)
    error "Invalid MATRIXONE_MODE: ${MATRIXONE_MODE}"
    exit 1
    ;;
esac

case "${MATRIXONE_DEPLOY_MODE}" in
  docker|git) ;;
  *)
    error "Invalid MatrixOne deploy mode: ${MATRIXONE_DEPLOY_MODE}"
    exit 1
    ;;
esac

# ── Resolve target & URL ────────────────────────────────────────────

TARGET="$(detect_target)"
if [ -z "$TARGET" ]; then
  error "Unsupported platform: $(uname -s) $(uname -m)"
  exit 1
fi

TAG="${VERSION:-latest}"
ASSET="memoria-${TARGET}.tar.gz"
if [ "$TAG" = "latest" ]; then
  URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
  SUM_URL="https://github.com/${REPO}/releases/latest/download/SHA256SUMS.txt"
else
  URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"
  SUM_URL="https://github.com/${REPO}/releases/download/${TAG}/SHA256SUMS.txt"
fi

if [ "$DRY_RUN" = true ]; then
  echo "URL: $URL"
  exit 0
fi

if [ -z "$INSTALL_DIR" ]; then
  INSTALL_DIR=/usr/local/bin
fi

# ── Show plan & confirm ─────────────────────────────────────────────

printf '\n'
info "${BOLD}Memoria version${NC}:   ${GREEN}${TAG}${NC}"
info "${BOLD}Platform${NC}:          ${GREEN}${TARGET}${NC}"
if [ "${MATRIXONE_ONLY}" = false ]; then
  info "${BOLD}Binary directory${NC}:  ${GREEN}${INSTALL_DIR}${NC}"
fi
info "${BOLD}MatrixOne mode${NC}:    ${GREEN}${MATRIXONE_MODE}${NC}"
info "${BOLD}MatrixOne DB URL${NC}:  ${GREEN}${MATRIXONE_DB_URL}${NC}"
if [ "${MATRIXONE_MODE}" = "ensure" ]; then
  info "${BOLD}MatrixOne deploy${NC}:  ${GREEN}${MATRIXONE_DEPLOY_MODE}${NC}"
  info "${BOLD}MatrixOne version${NC}: ${GREEN}${MATRIXONE_VERSION}${NC}"
fi
printf '\n'

if [ "${MATRIXONE_ONLY}" = true ]; then
  if ! confirm "Check or install MatrixOne now?"; then
    info "Aborted"
    exit 0
  fi
else
  if ! confirm "Install Memoria and continue with MatrixOne readiness checks?"; then
    info "Aborted"
    exit 0
  fi
fi

# ── Install Memoria binary ──────────────────────────────────────────

if [ "${MATRIXONE_ONLY}" = false ]; then
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

  need_cmd tar

  TMP="$(mktemp -d)"
  trap '[ -n "$TMP" ] && rm -rf "$TMP"' EXIT HUP INT TERM

  info "Downloading ${BLUE}${URL}${NC}"
  curl -fL# -o "$TMP/$ASSET" "$URL" || {
    error "Download failed; check that version '${TAG}' exists"
    info "Available releases: ${BLUE}https://github.com/${REPO}/releases${NC}"
    exit 1
  }

  if curl -sSLf -o "$TMP/SHA256SUMS.txt" "$SUM_URL" 2>/dev/null; then
    if (cd "$TMP" && grep -F "$ASSET" SHA256SUMS.txt | (sha256sum -c 2>/dev/null || shasum -a 256 -c 2>/dev/null)); then
      ok "Checksum verified"
    else
      error "Checksum verification failed"
      exit 1
    fi
  fi

  tar -xzf "$TMP/$ASSET" -C "$TMP"
  $SUDO rm -f "$INSTALL_DIR/memoria"
  $SUDO cp "$TMP/memoria" "$INSTALL_DIR/memoria"
  $SUDO chmod +x "$INSTALL_DIR/memoria"

  printf '\n'
  ok "Installed ${GREEN}memoria${NC} to ${GREEN}${INSTALL_DIR}/memoria${NC}"
fi

# ── MatrixOne readiness ─────────────────────────────────────────────

if [ "${MATRIXONE_MODE}" != "skip" ]; then
  printf '\n'
  info "Checking MatrixOne readiness"
  ensure_matrixone_ready
fi

# ── Wrap-up ─────────────────────────────────────────────────────────

if [ "${MATRIXONE_ONLY}" = false ]; then
  if ! check_path "$INSTALL_DIR"; then
    print_path_hint "$INSTALL_DIR"
  fi
  print_next_hint
fi

printf '\n'
ok "Installation flow complete"

if [ "${MATRIXONE_MODE}" != "skip" ]; then
  info "Verified MatrixOne target: ${BLUE}${MATRIXONE_DB_URL}${NC}"
fi
