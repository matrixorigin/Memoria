#!/usr/bin/env node
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";

function readArg(name, fallback = "") {
  const index = process.argv.indexOf(name);
  if (index >= 0 && index + 1 < process.argv.length) {
    return process.argv[index + 1];
  }
  return fallback;
}

const openclawBin = readArg("--openclaw-bin", "openclaw");
const memoriaBin = readArg("--memoria-bin", "memoria");
const configFile = path.resolve(readArg("--config-file", path.join(process.env.HOME || "", ".openclaw", "openclaw.json")));

function run(command, args) {
  return execFileSync(command, args, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
}

function runResult(command, args) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });

  return {
    status: typeof result.status === "number" ? result.status : null,
    signal: result.signal ?? null,
    stdout: typeof result.stdout === "string" ? result.stdout.trim() : "",
    stderr: typeof result.stderr === "string" ? result.stderr.trim() : "",
    error: result.error ? String(result.error) : null,
  };
}

function stripAnsi(value) {
  return typeof value === "string"
    ? value.replace(/\x1B\[[0-?]*[ -/]*[@-~]/g, "")
    : "";
}

function tryParseJson(raw) {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

function parseCommandJson(raw) {
  const cleaned = stripAnsi(raw).trim();
  const direct = tryParseJson(cleaned);
  if (direct) {
    return direct;
  }
  const start = cleaned.indexOf("{");
  const end = cleaned.lastIndexOf("}");
  if (start >= 0 && end > start) {
    return tryParseJson(cleaned.slice(start, end + 1)) ?? cleaned;
  }
  return cleaned;
}

function parsePort(rawProtocol, rawPort) {
  if (rawPort) {
    return Number.parseInt(rawPort, 10);
  }
  return rawProtocol === "mysql:" ? 3306 : 80;
}

function checkTcp(host, port, timeoutMs = 1500) {
  return new Promise((resolve) => {
    const socket = new net.Socket();
    let settled = false;

    const finish = (value) => {
      if (settled) {
        return;
      }
      settled = true;
      socket.destroy();
      resolve(value);
    };

    socket.setTimeout(timeoutMs);
    socket.once("connect", () => finish(true));
    socket.once("timeout", () => finish(false));
    socket.once("error", () => finish(false));
    socket.connect(port, host);
  });
}

if (!fs.existsSync(configFile)) {
  throw new Error(`OpenClaw config file not found: ${configFile}`);
}

const config = JSON.parse(fs.readFileSync(configFile, "utf8"));
const pluginEntry = config?.plugins?.entries?.["memory-memoria"];
if (!pluginEntry || pluginEntry.enabled !== true) {
  throw new Error("plugins.entries.memory-memoria is not enabled");
}

const pluginConfig = pluginEntry.config ?? {};
if (pluginConfig.memoriaExecutable == null) {
  throw new Error("plugins.entries.memory-memoria.config.memoriaExecutable is missing");
}

const resolvedMemoriaBin = pluginConfig.memoriaExecutable || memoriaBin;
const backend = pluginConfig.backend ?? "embedded";
const userId =
  typeof pluginConfig.defaultUserId === "string" && pluginConfig.defaultUserId.trim()
    ? pluginConfig.defaultUserId.trim()
    : "openclaw-user";
const openclawVersion = run(openclawBin, ["--version"]);
const configValidation = tryParseJson(run(openclawBin, ["config", "validate", "--json"]));
const memoriaVersion = run(resolvedMemoriaBin, ["--version"]);
const capabilities = run(openclawBin, ["memoria", "capabilities"]);

if (!capabilities.includes("memory_branch") || !capabilities.includes("memory_snapshot")) {
  throw new Error("OpenClaw memoria capabilities output is missing expected Rust Memoria tools");
}

const result = {
  ok: true,
  configFile,
  openclawVersion,
  memoriaVersion,
  memoriaExecutable: pluginConfig.memoriaExecutable,
  configValidation,
  healthCheck: {
    performed: true,
    ok: false,
    backend,
    userId,
  },
  deepChecks: {
    backend,
    performed: false,
    skipped: null,
  },
};

const healthResult = runResult(openclawBin, ["memoria", "health", "--user-id", userId]);
result.healthCheck.exitStatus = healthResult.status;
result.healthCheck.stdout = parseCommandJson(healthResult.stdout);
if (healthResult.stderr) {
  result.healthCheck.stderr = healthResult.stderr;
}
if (healthResult.error) {
  result.healthCheck.error = healthResult.error;
}
if (healthResult.status === 0) {
  result.healthCheck.ok = true;
} else {
  const hints = [];
  if (backend === "embedded" && typeof pluginConfig.dbUrl === "string") {
    hints.push(`Verify MatrixOne is reachable at ${pluginConfig.dbUrl}.`);
  }
  if (backend === "embedded" && pluginConfig.embeddingProvider === "local") {
    hints.push(
      "embeddingProvider=local requires a memoria binary built with local-embedding support; otherwise setup may validate but health/search will fail at runtime.",
    );
  }
  if (backend === "http" && typeof pluginConfig.apiUrl === "string") {
    hints.push(`Verify the remote Memoria API is healthy and reachable at ${pluginConfig.apiUrl}.`);
  }

  result.ok = false;
  result.healthCheck.hints = hints;
  console.log(JSON.stringify(result, null, 2));
  process.exit(1);
}

if (backend === "embedded" && typeof pluginConfig.dbUrl === "string") {
  const dbUrl = new URL(pluginConfig.dbUrl);
  const dbReachable = await checkTcp(
    dbUrl.hostname || "127.0.0.1",
    parsePort(dbUrl.protocol, dbUrl.port),
  );

  result.deepChecks.dbUrl = pluginConfig.dbUrl;
  result.deepChecks.dbReachable = dbReachable;

  if (dbReachable) {
    const statsRaw = run(openclawBin, ["memoria", "stats", "--user-id", userId]);
    const listRaw = run(openclawBin, ["ltm", "list", "--limit", "1", "--user-id", userId]);
    result.deepChecks.performed = true;
    result.deepChecks.userId = userId;
    result.deepChecks.stats = parseCommandJson(statsRaw);
    result.deepChecks.list = parseCommandJson(listRaw);
  } else {
    result.ok = false;
    result.deepChecks.skipped = "Embedded database is not reachable; skipped stats/list verification.";
  }
} else {
  const listRaw = run(openclawBin, ["ltm", "list", "--limit", "1", "--user-id", userId]);
  result.deepChecks.performed = true;
  result.deepChecks.userId = userId;
  result.deepChecks.list = parseCommandJson(listRaw);
}

console.log(JSON.stringify(result, null, 2));
if (!result.ok) {
  process.exit(1);
}
