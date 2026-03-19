#!/usr/bin/env bash
set -euo pipefail

PLUGIN_ID="memory-memoria"
DEFAULT_REPO_URL="https://github.com/matrixorigin/openclaw-memoria.git"
DEFAULT_REPO_REF="main"
DEFAULT_MEMORIA_VERSION="v0.1.0"

MEMORIA_TOOL_NAMES=(
  memory_search
  memory_get
  memory_store
  memory_retrieve
  memory_recall
  memory_list
  memory_stats
  memory_profile
  memory_correct
  memory_purge
  memory_forget
  memory_health
  memory_observe
  memory_governance
  memory_consolidate
  memory_reflect
  memory_extract_entities
  memory_link_entities
  memory_rebuild_index
  memory_capabilities
  memory_snapshot
  memory_snapshots
  memory_rollback
  memory_branch
  memory_branches
  memory_checkout
  memory_branch_delete
  memory_merge
  memory_diff
)

log() {
  printf '[memory-memoria] %s\n' "$*"
}

fail() {
  printf '[memory-memoria] error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "Missing required command: $1"
}

validate_openclaw_bin() {
  local candidate="$1"
  if ! "${candidate}" --version >/dev/null 2>&1; then
    return 1
  fi
  return 0
}

resolve_openclaw_bin() {
  local candidate="${1:-openclaw}"
  local resolved=''

  if [[ "${candidate}" == */* ]]; then
    [[ -x "${candidate}" ]] || fail "OPENCLAW_BIN is not executable: ${candidate}"
    printf '%s' "${candidate}"
    return 0
  fi

  resolved="$(command -v "${candidate}" 2>/dev/null || true)"
  if [[ -n "${resolved}" ]]; then
    printf '%s' "${resolved}"
    return 0
  fi

  for fallback in \
    "${HOME}/Library/pnpm/openclaw" \
    "${HOME}/.local/share/pnpm/openclaw" \
    "${HOME}/.pnpm/openclaw"
  do
    if [[ -x "${fallback}" ]]; then
      printf '%s' "${fallback}"
      return 0
    fi
  done

  if command -v pnpm >/dev/null 2>&1; then
    resolved="$(pnpm bin -g 2>/dev/null || true)"
    if [[ -n "${resolved}" && -x "${resolved}/openclaw" ]]; then
      printf '%s' "${resolved}/openclaw"
      return 0
    fi
  fi

  fail "Missing required command: openclaw. Set OPENCLAW_BIN=/absolute/path/to/openclaw"
}

usage() {
  cat <<'EOF'
Install the OpenClaw Memoria plugin using the Rust Memoria CLI runtime.

Usage:
  bash scripts/install-openclaw-memoria.sh [options]
  curl -fsSL <raw-script-url> | env MEMORIA_EMBEDDING_API_KEY=... bash -s --

Options:
  --source-dir <path>           Use an existing checkout instead of cloning.
  --install-dir <path>          Clone target when --source-dir is not provided.
  --repo-url <url>              Git repo to clone when no local checkout is used.
  --ref <ref>                   Git branch, tag, or ref to clone. Default: main.
  --openclaw-bin <path|command> Use an existing openclaw executable.
  --memoria-bin <path|command>  Use an existing memoria executable.
  --memoria-version <tag>       Rust Memoria release tag. Default: v0.1.0.
  --memoria-install-dir <path>  Where to install memoria if it is missing.
  --skip-memoria-install        Require an existing memoria executable.
  --skip-plugin-install         Assume the plugin is already installed/enabled in OpenClaw.
  --verify                      Run verify_plugin_install.mjs after installation.
  --help                        Show this help text.

Environment overrides:
  OPENCLAW_BIN                    Default: auto-detected openclaw executable
  OPENCLAW_HOME                   Optional target OpenClaw home.
  MEMORIA_DB_URL                  Default: mysql://root:111@127.0.0.1:6001/memoria
  MEMORIA_DEFAULT_USER_ID         Default: openclaw-user
  MEMORIA_USER_ID_STRATEGY        Default: config
  MEMORIA_AUTO_RECALL             Default: true
  MEMORIA_AUTO_OBSERVE            Default: false
  MEMORIA_EXECUTABLE              Alias for --memoria-bin
  MEMORIA_RELEASE_TAG             Alias for --memoria-version
  MEMORIA_BINARY_INSTALL_DIR      Alias for --memoria-install-dir
  MEMORIA_EMBEDDING_PROVIDER      Default: openai
  MEMORIA_EMBEDDING_MODEL         Default: text-embedding-3-small
  MEMORIA_EMBEDDING_BASE_URL      Optional for official OpenAI; required for compatible gateways
  MEMORIA_EMBEDDING_API_KEY       Required unless provider=local
  MEMORIA_EMBEDDING_DIM           Auto-filled for common models; otherwise required
  MEMORIA_LLM_BASE_URL            Optional OpenAI-compatible base URL
  MEMORIA_LLM_API_KEY             Optional; required if autoObserve=true
  MEMORIA_LLM_MODEL               Optional; required if autoObserve=true
EOF
}

normalize_bool() {
  local raw="${1:-}"
  case "${raw,,}" in
    1|true|yes|on)
      printf 'true'
      ;;
    0|false|no|off)
      printf 'false'
      ;;
    *)
      fail "Expected boolean value, got: ${raw}"
      ;;
  esac
}

infer_embedding_dim() {
  local model="${1:-}"
  case "${model}" in
    text-embedding-3-small|openai/text-embedding-3-small)
      printf '1536'
      ;;
    text-embedding-3-large|openai/text-embedding-3-large)
      printf '3072'
      ;;
    text-embedding-ada-002|openai/text-embedding-ada-002)
      printf '1536'
      ;;
    all-MiniLM-L6-v2|sentence-transformers/all-MiniLM-L6-v2)
      printf '384'
      ;;
    BAAI/bge-m3)
      printf '1024'
      ;;
    *)
      printf ''
      ;;
  esac
}

normalize_base_url() {
  local url="${1:-}"
  url="${url%/}"
  case "${url}" in
    */embeddings)
      url="${url%/embeddings}"
      ;;
    */chat/completions)
      url="${url%/chat/completions}"
      ;;
    */completions)
      url="${url%/completions}"
      ;;
  esac
  printf '%s' "${url}"
}

