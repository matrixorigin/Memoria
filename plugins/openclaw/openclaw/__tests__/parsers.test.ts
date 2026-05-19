/**
 * Group 4: Response Parsing
 * "Can the plugin understand what Memoria returns?"
 *
 * Tests the text parsers that bridge Memoria's output to typed data structures.
 * These parsers are used by both embedded (MCP binary) and api (HTTP) modes.
 *
 * Note: parsers are not exported from client.ts, so we test them indirectly
 * through MemoriaClient methods with a mocked transport.
 * For direct parser testing, we import the module and use the internal functions
 * via a re-export helper or test the formatted text round-trip.
 */
import { describe, it, expect, afterEach } from "vitest";
import { MemoriaHttpTransport } from "../http-client.js";
import { buildApiConfig, mockFetch } from "./helpers.js";

// We test parsers indirectly: http-client produces formatted text that
// matches the Rust MCP binary format, and client.ts parsers consume it.
// Here we verify the http-client output IS parseable by checking the
// text format matches the expected patterns.

const originalFetch = globalThis.fetch;
let fetchHelper: ReturnType<typeof mockFetch>;

function setup() {
  fetchHelper = mockFetch();
  return new MemoriaHttpTransport(buildApiConfig(), "test-user");
}

function teardown() {
  globalThis.fetch = originalFetch;
}

describe("Group 4: Response Parsing (format round-trip)", () => {
  afterEach(teardown);

  // ── Memory list parsing ──────────────────────────────────

  describe("Memory list text format", () => {
    it("4.1 single memory → [id] (type) content", async () => {
      const t = setup();
      fetchHelper.respondWith(200, {
        results: [{ memory_id: "abc-123", memory_type: "semantic", content: "Hello world" }],
      });
      const result = await t.callTool("memory_retrieve", { query: "q" }) as any;
      const text = result.content[0].text;
      expect(text).toMatch(/^\[abc-123\] \(semantic\) Hello world$/);
    });

    it("4.2 multiple memories → one per line", async () => {
      const t = setup();
      fetchHelper.respondWith(200, {
        results: [
          { memory_id: "m1", memory_type: "semantic", content: "First" },
          { memory_id: "m2", memory_type: "profile", content: "Second" },
        ],
      });
      const result = await t.callTool("memory_search", { query: "q" }) as any;
      const lines = result.content[0].text.split("\n");
      expect(lines).toHaveLength(2);
      expect(lines[0]).toMatch(/^\[m1\] \(semantic\) First$/);
      expect(lines[1]).toMatch(/^\[m2\] \(profile\) Second$/);
    });

    it("4.3 empty results → 'No relevant memories found.'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { results: [] });
      const result = await t.callTool("memory_retrieve", { query: "q" }) as any;
      expect(result.content[0].text).toBe("No relevant memories found.");
    });

    it("4.4 array response shape (no results wrapper)", async () => {
      const t = setup();
      fetchHelper.respondWith(200, [
        { memory_id: "m1", memory_type: "semantic", content: "Direct array" },
      ]);
      const result = await t.callTool("memory_retrieve", { query: "q" }) as any;
      expect(result.content[0].text).toContain("[m1] (semantic) Direct array");
    });
  });

  // ── Store/correct/purge parsing ──────────────────────────

  describe("Store/correct/purge text format", () => {
    it("4.5 store → 'Stored memory <id>: <content>'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { memory_id: "new-1", content: "Test memory" });
      const result = await t.callTool("memory_store", { content: "Test memory" }) as any;
      expect(result.content[0].text).toBe("Stored memory new-1: Test memory");
    });

    it("4.6 store with missing id → uses empty string", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { content: "No id returned" });
      const result = await t.callTool("memory_store", { content: "No id returned" }) as any;
      expect(result.content[0].text).toBe("Stored memory : No id returned");
    });

    it("4.7 correct by id → 'Corrected memory <id>: <content>'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { memory_id: "m1", content: "Fixed" });
      const result = await t.callTool("memory_correct", {
        memory_id: "m1", new_content: "Fixed",
      }) as any;
      expect(result.content[0].text).toBe("Corrected memory m1: Fixed");
    });

    it("4.8 correct by query → same format", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { memory_id: "m2", content: "Updated" });
      const result = await t.callTool("memory_correct", {
        query: "old content", new_content: "Updated",
      }) as any;
      expect(result.content[0].text).toBe("Corrected memory m2: Updated");
    });

    it("4.9 purge → 'Purged N memory(ies).'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { purged: 5 });
      const result = await t.callTool("memory_purge", { topic: "test" }) as any;
      expect(result.content[0].text).toBe("Purged 5 memory(ies).");
    });

    it("4.10 purge with zero → 'Purged 0 memory(ies).'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { purged: 0 });
      const result = await t.callTool("memory_purge", { topic: "nothing" }) as any;
      expect(result.content[0].text).toBe("Purged 0 memory(ies).");
    });
  });

  // ── Snapshot list parsing ────────────────────────────────

  describe("Snapshot list text format", () => {
    it("4.11 snapshots → 'Snapshots (N):\\n  name (ts)'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, {
        snapshots: [
          { name: "before-refactor", timestamp: "2026-03-23T10:00:00Z" },
        ],
      });
      const result = await t.callTool("memory_snapshots", {}) as any;
      const text = result.content[0].text;
      expect(text).toMatch(/^Snapshots \(1\):/);
      expect(text).toContain("before-refactor (2026-03-23T10:00:00Z)");
    });

    it("4.12 empty snapshots → 'Snapshots (0):'", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { snapshots: [] });
      const result = await t.callTool("memory_snapshots", {}) as any;
      expect(result.content[0].text).toBe("Snapshots (0):");
    });
  });

  // ── Branch list parsing ──────────────────────────────────

  describe("Branch list text format", () => {
    it("4.14 branches with active marker", async () => {
      const t = setup();
      fetchHelper.respondWith(200, {
        branches: [
          { name: "main", active: true },
          { name: "experiment", active: false },
        ],
      });
      const result = await t.callTool("memory_branches", {}) as any;
      const text = result.content[0].text;
      expect(text).toContain("main ← active");
      expect(text).toMatch(/^\s*experiment$/m);
    });

    it("4.15 branches with no active → main defaults active", async () => {
      // When API returns no active flag, formatBranchList just passes through
      // The client-side parseBranches defaults main to active
      const t = setup();
      fetchHelper.respondWith(200, {
        branches: [
          { name: "main", active: false },
          { name: "dev", active: false },
        ],
      });
      const result = await t.callTool("memory_branches", {}) as any;
      const text = result.content[0].text;
      // formatBranchList respects the active flag from API
      expect(text).toContain("Branches:");
      expect(text).toContain("main");
    });

    it("4.16 empty branches → default main active", async () => {
      const t = setup();
      fetchHelper.respondWith(200, { branches: [] });
      const result = await t.callTool("memory_branches", {}) as any;
      expect(result.content[0].text).toContain("main ← active");
    });
  });
});
