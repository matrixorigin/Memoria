#!/usr/bin/env bash
set -euo pipefail

PLUGIN_ID="memory-memoria"
DEFAULT_INSTALL_DIR="${HOME}/.local/share/openclaw-plugins/openclaw-memoria"

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

usage() {
  cat <<'EOF'
Remove the OpenClaw Memoria plugin and its OpenClaw config.

Usage:
  bash scripts/uninstall-openclaw-memoria.sh [options]
  curl -fsSL <raw-script-url> | bash -s --

Options:
  --source-dir <path>  Also delete this plugin checkout after uninstall.
  --keep-source        Keep the managed install directory on disk.
  --help               Show this help text.

Environment overrides:
  OPENCLAW_HOME        Optional target OpenClaw home.

What gets removed by default:
  - plugins.entries["memory-memoria"]
  - plugins.installs["memory-memoria"]
  - plugins.allow entry for memory-memoria
  - plugins.load.paths entries that point at this plugin
  - tool policy entries for the Memoria tool surface
  - managed companion skills in ~/.openclaw/skills: memoria-memory, memoria-recovery
  - the default managed plugin dir: ~/.local/share/openclaw-plugins/openclaw-memoria

What gets restored:
  - plugins.slots.memory -> memory-core
  - plugins.entries["memory-core"].enabled -> true
EOF
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

OPENCLAW_HOME_VALUE="${OPENCLAW_HOME:-}"
SOURCE_DIR=""
KEEP_SOURCE=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --source-dir)
      SOURCE_DIR="${2:?missing value for --source-dir}"
      shift 2
      ;;
    --keep-source)
      KEEP_SOURCE=true
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

need_cmd node

CONFIG_FILE="$(config_file_path)"
MANAGED_SKILLS_DIR="$(skills_dir_path)"
MEMORIA_TOOL_NAMES_JSON="$(printf '%s\n' "${MEMORIA_TOOL_NAMES[@]}" | node -e 'const fs=require("node:fs"); const lines=fs.readFileSync(0,"utf8").trim().split(/\n+/).filter(Boolean); process.stdout.write(JSON.stringify(lines));')"

UNINSTALL_RESULT="$(
  CONFIG_FILE="${CONFIG_FILE}" \
  PLUGIN_ID="${PLUGIN_ID}" \
  DEFAULT_INSTALL_DIR="${DEFAULT_INSTALL_DIR}" \
  SOURCE_DIR_RAW="${SOURCE_DIR}" \
  KEEP_SOURCE="${KEEP_SOURCE}" \
  MEMORIA_TOOL_NAMES_JSON="${MEMORIA_TOOL_NAMES_JSON}" \
  node - <<'NODE'
const fs = require("node:fs");
const path = require("node:path");

const configPath = path.resolve(process.env.CONFIG_FILE);
const pluginId = process.env.PLUGIN_ID;
const defaultInstallDir = path.resolve(process.env.DEFAULT_INSTALL_DIR);
const sourceDirRaw = process.env.SOURCE_DIR_RAW || "";
const keepSource = process.env.KEEP_SOURCE === "true";
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