normalize_db_url() {
  local url="${1:-}"
  printf '%s' "${url/mysql+pymysql:\/\//mysql://}"
}

resolve_memoria_target() {
  local os arch
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"
  case "${arch}" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) arch="" ;;
  esac
  case "${os}" in
    linux)
      [[ "${arch}" == "x86_64" ]] && printf 'x86_64-unknown-linux-musl' && return 0
      [[ "${arch}" == "aarch64" ]] && printf 'aarch64-unknown-linux-musl' && return 0
      ;;
    darwin)
      [[ "${arch}" == "x86_64" ]] && printf 'x86_64-apple-darwin' && return 0
      [[ "${arch}" == "aarch64" ]] && printf 'aarch64-apple-darwin' && return 0
      ;;
  esac
  fail "Unsupported platform: $(uname -s) $(uname -m)"
}

install_memoria_binary() {
  local install_dir="$1"
  local version="$2"
  local target asset url sum_url tmp

  need_cmd curl
  need_cmd tar

  target="$(resolve_memoria_target)"
  asset="memoria-${target}.tar.gz"
  url="https://github.com/matrixorigin/Memoria/releases/download/${version}/${asset}"
  sum_url="https://github.com/matrixorigin/Memoria/releases/download/${version}/SHA256SUMS.txt"

  mkdir -p "${install_dir}"
  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp}"' RETURN

  log "Downloading Rust Memoria ${version} (${target})"
  curl -fL# -o "${tmp}/${asset}" "${url}"

  if curl -fsSL -o "${tmp}/SHA256SUMS.txt" "${sum_url}" 2>/dev/null; then
    if command -v sha256sum >/dev/null 2>&1; then
      (cd "${tmp}" && grep -F "${asset}" SHA256SUMS.txt | sha256sum -c - >/dev/null)
    elif command -v shasum >/dev/null 2>&1; then
      (cd "${tmp}" && grep -F "${asset}" SHA256SUMS.txt | shasum -a 256 -c - >/dev/null)
    fi
  fi

  tar -xzf "${tmp}/${asset}" -C "${tmp}"
  cp "${tmp}/memoria" "${install_dir}/memoria"
  chmod +x "${install_dir}/memoria"
  printf '%s' "${install_dir}/memoria"
}

