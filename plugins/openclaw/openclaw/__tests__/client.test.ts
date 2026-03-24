/**
 * Group 5: Session & Backend Routing
 * "Does the right transport get used?"
 */
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { MemoriaClient } from "../client.js";
import { MemoriaHttpTransport } from "../http-client.js";
import { buildApiConfig, buildEmbeddedConfig, mockFetch } from "./helpers.js";

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

describe("Group 5: Session & Backend Routing", () => {
  it("5.1 getSession with backend=api returns MemoriaHttpTransport", async () => {
    const fetchMock = mockFetch();
    fetchMock.respondWith(200, { results: [] });
    const client = new MemoriaClient(buildApiConfig());
    // Trigger a call to force session creation
    await client.retrieve({ userId: "u1", query: "test", topK: 5 });
    // Verify it used fetch (HTTP transport), not spawn (MCP session)
    expect(fetchMock.calls.length).toBeGreaterThan(0);
    expect(fetchMock.lastCall()!.url).toContain("/v1/memories/retrieve");
    client.close();
  });

  // 5.2 is hard to test without spawning a real binary — skip for unit tests

  it("5.3 same userId reuses session (no extra fetch for session setup)", async () => {
    const fetchMock = mockFetch();
    fetchMock.respondWith(200, { results: [] });
    const client = new MemoriaClient(buildApiConfig());
    await client.retrieve({ userId: "u1", query: "q1", topK: 5 });
    const callCount1 = fetchMock.calls.length;
    await client.retrieve({ userId: "u1", query: "q2", topK: 5 });
    const callCount2 = fetchMock.calls.length;
    // Second call should add exactly 1 more fetch (the retrieve), not 2 (no session setup)
    expect(callCount2 - callCount1).toBe(1);
    client.close();
  });

  it("5.4 rebuildIndex in api mode returns cloud-managed message", async () => {
    const fetchMock = mockFetch();
    const client = new MemoriaClient(buildApiConfig());
    const result = await client.rebuildIndex("mem_memories");
    expect(result.message).toMatch(/cloud/i);
    // No HTTP call should have been made
    expect(fetchMock.calls.length).toBe(0);
    client.close();
  });

  it("5.5 health in api mode calls healthCheck on transport", async () => {
    const fetchMock = mockFetch();
    fetchMock.respondWith(200, { status: "ok", instance_id: "i-1", db: true });
    const client = new MemoriaClient(buildApiConfig());
    const result = await client.health("u1");
    expect(result.status).toBe("ok");
    expect(result.mode).toBe("api");
    expect(fetchMock.lastCall()!.url).toContain("/health/instance");
    client.close();
  });

  it("5.6 close() does not throw", () => {
    const client = new MemoriaClient(buildApiConfig());
    expect(() => client.close()).not.toThrow();
  });
});