function maybePath(rawPath) {
  if (!rawPath) {
    return null;
  }
  return path.resolve(rawPath.replace(/^~(?=$|\/|\\)/, process.env.HOME || "~"));
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

const data = readJson(configPath);
let changed = false;
const plugins = data.plugins && typeof data.plugins === "object" && !Array.isArray(data.plugins)
  ? data.plugins
  : {};
const tools = data.tools && typeof data.tools === "object" && !Array.isArray(data.tools)
  ? data.tools
  : {};

const candidatePaths = new Set([defaultInstallDir]);
const sourceDir = maybePath(sourceDirRaw);
if (sourceDir) {
  candidatePaths.add(sourceDir);
}

if (plugins.entries && typeof plugins.entries === "object" && !Array.isArray(plugins.entries)) {
  if (pluginId in plugins.entries) {
    delete plugins.entries[pluginId];
    changed = true;
  }
  const quotedKey = JSON.stringify(pluginId);
  if (quotedKey in plugins.entries) {
    delete plugins.entries[quotedKey];
    changed = true;
  }
  if (Object.keys(plugins.entries).length === 0) {
    delete plugins.entries;
  }
}

if (plugins.installs && typeof plugins.installs === "object" && !Array.isArray(plugins.installs)) {
  if (pluginId in plugins.installs) {
    delete plugins.installs[pluginId];
    changed = true;
  }
  if (Object.keys(plugins.installs).length === 0) {
    delete plugins.installs;
  }
}

if (Array.isArray(plugins.allow)) {
  const next = plugins.allow.filter((entry) => entry !== pluginId);
  if (next.length !== plugins.allow.length) {
    plugins.allow = next;
    changed = true;
  }
  if (plugins.allow.length === 0) {
    delete plugins.allow;
  }
}

if (plugins.load && typeof plugins.load === "object" && !Array.isArray(plugins.load) && Array.isArray(plugins.load.paths)) {
  const next = plugins.load.paths.filter((entry) => {
    if (typeof entry !== "string") {
      return true;
    }
    const resolved = maybePath(entry);
    if (resolved && candidatePaths.has(resolved)) {
      changed = true;
      return false;
    }
    if (resolved && manifestPluginId(resolved) === pluginId) {
      changed = true;
      return false;
    }
    if ((entry.includes("openclaw-memoria") || entry.includes(pluginId)) && (!resolved || !fs.existsSync(resolved))) {
      changed = true;
      return false;
    }
    return true;
  });
  plugins.load.paths = next;
  if (plugins.load.paths.length === 0) {
    delete plugins.load.paths;
  }
  if (Object.keys(plugins.load).length === 0) {
    delete plugins.load;
  }
}

if (plugins.slots && typeof plugins.slots === "object" && !Array.isArray(plugins.slots) && plugins.slots.memory === pluginId) {
  plugins.slots.memory = "memory-core";
  plugins.entries = plugins.entries && typeof plugins.entries === "object" && !Array.isArray(plugins.entries)
    ? plugins.entries
    : {};
  const coreEntry = plugins.entries["memory-core"] && typeof plugins.entries["memory-core"] === "object" && !Array.isArray(plugins.entries["memory-core"])
    ? plugins.entries["memory-core"]
    : (plugins.entries["memory-core"] = {});
  coreEntry.enabled = true;
  changed = true;
}

for (const key of ["allow", "alsoAllow"]) {
  if (Array.isArray(tools[key])) {
    const next = tools[key].filter((entry) => !memoriaToolNames.includes(entry));
    if (next.length !== tools[key].length) {
      tools[key] = next;
      changed = true;
    }
    if (tools[key].length === 0) {
      delete tools[key];
    }
  }
}

if (data.agents && typeof data.agents === "object" && Array.isArray(data.agents.list)) {
  for (const agent of data.agents.list) {
    if (!agent || typeof agent !== "object" || Array.isArray(agent) || !agent.tools || typeof agent.tools !== "object" || Array.isArray(agent.tools)) {
      continue;
    }
    for (const key of ["allow", "alsoAllow"]) {
      if (Array.isArray(agent.tools[key])) {
        const next = agent.tools[key].filter((entry) => !memoriaToolNames.includes(entry));
        if (next.length !== agent.tools[key].length) {
          agent.tools[key] = next;
          changed = true;
        }
        if (agent.tools[key].length === 0) {
          delete agent.tools[key];
        }
      }
    }
    if (Object.keys(agent.tools).length === 0) {
      delete agent.tools;
    }
  }
}

if (Object.keys(plugins).length > 0) {
  data.plugins = plugins;
} else {
  delete data.plugins;
}
if (Object.keys(tools).length > 0) {
  data.tools = tools;
} else {
  delete data.tools;
}

if (changed && fs.existsSync(configPath)) {
  writeJson(`${configPath}.bak`, readJson(configPath));
  writeJson(configPath, data);
}

const deletedPaths = [];
if (!keepSource && fs.existsSync(defaultInstallDir)) {
  fs.rmSync(defaultInstallDir, { recursive: true, force: true });
  deletedPaths.push(defaultInstallDir);
}
if (sourceDir && fs.existsSync(sourceDir)) {
  fs.rmSync(sourceDir, { recursive: true, force: true });
  deletedPaths.push(sourceDir);
}

process.stdout.write(JSON.stringify({
  ok: true,
  configFile: configPath,
  configChanged: changed,
  deletedPaths
}));
NODE
)"

for skill_name in memoria-memory memoria-recovery; do
  if [[ -d "${MANAGED_SKILLS_DIR}/${skill_name}" ]]; then
    rm -rf "${MANAGED_SKILLS_DIR:?}/${skill_name}"
    log "Removed managed skill: ${skill_name}"
  fi
done

log "Removed OpenClaw Memoria plugin configuration"
log "${UNINSTALL_RESULT}"

cat <<EOF

Uninstall complete.

Config file: ${CONFIG_FILE}

Recommended follow-up checks:
  cd ~
  openclaw plugins list --json | rg 'memory-memoria|openclaw-memoria' || true
  openclaw config get 'plugins.slots.memory'
EOF
