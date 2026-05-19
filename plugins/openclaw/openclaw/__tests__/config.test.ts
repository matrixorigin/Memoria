/**
 * Group 1: Config & Onboarding
 * "Can a user configure the plugin correctly?"
 */
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import {
  parseMemoriaPluginConfig,
  memoriaPluginConfigSchema,
} from "../config.js";

describe("Group 1: Config & Onboarding", () => {
  const savedEnv = { ...process.env };
  afterEach(() => { process.env = { ...savedEnv }; });

  // ── Defaults ─────────────────────────────────────────────

  it("1.1 empty config returns valid defaults (backend=embedded)", () => {
    const config = parseMemoriaPluginConfig({});
    expect(config.backend).toBe("embedded");
    expect(config.defaultUserId).toBe("openclaw-user");
    expect(config.userIdStrategy).toBe("config");
    expect(config.autoRecall).toBe(true);
    expect(config.autoObserve).toBe(false);
    expect(config.timeoutMs).toBe(15_000);
    expect(config.retrieveTopK).toBe(5);
  });

  it("1.18 undefined input returns valid defaults", () => {
    const config = parseMemoriaPluginConfig(undefined);
    expect(config.backend).toBe("embedded");
  });

  // ── Valid api config ─────────────────────────────────────

  it("1.2 valid api config with apiUrl + apiKey", () => {
    const config = parseMemoriaPluginConfig({
      backend: "api",
      apiUrl: "https://memoria.example.com",
      apiKey: "sk-test-123",
    });
    expect(config.backend).toBe("api");
    expect(config.apiUrl).toBe("https://memoria.example.com");
    expect(config.apiKey).toBe("sk-test-123");
  });

  // ── "http" backend rejection ─────────────────────────────

  it("1.3 backend 'http' is rejected with clear error", () => {
    expect(() => parseMemoriaPluginConfig({ backend: "http" }))
      .toThrow(/no longer supported/i);
  });

  it("1.3b backend 'HTTP' (uppercase) is also rejected", () => {
    expect(() => parseMemoriaPluginConfig({ backend: "HTTP" }))
      .toThrow(/no longer supported/i);
  });

  // ── api mode validation ──────────────────────────────────

  it("1.4 api mode without apiKey throws even with default apiUrl", () => {
    // apiUrl has a default (127.0.0.1:8100), so omitting it doesn't error.
    // But apiKey has no default, so omitting it does.
    expect(() => parseMemoriaPluginConfig({ backend: "api" }))
      .toThrow(/apiKey/);
  });

  it("1.5 api mode with empty apiUrl falls back to default", () => {
    // readString returns default when value is empty/whitespace
    const config = parseMemoriaPluginConfig({
      backend: "api",
      apiUrl: "   ",
      apiKey: "sk-test",
    });
    expect(config.apiUrl).toBe("http://127.0.0.1:8100");
  });

  it("1.6 api mode with empty string apiKey throws", () => {
    expect(() => parseMemoriaPluginConfig({
      backend: "api",
      apiUrl: "https://example.com",
      apiKey: "   ",
    })).toThrow(/apiKey/);
  });

  // ── Unknown keys ─────────────────────────────────────────

  it("1.7 unknown config key rejected", () => {
    expect(() => parseMemoriaPluginConfig({ foo: "bar" }))
      .toThrow(/unknown config key/i);
  });

  // ── Env var interpolation ────────────────────────────────

  it("1.8 env var ${VAR} resolves from process.env", () => {
    process.env.TEST_MEMORIA_URL = "https://from-env.example.com";
    const config = parseMemoriaPluginConfig({
      backend: "api",
      apiUrl: "${TEST_MEMORIA_URL}",
      apiKey: "sk-test",
    });
    expect(config.apiUrl).toBe("https://from-env.example.com");
  });

  it("1.9 missing env var throws with var name", () => {
    delete process.env.NONEXISTENT_VAR_XYZ;
    expect(() => parseMemoriaPluginConfig({
      apiUrl: "${NONEXISTENT_VAR_XYZ}",
    })).toThrow(/NONEXISTENT_VAR_XYZ/);
  });

  // ── URL normalization ────────────────────────────────────

  it("1.10 trailing slashes stripped from apiUrl", () => {
    const config = parseMemoriaPluginConfig({
      backend: "api",
      apiUrl: "https://example.com///",
      apiKey: "sk-test",
    });
    expect(config.apiUrl).toBe("https://example.com");
  });

  it("1.11 dbUrl mysql+pymysql:// normalized to mysql://", () => {
    const config = parseMemoriaPluginConfig({
      dbUrl: "mysql+pymysql://root:pass@localhost:6001/memoria",
    });
    expect(config.dbUrl).toBe("mysql://root:pass@localhost:6001/memoria");
  });

  // ── Type/enum/range validation ───────────────────────────

  it("1.12 invalid enum value rejected with path", () => {
    expect(() => parseMemoriaPluginConfig({ backend: "magic" }))
      .toThrow(/backend/);
  });

  it("1.13 integer out of range rejected", () => {
    expect(() => parseMemoriaPluginConfig({ timeoutMs: 500 }))
      .toThrow(/timeoutMs/);
  });

  it("1.14 wrong type rejected with path", () => {
    expect(() => parseMemoriaPluginConfig({ autoRecall: "yes" }))
      .toThrow(/autoRecall/);
  });

  // ── safeParse ────────────────────────────────────────────

  it("1.15 safeParse success path", () => {
    const result = memoriaPluginConfigSchema.safeParse({});
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.backend).toBe("embedded");
    }
  });

  it("1.16 safeParse failure path (no throw)", () => {
    const result = memoriaPluginConfigSchema.safeParse({ backend: "magic" });
    expect(result.success).toBe(false);
    if (!result.success) {
      expect(result.error.issues.length).toBeGreaterThan(0);
      expect(result.error.issues[0].message).toMatch(/backend/);
    }
  });

  // ── Legacy keys ──────────────────────────────────────────

  it("1.17 legacy keys pythonExecutable and memoriaRoot accepted", () => {
    expect(() => parseMemoriaPluginConfig({
      pythonExecutable: "/usr/bin/python3",
      memoriaRoot: "/opt/memoria",
    })).not.toThrow();
  });
});