resolve_memoria_bin() {
  local candidate="${1:-}"
  if [[ -n "${candidate}" ]]; then
    if [[ "${candidate}" == */* ]]; then
      [[ -x "${candidate}" ]] || fail "Memoria executable is not executable: ${candidate}"
      printf '%s' "${candidate}"
      return 0
    fi
    local resolved=''
    resolved="$(command -v "${candidate}" 2>/dev/null || true)"
    [[ -n "${resolved}" ]] || fail "Could not find memoria executable in PATH: ${candidate}"
    printf '%s' "${resolved}"
    return 0
  fi

  local resolved=''
  resolved="$(command -v memoria 2>/dev/null || true)"
  if [[ -n "${resolved}" ]]; then
    printf '%s' "${resolved}"
    return 0
  fi

  return 1
}

run_openclaw() {
  if [[ -n "${OPENCLAW_HOME_VALUE}" ]]; then
    OPENCLAW_HOME="${OPENCLAW_HOME_VALUE}" "${OPENCLAW_BIN}" "$@"
  else
    "${OPENCLAW_BIN}" "$@"
  fi
}

config_file_path() {
  if [[ -n "${OPENCLAW_HOME_VALUE}" ]]; then
    printf '%s/.openclaw/openclaw.json' "${OPENCLAW_HOME_VALUE}"
  else
    printf '%s/.openclaw/openclaw.json' "${HOME}"
  fi
}

skills_dir_path() {
  if [[ -n "${OPENCLAW_HOME_VALUE}" ]]; then
    printf '%s/.openclaw/skills' "${OPENCLAW_HOME_VALUE}"
  else
    printf '%s/.openclaw/skills' "${HOME}"
  fi
}

install_bundled_skills() {
  local source_skills_dir="$1"
  local managed_skills_dir="$2"

  [[ -d "${source_skills_dir}" ]] || return 0

  mkdir -p "${managed_skills_dir}"
  local skill_dir=""
  for skill_dir in "${source_skills_dir}"/*; do
    [[ -d "${skill_dir}" ]] || continue
    local skill_name
    skill_name="$(basename -- "${skill_dir}")"
    rm -rf "${managed_skills_dir}/${skill_name}"
    cp -R "${skill_dir}" "${managed_skills_dir}/${skill_name}"
    log "Installed managed skill: ${skill_name}"
  done
}

SOURCE_DIR="${MEMORIA_SOURCE_DIR:-}"
INSTALL_DIR="${MEMORIA_INSTALL_DIR:-$HOME/.local/share/openclaw-plugins/openclaw-memoria}"
REPO_URL="${MEMORIA_REPO_URL:-$DEFAULT_REPO_URL}"
REPO_REF="${MEMORIA_REPO_REF:-$DEFAULT_REPO_REF}"
OPENCLAW_BIN="${OPENCLAW_BIN:-openclaw}"
OPENCLAW_HOME_VALUE="${OPENCLAW_HOME:-}"
MEMORIA_BIN="${MEMORIA_EXECUTABLE:-${MEMORIA_BIN:-}}"
MEMORIA_RELEASE_TAG="${MEMORIA_RELEASE_TAG:-$DEFAULT_MEMORIA_VERSION}"
MEMORIA_BINARY_INSTALL_DIR="${MEMORIA_BINARY_INSTALL_DIR:-$HOME/.local/bin}"
SKIP_MEMORIA_INSTALL=false
SKIP_PLUGIN_INSTALL=false
RUN_VERIFY=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --source-dir)
      SOURCE_DIR="${2:?missing value for --source-dir}"
      shift 2
      ;;
    --install-dir)
      INSTALL_DIR="${2:?missing value for --install-dir}"
      shift 2
      ;;
    --repo-url)
      REPO_URL="${2:?missing value for --repo-url}"
      shift 2
      ;;
    --ref)
      REPO_REF="${2:?missing value for --ref}"
      shift 2
      ;;
    --openclaw-bin)
      OPENCLAW_BIN="${2:?missing value for --openclaw-bin}"
      shift 2
      ;;
    --memoria-bin)
      MEMORIA_BIN="${2:?missing value for --memoria-bin}"
      shift 2
      ;;
    --memoria-version)
      MEMORIA_RELEASE_TAG="${2:?missing value for --memoria-version}"
      shift 2
      ;;
    --memoria-install-dir)
      MEMORIA_BINARY_INSTALL_DIR="${2:?missing value for --memoria-install-dir}"
      shift 2
      ;;
    --skip-memoria-install)
      SKIP_MEMORIA_INSTALL=true
      shift
      ;;
    --skip-plugin-install)
      SKIP_PLUGIN_INSTALL=true
      shift
      ;;
    --verify)
      RUN_VERIFY=true
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      fail "Unknown option: $1"
      ;;
  esac
done

OPENCLAW_BIN="$(resolve_openclaw_bin "${OPENCLAW_BIN}")"
validate_openclaw_bin "${OPENCLAW_BIN}" || fail "OpenClaw executable is not healthy: ${OPENCLAW_BIN}. Fix OpenClaw first, then retry."
need_cmd node

log "Using OpenClaw executable: ${OPENCLAW_BIN}"
log "OpenClaw version: $("${OPENCLAW_BIN}" --version 2>/dev/null | head -n 1)"

if [[ -z "${SOURCE_DIR}" ]]; then
  if [[ -f "${PWD}/openclaw.plugin.json" && -f "${PWD}/package.json" ]]; then
    SOURCE_DIR="${PWD}"
  else
    SCRIPT_SOURCE="${BASH_SOURCE:-}"
    if [[ -n "${SCRIPT_SOURCE}" ]]; then
      SCRIPT_DIR="$(cd -- "$(dirname -- "${SCRIPT_SOURCE}")" && pwd)"
      REPO_CANDIDATE="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
      if [[ -f "${REPO_CANDIDATE}/openclaw.plugin.json" && -f "${REPO_CANDIDATE}/package.json" ]]; then
        SOURCE_DIR="${REPO_CANDIDATE}"
      fi
    fi
  fi
fi

if [[ -z "${SOURCE_DIR}" ]]; then
  need_cmd git
  SOURCE_DIR="${INSTALL_DIR}"
  mkdir -p "$(dirname -- "${SOURCE_DIR}")"
  if [[ -d "${SOURCE_DIR}/.git" ]]; then
    log "Updating existing checkout in ${SOURCE_DIR}"
    git -C "${SOURCE_DIR}" fetch --depth 1 origin "${REPO_REF}"
    git -C "${SOURCE_DIR}" checkout -f FETCH_HEAD
  elif [[ -e "${SOURCE_DIR}" ]]; then
    fail "Install dir already exists and is not a git checkout: ${SOURCE_DIR}"
  else
    log "Cloning ${REPO_URL}#${REPO_REF} to ${SOURCE_DIR}"
    git clone --depth 1 --branch "${REPO_REF}" "${REPO_URL}" "${SOURCE_DIR}"
  fi
else
  SOURCE_DIR="$(cd -- "${SOURCE_DIR}" && pwd)"
  log "Using existing checkout: ${SOURCE_DIR}"
fi

[[ -f "${SOURCE_DIR}/openclaw.plugin.json" ]] || fail "Missing openclaw.plugin.json in ${SOURCE_DIR}"
[[ -f "${SOURCE_DIR}/package.json" ]] || fail "Missing package.json in ${SOURCE_DIR}"

if MEMORIA_EXECUTABLE_VALUE="$(resolve_memoria_bin "${MEMORIA_BIN}" 2>/dev/null)"; then
  log "Using existing memoria executable: ${MEMORIA_EXECUTABLE_VALUE}"
else
  [[ "${SKIP_MEMORIA_INSTALL}" == false ]] || fail "--skip-memoria-install requires an existing memoria executable"
  MEMORIA_EXECUTABLE_VALUE="$(install_memoria_binary "${MEMORIA_BINARY_INSTALL_DIR}" "${MEMORIA_RELEASE_TAG}")"
  log "Installed memoria executable: ${MEMORIA_EXECUTABLE_VALUE}"
fi
log "Memoria version: $("${MEMORIA_EXECUTABLE_VALUE}" --version 2>/dev/null | head -n 1)"

MEMORIA_DB_URL="$(normalize_db_url "${MEMORIA_DB_URL:-mysql://root:111@127.0.0.1:6001/memoria}")"
MEMORIA_DEFAULT_USER_ID="${MEMORIA_DEFAULT_USER_ID:-openclaw-user}"
MEMORIA_USER_ID_STRATEGY="${MEMORIA_USER_ID_STRATEGY:-config}"
MEMORIA_AUTO_RECALL="$(normalize_bool "${MEMORIA_AUTO_RECALL:-true}")"
MEMORIA_AUTO_OBSERVE="$(normalize_bool "${MEMORIA_AUTO_OBSERVE:-false}")"
MEMORIA_EMBEDDING_PROVIDER="${MEMORIA_EMBEDDING_PROVIDER:-openai}"
MEMORIA_EMBEDDING_MODEL="${MEMORIA_EMBEDDING_MODEL:-text-embedding-3-small}"
MEMORIA_EMBEDDING_BASE_URL="${MEMORIA_EMBEDDING_BASE_URL:-}"
MEMORIA_EMBEDDING_API_KEY="${MEMORIA_EMBEDDING_API_KEY:-}"
MEMORIA_EMBEDDING_DIM="${MEMORIA_EMBEDDING_DIM:-}"
MEMORIA_LLM_BASE_URL="${MEMORIA_LLM_BASE_URL:-}"
MEMORIA_LLM_API_KEY="${MEMORIA_LLM_API_KEY:-}"
MEMORIA_LLM_MODEL="${MEMORIA_LLM_MODEL:-}"

EMBEDDING_BASE_URL_RAW="${MEMORIA_EMBEDDING_BASE_URL}"
LLM_BASE_URL_RAW="${MEMORIA_LLM_BASE_URL}"

MEMORIA_EMBEDDING_BASE_URL="$(normalize_base_url "${MEMORIA_EMBEDDING_BASE_URL}")"
MEMORIA_LLM_BASE_URL="$(normalize_base_url "${MEMORIA_LLM_BASE_URL}")"

if [[ -n "${EMBEDDING_BASE_URL_RAW}" && "${EMBEDDING_BASE_URL_RAW}" != "${MEMORIA_EMBEDDING_BASE_URL}" ]]; then
  log "Normalized embedding base URL to ${MEMORIA_EMBEDDING_BASE_URL}"
fi
if [[ -n "${LLM_BASE_URL_RAW}" && "${LLM_BASE_URL_RAW}" != "${MEMORIA_LLM_BASE_URL}" ]]; then
  log "Normalized LLM base URL to ${MEMORIA_LLM_BASE_URL}"
fi

KNOWN_EMBEDDING_DIM="$(infer_embedding_dim "${MEMORIA_EMBEDDING_MODEL}")"
if [[ "${MEMORIA_EMBEDDING_PROVIDER}" != "local" && -z "${MEMORIA_EMBEDDING_API_KEY}" ]]; then
  fail "MEMORIA_EMBEDDING_API_KEY is required unless provider=local"
fi
if [[ "${MEMORIA_EMBEDDING_PROVIDER}" != "local" && -z "${MEMORIA_EMBEDDING_DIM}" ]]; then
  MEMORIA_EMBEDDING_DIM="${KNOWN_EMBEDDING_DIM}"
  [[ -n "${MEMORIA_EMBEDDING_DIM}" ]] || fail "MEMORIA_EMBEDDING_DIM is required for model ${MEMORIA_EMBEDDING_MODEL}"
  log "Auto-selected embedding dimension ${MEMORIA_EMBEDDING_DIM} for ${MEMORIA_EMBEDDING_MODEL}"
fi
if [[ "${MEMORIA_EMBEDDING_PROVIDER}" == "local" ]]; then
  log "Embedding provider is local. Make sure your memoria binary was built with local-embedding support."
fi
if [[ "${MEMORIA_AUTO_OBSERVE}" == "true" ]]; then
  [[ -n "${MEMORIA_LLM_API_KEY}" ]] || fail "MEMORIA_AUTO_OBSERVE=true requires MEMORIA_LLM_API_KEY"
  [[ -n "${MEMORIA_LLM_MODEL}" ]] || fail "MEMORIA_AUTO_OBSERVE=true requires MEMORIA_LLM_MODEL"
fi

CONFIG_FILE="$(config_file_path)"

if [[ "${SKIP_PLUGIN_INSTALL}" == false ]]; then
  log "Installing plugin into OpenClaw"
  run_openclaw plugins install --link "${SOURCE_DIR}"
  run_openclaw plugins enable "${PLUGIN_ID}"
else
  log "Skipping OpenClaw plugin install/enable; assuming ${PLUGIN_ID} is already active"
fi

log "Writing plugin configuration"
MEMORIA_TOOL_NAMES_JSON="$(printf '%s\n' "${MEMORIA_TOOL_NAMES[@]}" | node -e 'const fs=require("node:fs"); const lines=fs.readFileSync(0,"utf8").trim().split(/\n+/).filter(Boolean); process.stdout.write(JSON.stringify(lines));')"

CONFIG_FILE="${CONFIG_FILE}" \
PLUGIN_ID="${PLUGIN_ID}" \
SOURCE_DIR="${SOURCE_DIR}" \
MEMORIA_EXECUTABLE_VALUE="${MEMORIA_EXECUTABLE_VALUE}" \
MEMORIA_DB_URL="${MEMORIA_DB_URL}" \
MEMORIA_DEFAULT_USER_ID="${MEMORIA_DEFAULT_USER_ID}" \
MEMORIA_USER_ID_STRATEGY="${MEMORIA_USER_ID_STRATEGY}" \
MEMORIA_AUTO_RECALL="${MEMORIA_AUTO_RECALL}" \
MEMORIA_AUTO_OBSERVE="${MEMORIA_AUTO_OBSERVE}" \
MEMORIA_EMBEDDING_PROVIDER="${MEMORIA_EMBEDDING_PROVIDER}" \
MEMORIA_EMBEDDING_MODEL="${MEMORIA_EMBEDDING_MODEL}" \
MEMORIA_EMBEDDING_BASE_URL="${MEMORIA_EMBEDDING_BASE_URL}" \
MEMORIA_EMBEDDING_API_KEY="${MEMORIA_EMBEDDING_API_KEY}" \
MEMORIA_EMBEDDING_DIM="${MEMORIA_EMBEDDING_DIM}" \
MEMORIA_LLM_BASE_URL="${MEMORIA_LLM_BASE_URL}" \
MEMORIA_LLM_API_KEY="${MEMORIA_LLM_API_KEY}" \
MEMORIA_LLM_MODEL="${MEMORIA_LLM_MODEL}" \
MEMORIA_TOOL_NAMES_JSON="${MEMORIA_TOOL_NAMES_JSON}" \
node - <<'NODE'
const fs = require("node:fs");
const path = require("node:path");

const configPath = path.resolve(process.env.CONFIG_FILE);
const pluginId = process.env.PLUGIN_ID;
const sourceDir = path.resolve(process.env.SOURCE_DIR);
const memoriaToolNames = JSON.parse(process.env.MEMORIA_TOOL_NAMES_JSON);

function readJson(filePath) {
  if (!fs.existsSync(filePath)) {
    return {};
  }
  return JSON.parse(fs.readFileSync(filePath, "utf8"));
}

function writeJson(filePath, value) {
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  fs.writeFileSync(filePath, `${JSON.stringify(value, null, 2)}\n`);
}

function manifestPluginId(candidatePath) {
  try {
    const manifestPath = path.join(candidatePath, "openclaw.plugin.json");
    if (!fs.existsSync(manifestPath)) {
      return null;
    }
    const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
    return typeof manifest.id === "string" ? manifest.id : null;
  } catch {
    return null;
  }
}

function mergeToolPolicy(policy) {
  const result = policy && typeof policy === "object" && !Array.isArray(policy) ? { ...policy } : {};
  const targetKey = Array.isArray(result.allow) ? "allow" : Array.isArray(result.alsoAllow) ? "alsoAllow" : "alsoAllow";
  const current = Array.isArray(result[targetKey]) ? [...result[targetKey]] : [];
  for (const toolName of memoriaToolNames) {
    if (!current.includes(toolName)) {
      current.push(toolName);
    }
  }
  result[targetKey] = current;
  return result;
}

const data = readJson(configPath);
const plugins = data.plugins && typeof data.plugins === "object" && !Array.isArray(data.plugins)
  ? data.plugins
  : (data.plugins = {});

const load = plugins.load && typeof plugins.load === "object" && !Array.isArray(plugins.load)
  ? plugins.load
  : (plugins.load = {});
const existingLoadPaths = Array.isArray(load.paths) ? load.paths : [];
const nextLoadPaths = [];
const seenLoadPaths = new Set();

for (const entry of existingLoadPaths) {
  if (typeof entry !== "string" || !entry.trim()) {
    continue;
  }
  const trimmed = entry.trim();
  const resolved = path.resolve(trimmed.replace(/^~(?=$|\/|\\)/, process.env.HOME || "~"));
  if ((trimmed.includes("openclaw-memoria") || trimmed.includes(pluginId)) && !fs.existsSync(resolved)) {
    continue;
  }
  if (seenLoadPaths.has(resolved)) {
    continue;
  }
  seenLoadPaths.add(resolved);
  nextLoadPaths.push(trimmed);
}

if (!seenLoadPaths.has(sourceDir)) {
  nextLoadPaths.push(sourceDir);
}

load.paths = nextLoadPaths;

plugins.allow = Array.isArray(plugins.allow) ? plugins.allow : [];
if (!plugins.allow.includes(pluginId)) {
  plugins.allow.push(pluginId);
}

plugins.entries = plugins.entries && typeof plugins.entries === "object" && !Array.isArray(plugins.entries)
  ? plugins.entries
  : {};
delete plugins.entries[JSON.stringify(pluginId)];

const pluginEntry = plugins.entries[pluginId] && typeof plugins.entries[pluginId] === "object" && !Array.isArray(plugins.entries[pluginId])
  ? plugins.entries[pluginId]
  : (plugins.entries[pluginId] = {});
pluginEntry.enabled = true;
pluginEntry.config = {
  backend: "embedded",
  memoriaExecutable: process.env.MEMORIA_EXECUTABLE_VALUE,
  dbUrl: process.env.MEMORIA_DB_URL,
  defaultUserId: process.env.MEMORIA_DEFAULT_USER_ID,
  userIdStrategy: process.env.MEMORIA_USER_ID_STRATEGY,
  autoRecall: process.env.MEMORIA_AUTO_RECALL === "true",
  autoObserve: process.env.MEMORIA_AUTO_OBSERVE === "true",
  embeddingProvider: process.env.MEMORIA_EMBEDDING_PROVIDER,
  embeddingModel: process.env.MEMORIA_EMBEDDING_MODEL
};

const optionalFields = {
  embeddingBaseUrl: process.env.MEMORIA_EMBEDDING_BASE_URL,
  embeddingApiKey: process.env.MEMORIA_EMBEDDING_API_KEY,
  llmBaseUrl: process.env.MEMORIA_LLM_BASE_URL,
  llmApiKey: process.env.MEMORIA_LLM_API_KEY,
  llmModel: process.env.MEMORIA_LLM_MODEL
};
for (const [key, value] of Object.entries(optionalFields)) {
  if (value) {
    pluginEntry.config[key] = value;
  }
}
if (process.env.MEMORIA_EMBEDDING_DIM) {
  pluginEntry.config.embeddingDim = Number.parseInt(process.env.MEMORIA_EMBEDDING_DIM, 10);
}

pluginEntry.hooks = pluginEntry.hooks && typeof pluginEntry.hooks === "object" && !Array.isArray(pluginEntry.hooks)
  ? pluginEntry.hooks
  : {};
pluginEntry.hooks.allowPromptInjection = true;

plugins.slots = plugins.slots && typeof plugins.slots === "object" && !Array.isArray(plugins.slots)
  ? plugins.slots
  : {};
plugins.slots.memory = pluginId;

data.tools = data.tools && typeof data.tools === "object" && !Array.isArray(data.tools)
  ? data.tools
  : {};
Object.assign(data.tools, mergeToolPolicy(data.tools));

if (data.agents && typeof data.agents === "object" && Array.isArray(data.agents.list)) {
  for (const agent of data.agents.list) {
    if (!agent || typeof agent !== "object" || Array.isArray(agent)) {
      continue;
    }
    agent.tools = mergeToolPolicy(agent.tools);
  }
}

writeJson(configPath, data);
NODE

log "Validating OpenClaw config"
run_openclaw config validate >/dev/null

install_bundled_skills "${SOURCE_DIR}/skills" "$(skills_dir_path)"

if [[ "${RUN_VERIFY}" == true ]]; then
  log "Running install verification"
  node "${SOURCE_DIR}/scripts/verify_plugin_install.mjs" \
    --openclaw-bin "${OPENCLAW_BIN}" \
    --config-file "${CONFIG_FILE}" \
    --memoria-bin "${MEMORIA_EXECUTABLE_VALUE}"
fi

cat <<EOF

Install complete.

Plugin source: ${SOURCE_DIR}
Memoria executable: ${MEMORIA_EXECUTABLE_VALUE}
OpenClaw config: ${CONFIG_FILE}

Recommended smoke checks:
  openclaw memoria capabilities
  openclaw memoria stats
  openclaw ltm list --limit 10

If embedded mode is enabled, make sure MatrixOne is reachable at:
  ${MEMORIA_DB_URL}
EOF
