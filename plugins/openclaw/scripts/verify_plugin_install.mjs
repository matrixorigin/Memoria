#!/usr/bin/env node
import { execFileSync } from "node:child_process";
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

function tryParseJson(raw) {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

function parseCommandJson(raw) {
  const direct = tryParseJson(raw);
  if (direct) {
    return direct;
  }
  const start = raw.indexOf("{");
  const end = raw.lastIndexOf("}");
  if (start >= 0 && end > start) {
    return tryParseJson(raw.slice(start, end + 1)) ?? raw;
  }
  return raw;
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
  deepChecks: {
    backend: pluginConfig.backend ?? "embedded",
    performed: false,
    skipped: null,
  },
};

if ((pluginConfig.backend ?? "embedded") === "embedded" && typeof pluginConfig.dbUrl === "string") {
  const dbUrl = new URL(pluginConfig.dbUrl);
  const dbReachable = await checkTcp(
    dbUrl.hostname || "127.0.0.1",
    parsePort(dbUrl.protocol, dbUrl.port),
  );

  result.deepChecks.dbUrl = pluginConfig.dbUrl;
  result.deepChecks.dbReachable = dbReachable;

  if (dbReachable) {
    const userId =
      typeof pluginConfig.defaultUserId === "string" && pluginConfig.defaultUserId.trim()
        ? pluginConfig.defaultUserId.trim()
        : "openclaw-user";
    const statsRaw = run(openclawBin, ["memoria", "stats", "--user-id", userId]);
    const listRaw = run(openclawBin, ["ltm", "list", "--limit", "1", "--user-id", userId]);
    result.deepChecks.performed = true;
    result.deepChecks.userId = userId;
    result.deepChecks.stats = parseCommandJson(statsRaw);
    result.deepChecks.list = parseCommandJson(listRaw);
  } else {
    result.deepChecks.skipped = "Embedded database is not reachable; skipped stats/list verification.";
  }
} else {
  result.deepChecks.skipped = "Deep verification is only automatic for embedded mode.";
}

console.log(
  JSON.stringify(
    result,
    null,
    2,
  ),
);
