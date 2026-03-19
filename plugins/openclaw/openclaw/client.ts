import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface, type Interface as ReadlineInterface } from "node:readline";
import type {
  MemoriaMemoryType,
  MemoriaPluginConfig,
  MemoriaTrustTier,
} from "./config.js";
import { MEMORIA_MEMORY_TYPES } from "./config.js";

export type MemoriaMemoryRecord = {
  memory_id: string;
  content: string;
  memory_type?: string;
  trust_tier?: string | null;
  confidence?: number | null;
  session_id?: string | null;
  is_active?: boolean;
  observed_at?: string | null;
  updated_at?: string | null;
};

export type MemoriaProfileResponse = {
  user_id: string;
  profile: string | null;
  stats?: Record<string, unknown>;
};

export type MemoriaBranchRecord = {
  name: string;
  branch_db?: string;
  active?: boolean;
};

export type MemoriaSnapshotSummary = {
  name: string;
  snapshot_name: string;
  description?: string | null;
  timestamp: string;
};

export type MemoriaListMemoriesResponse = {
  items: MemoriaMemoryRecord[];
  count: number;
  user_id: string;
  backend: string;
  partial?: boolean;
  include_inactive?: boolean;
  limitations?: string[];
};

export type MemoriaStatsResponse = {
  backend: string;
  user_id: string;
  activeMemoryCount: number;
  inactiveMemoryCount: number | null;
  byType: Record<string, number>;
  entityCount: number | null;
  snapshotCount: number | null;
  branchCount: number | null;
  healthWarnings: string[];
  partial?: boolean;
  limitations?: string[];
};

type JsonRpcError = {
  code?: number;
  message?: string;
};

type JsonRpcResponse = {
  id?: number | null;
  result?: unknown;
  error?: JsonRpcError;
};

type PendingRequest = {
  reject: (error: Error) => void;
  resolve: (value: unknown) => void;
  timer: NodeJS.Timeout;
};

type McpContentBlock = {
  type?: string;
  text?: string;
};

const MCP_PROTOCOL_VERSION = "2024-11-05";
const PLUGIN_VERSION = "0.3.0";
const MEMORY_LINE_RE = /^\[([^\]]+)\] \(([^)]+)\) ?([\s\S]*)$/;

function asRecord(value: unknown): Record<string, unknown> | null {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function tryParseJson(raw: string): unknown {
  try {
    return JSON.parse(raw);
  } catch {
    return undefined;
  }
}

function normalizeMemoryRecord(value: Partial<MemoriaMemoryRecord> & Record<string, unknown>) {
  return {
    memory_id: typeof value.memory_id === "string" ? value.memory_id : "",
    content: typeof value.content === "string" ? value.content : "",
    memory_type:
      typeof value.memory_type === "string"
        ? value.memory_type
        : typeof value.type === "string"
          ? value.type
          : undefined,
    trust_tier:
      typeof value.trust_tier === "string" || value.trust_tier === null
        ? (value.trust_tier as string | null)
        : undefined,
    confidence:
      typeof value.confidence === "number" && Number.isFinite(value.confidence)
        ? value.confidence
        : null,
    session_id:
      typeof value.session_id === "string" || value.session_id === null
        ? (value.session_id as string | null)
        : undefined,
    is_active: typeof value.is_active === "boolean" ? value.is_active : true,
    observed_at:
      typeof value.observed_at === "string" || value.observed_at === null
        ? (value.observed_at as string | null)
        : undefined,
    updated_at:
      typeof value.updated_at === "string" || value.updated_at === null
        ? (value.updated_at as string | null)
        : undefined,
  };
}

function normalizeTypeCounts(value: unknown): Record<string, number> {
  const counts = Object.fromEntries(MEMORIA_MEMORY_TYPES.map((type) => [type, 0])) as Record<
    string,
    number
  >;
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return counts;
  }
  for (const [key, raw] of Object.entries(value)) {
    if (typeof raw === "number" && Number.isFinite(raw)) {
      counts[key] = raw;
    }
  }
  return counts;
}

function extractToolText(result: unknown): string {
  const record = asRecord(result);
  if (!record) {
    return typeof result === "string" ? result.trim() : "";
  }

  const content = record.content;
  if (!Array.isArray(content)) {
    return typeof record.text === "string" ? record.text.trim() : "";
  }

  const parts: string[] = [];
  for (const entry of content) {
    const block = entry as McpContentBlock;
    if (block?.type === "text" && typeof block.text === "string") {
      parts.push(block.text);
    }
  }
  return parts.join("\n").trim();
}

function parseMemoryTextList(text: string): MemoriaMemoryRecord[] {
  const trimmed = text.trim();
  if (!trimmed || trimmed.startsWith("No relevant memories found.") || trimmed === "No memories found.") {
    return [];
  }

  const memories: MemoriaMemoryRecord[] = [];
  let current: MemoriaMemoryRecord | null = null;

  for (const line of trimmed.split(/\r?\n/)) {
    const match = line.match(MEMORY_LINE_RE);
    if (match) {
      if (current) {
        memories.push(normalizeMemoryRecord(current));
      }
      current = {
        memory_id: match[1].trim(),
        memory_type: match[2].trim(),
        content: match[3] ?? "",
      };
      continue;
    }

    if (current) {
      current.content = current.content ? `${current.content}\n${line}` : line;
    }
  }

  if (current) {
    memories.push(normalizeMemoryRecord(current));
  }

  return memories;
}

function parseStoredMemory(text: string, fallback: {
  content: string;
  memoryType: MemoriaMemoryType;
  trustTier?: MemoriaTrustTier;
  sessionId?: string;
}): MemoriaMemoryRecord {
  const match = text.match(/^Stored memory ([^:]+):\s*([\s\S]*)$/);
  return normalizeMemoryRecord({
    memory_id: match?.[1]?.trim() ?? "",
    content: match?.[2] ?? fallback.content,
    memory_type: fallback.memoryType,
    trust_tier: fallback.trustTier ?? null,
    session_id: fallback.sessionId ?? null,
  });
}

function parseCorrectedMemory(text: string, fallbackContent: string) {
  const match = text.match(/^Corrected memory ([^:]+):\s*([\s\S]*)$/);
  if (!match) {
    return null;
  }
  return normalizeMemoryRecord({
    memory_id: match[1].trim(),
    content: match[2] || fallbackContent,
  });
}

function parsePurgedCount(text: string): number {
  const match = text.match(/Purged (\d+) memory/);
  return match ? Number.parseInt(match[1], 10) : 0;
}

function parseSnapshotList(text: string): MemoriaSnapshotSummary[] {
  const lines = text.trim().split(/\r?\n/);
  if (lines.length === 0 || !lines[0].startsWith("Snapshots (")) {
    return [];
  }

  const snapshots: MemoriaSnapshotSummary[] = [];
  for (const line of lines.slice(1)) {
    const trimmed = line.trim();
    if (!trimmed) {
      continue;
    }
    const splitIndex = trimmed.lastIndexOf(" (");
    if (splitIndex <= 0 || !trimmed.endsWith(")")) {
      continue;
    }
    const name = trimmed.slice(0, splitIndex);
    const timestamp = trimmed.slice(splitIndex + 2, -1);
    snapshots.push({
      name,
      snapshot_name: name,
      timestamp,
    });
  }
  return snapshots;
}

function parseSnapshotCreated(text: string, name: string): MemoriaSnapshotSummary {
  const match = text.match(/^Snapshot '(.+)' created at (.+)$/);
  return {
    name: match?.[1] ?? name,
    snapshot_name: match?.[1] ?? name,
    timestamp: match?.[2] ?? "",
  };
}

function parseBranches(text: string): MemoriaBranchRecord[] {
  const lines = text.trim().split(/\r?\n/);
  if (lines.length === 0 || lines[0] !== "Branches:") {
    return [];
  }

  const branches: MemoriaBranchRecord[] = [];
  let explicitActive = false;

  for (const line of lines.slice(1)) {
    const trimmed = line.trim();
    if (!trimmed) {
      continue;
    }
    const active = trimmed.endsWith(" ← active");
    const name = active ? trimmed.slice(0, -" ← active".length).trim() : trimmed;
    explicitActive ||= active;
    branches.push({ name, active });
  }

  if (!explicitActive) {
    const main = branches.find((branch) => branch.name === "main");
    if (main) {
      main.active = true;
    }
  }

  return branches;
}

function parseJsonText(text: string): Record<string, unknown> | null {
  const parsed = tryParseJson(text);
  return asRecord(parsed);
}

function parseGenericResult(text: string): Record<string, unknown> {
  return parseJsonText(text) ?? { message: text };
}

class MemoriaMcpSession {
  private child: ChildProcessWithoutNullStreams | null = null;
  private stdoutReader: ReadlineInterface | null = null;
  private initialized: Promise<void> | null = null;
  private nextId = 1;
  private readonly pending = new Map<number, PendingRequest>();
  private readonly stderrLines: string[] = [];

  constructor(
    private readonly config: MemoriaPluginConfig,
    private readonly userId: string,
  ) {}

  isAlive(): boolean {
    return Boolean(this.child && this.child.exitCode === null && !this.child.killed);
  }

  async callTool(name: string, args: Record<string, unknown>): Promise<unknown> {
    await this.ensureInitialized();
    return this.request("tools/call", {
      name,
      arguments: args,
    });
  }

  close() {
    this.stdoutReader?.close();
    this.stdoutReader = null;
    if (this.child && this.child.exitCode === null && !this.child.killed) {
      this.child.kill("SIGTERM");
    }
    this.failPending(new Error("Memoria MCP session closed."));
    this.child = null;
    this.initialized = null;
  }

  private async ensureInitialized(): Promise<void> {
    if (this.isAlive() && this.initialized) {
      return this.initialized;
    }
    this.initialized = this.start();
    try {
      await this.initialized;
    } catch (error) {
      this.initialized = null;
      throw error;
    }
  }

  private async start(): Promise<void> {
    const child = spawn(this.config.memoriaExecutable, this.buildArgs(), {
      cwd: process.cwd(),
      env: this.buildEnv(),
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.child = child;

    this.stdoutReader = createInterface({ input: child.stdout });
    this.stdoutReader.on("line", (line) => {
      this.handleStdout(line);
    });

    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk: string) => {
      for (const line of chunk.split(/\r?\n/)) {
        const trimmed = line.trim();
        if (!trimmed) {
          continue;
        }
        this.stderrLines.push(trimmed);
        if (this.stderrLines.length > 20) {
          this.stderrLines.shift();
        }
      }
    });

    child.on("error", (error) => {
      this.failPending(
        new Error(`Failed to start memoria executable '${this.config.memoriaExecutable}': ${error.message}`),
      );
      this.child = null;
      this.initialized = null;
    });

    child.on("close", (code, signal) => {
      this.stdoutReader?.close();
      this.stdoutReader = null;
      const tail = this.stderrLines.length > 0 ? ` stderr: ${this.stderrLines.join(" | ")}` : "";
      this.failPending(
        new Error(
          `Memoria MCP exited for user '${this.userId}' (code=${String(code)} signal=${String(signal)}).${tail}`,
        ),
      );
      this.child = null;
      this.initialized = null;
    });

    await this.request("initialize", {
      protocolVersion: MCP_PROTOCOL_VERSION,
      capabilities: {},
      clientInfo: {
        name: "openclaw-memoria",
        version: PLUGIN_VERSION,
      },
    });
    this.notify("notifications/initialized");
  }

  private buildArgs(): string[] {
    const args = ["mcp"];
    if (this.config.backend === "http") {
      args.push("--api-url", this.config.apiUrl!);
      args.push("--token", this.config.apiKey!);
      args.push("--user", this.userId);
      return args;
    }

    args.push("--db-url", this.config.dbUrl);
    args.push("--user", this.userId);
    return args;
  }

  private buildEnv(): NodeJS.ProcessEnv {
    const env: NodeJS.ProcessEnv = { ...process.env };
    if (this.config.backend === "embedded") {
      // Embedded mode must not inherit remote-mode overrides from the shell.
      delete env.MEMORIA_API_URL;
      delete env.MEMORIA_TOKEN;
      env.EMBEDDING_PROVIDER = this.config.embeddingProvider;
      env.EMBEDDING_MODEL = this.config.embeddingModel;
      if (this.config.embeddingBaseUrl) {
        env.EMBEDDING_BASE_URL = this.config.embeddingBaseUrl;
      }
      if (this.config.embeddingApiKey) {
        env.EMBEDDING_API_KEY = this.config.embeddingApiKey;
      }
      if (typeof this.config.embeddingDim === "number") {
        env.EMBEDDING_DIM = String(this.config.embeddingDim);
      }
      if (this.config.llmApiKey) {
        env.LLM_API_KEY = this.config.llmApiKey;
      }
      if (this.config.llmBaseUrl) {
        env.LLM_BASE_URL = this.config.llmBaseUrl;
      }
      if (this.config.llmModel) {
        env.LLM_MODEL = this.config.llmModel;
      }
    }
    return env;
  }

  private handleStdout(rawLine: string) {
    const line = rawLine.trim();
    if (!line) {
      return;
    }

    const parsed = tryParseJson(line);
    const response = asRecord(parsed) as JsonRpcResponse | null;
    if (!response || typeof response.id !== "number") {
      return;
    }

    const pending = this.pending.get(response.id);
    if (!pending) {
      return;
    }

    this.pending.delete(response.id);
    clearTimeout(pending.timer);

    if (response.error?.message) {
      pending.reject(new Error(response.error.message));
      return;
    }

    pending.resolve(response.result);
  }

  private notify(method: string, params?: Record<string, unknown>) {
    if (!this.child) {
      return;
    }
    this.child.stdin.write(
      `${JSON.stringify({
        jsonrpc: "2.0",
        method,
        ...(params ? { params } : {}),
      })}\n`,
    );
  }

  private request(method: string, params?: Record<string, unknown>): Promise<unknown> {
    if (!this.child) {
      return Promise.reject(new Error("Memoria MCP process is not running."));
    }

    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Memoria MCP request timed out after ${this.config.timeoutMs}ms: ${method}`));
        this.close();
      }, this.config.timeoutMs);

      this.pending.set(id, { resolve, reject, timer });
      this.child!.stdin.write(
        `${JSON.stringify({
          jsonrpc: "2.0",
          id,
          method,
          ...(params ? { params } : {}),
        })}\n`,
      );
    });
  }

  private failPending(error: Error) {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pending.clear();
  }
}

export class MemoriaClient {
  private readonly sessions = new Map<string, MemoriaMcpSession>();
  private readonly memoryCache = new Map<string, MemoriaMemoryRecord>();

  constructor(private readonly config: MemoriaPluginConfig) {}

  close() {
    for (const session of this.sessions.values()) {
      session.close();
    }
    this.sessions.clear();
  }

  async health(userId: string) {
    await this.callToolText(userId, "memory_list", { limit: 1 });
    return {
      status: "ok",
      mode: this.config.backend,
      warnings: [],
    };
  }

  async storeMemory(params: {
    userId: string;
    content: string;
    memoryType: MemoriaMemoryType;
    trustTier?: MemoriaTrustTier;
    sessionId?: string;
    source?: string;
  }) {
    const text = await this.callToolText(params.userId, "memory_store", {
      content: params.content,
      memory_type: params.memoryType,
      session_id: params.sessionId,
      trust_tier: params.trustTier,
    });
    const record = parseStoredMemory(text, params);
    this.cacheMemories(params.userId, [record]);
    return record;
  }

  async retrieve(params: {
    userId: string;
    query: string;
    topK: number;
    memoryTypes?: MemoriaMemoryType[];
    sessionId?: string;
    includeCrossSession?: boolean;
  }) {
    const text = await this.callToolText(params.userId, "memory_retrieve", {
      query: params.query,
      top_k: params.topK,
      session_id: params.sessionId,
    });
    const memories = parseMemoryTextList(text);
    this.cacheMemories(params.userId, memories);
    return memories;
  }

  async search(params: {
    userId: string;
    query: string;
    topK: number;
  }) {
    const text = await this.callToolText(params.userId, "memory_search", {
      query: params.query,
      top_k: params.topK,
    });
    const memories = parseMemoryTextList(text);
    this.cacheMemories(params.userId, memories);
    return memories;
  }

  async getMemory(params: {
    userId: string;
    memoryId: string;
  }) {
    const cached = this.memoryCache.get(this.memoryCacheKey(params.userId, params.memoryId));
    if (cached) {
      return cached;
    }

    const scanLimit = Math.min(2000, Math.max(200, this.config.maxListPages * 50));
    const listed = await this.listMemories({
      userId: params.userId,
      limit: scanLimit,
    });
    return (
      listed.items.find((memory) => memory.memory_id === params.memoryId) ?? null
    );
  }

  async listMemories(params: {
    userId: string;
    memoryType?: MemoriaMemoryType;
    limit: number;
    sessionId?: string;
    includeInactive?: boolean;
  }): Promise<MemoriaListMemoriesResponse> {
    const needsClientFiltering = Boolean(params.memoryType);
    const scanLimit = needsClientFiltering
      ? Math.min(2000, Math.max(params.limit, params.limit * this.config.maxListPages))
      : params.limit;

    const text = await this.callToolText(params.userId, "memory_list", {
      limit: scanLimit,
    });
    let items = parseMemoryTextList(text);
    this.cacheMemories(params.userId, items);

    const limitations: string[] = [];
    if (params.memoryType) {
      items = items.filter((memory) => memory.memory_type === params.memoryType);
      limitations.push("memoryType filtering is applied client-side from a bounded Rust MCP scan.");
    }
    if (params.sessionId) {
      limitations.push("Rust Memoria MCP does not expose session_id on memory_list; sessionId was ignored.");
    }
    if (params.includeInactive) {
      limitations.push("Rust Memoria MCP only lists active memories.");
    }

    const partial = limitations.length > 0 || items.length >= scanLimit;

    return {
      items: items.slice(0, params.limit),
      count: Math.min(items.length, params.limit),
      user_id: params.userId,
      backend: this.config.backend,
      partial,
      include_inactive: params.includeInactive ?? false,
      ...(limitations.length > 0 ? { limitations } : {}),
    };
  }

  async stats(userId: string): Promise<MemoriaStatsResponse> {
    const scanLimit = Math.min(2000, Math.max(200, this.config.maxListPages * 50));
    const list = await this.listMemories({ userId, limit: scanLimit });
    const byType = normalizeTypeCounts(undefined);
    for (const item of list.items) {
      const type = item.memory_type ?? "semantic";
      byType[type] = (byType[type] ?? 0) + 1;
    }

    const limitations = [
      ...(list.limitations ?? []),
      "Statistics are derived from Rust MCP text output. Inactive-memory and entity totals are unavailable.",
    ];

    let snapshotCount: number | null = null;
    try {
      snapshotCount = (await this.listSnapshots(userId)).length;
    } catch (error) {
      limitations.push(
        `Snapshot statistics unavailable: ${error instanceof Error ? error.message : String(error)}`,
      );
    }

    let branchCount: number | null = null;
    try {
      branchCount = (await this.branchList(userId)).length;
    } catch (error) {
      limitations.push(
        `Branch statistics unavailable: ${error instanceof Error ? error.message : String(error)}`,
      );
    }

    return {
      backend: this.config.backend,
      user_id: userId,
      activeMemoryCount: list.items.length,
      inactiveMemoryCount: null,
      byType,
      entityCount: null,
      snapshotCount,
      branchCount,
      healthWarnings: [],
      partial: true,
      limitations,
    };
  }

  async correctById(params: {
    userId: string;
    memoryId: string;
    newContent: string;
    reason?: string;
  }) {
    const text = await this.callToolText(params.userId, "memory_correct", {
      memory_id: params.memoryId,
      new_content: params.newContent,
      reason: params.reason ?? "",
    });
    const corrected = parseCorrectedMemory(text, params.newContent);
    if (corrected) {
      this.cacheMemories(params.userId, [corrected]);
      return corrected;
    }
    return { error: true, message: text };
  }

  async correctByQuery(params: {
    userId: string;
    query: string;
    newContent: string;
    reason?: string;
  }) {
    const text = await this.callToolText(params.userId, "memory_correct", {
      query: params.query,
      new_content: params.newContent,
      reason: params.reason ?? "",
    });
    const corrected = parseCorrectedMemory(text, params.newContent);
    if (corrected) {
      this.cacheMemories(params.userId, [corrected]);
      return corrected;
    }
    return { error: true, message: text };
  }

  async deleteMemory(params: {
    userId: string;
    memoryId: string;
    reason?: string;
  }) {
    const text = await this.callToolText(params.userId, "memory_purge", {
      memory_id: params.memoryId,
      reason: params.reason ?? "",
    });
    this.memoryCache.delete(this.memoryCacheKey(params.userId, params.memoryId));
    return { purged: parsePurgedCount(text) };
  }

  async purgeMemory(params: {
    userId: string;
    memoryId?: string;
    topic?: string;
    reason?: string;
  }) {
    const text = await this.callToolText(params.userId, "memory_purge", {
      memory_id: params.memoryId,
      topic: params.topic,
      reason: params.reason ?? "",
    });
    if (params.memoryId) {
      for (const memoryId of params.memoryId.split(",").map((entry) => entry.trim())) {
        if (memoryId) {
          this.memoryCache.delete(this.memoryCacheKey(params.userId, memoryId));
        }
      }
    }
    return { purged: parsePurgedCount(text), message: text };
  }

  async profile(userId: string): Promise<MemoriaProfileResponse> {
    const text = await this.callToolText(userId, "memory_profile", {});
    const profile = text === "No profile memories found." ? null : text;
    return {
      user_id: userId,
      profile,
    };
  }

  async governance(params: {
    userId: string;
    force?: boolean;
  }) {
    return parseGenericResult(
      await this.callToolText(params.userId, "memory_governance", {
        force: params.force ?? false,
      }),
    );
  }

  async consolidate(params: {
    userId: string;
    force?: boolean;
  }) {
    return parseGenericResult(
      await this.callToolText(params.userId, "memory_consolidate", {
        force: params.force ?? false,
      }),
    );
  }

  async reflect(params: {
    userId: string;
    force?: boolean;
    mode?: string;
  }) {
    return parseGenericResult(
      await this.callToolText(params.userId, "memory_reflect", {
        force: params.force ?? false,
        mode: params.mode ?? "auto",
      }),
    );
  }

  async extractEntities(params: {
    userId: string;
    force?: boolean;
    mode?: string;
  }) {
    return parseGenericResult(
      await this.callToolText(params.userId, "memory_extract_entities", {
        force: params.force ?? false,
        mode: params.mode ?? "auto",
      }),
    );
  }

  async linkEntities(params: {
    userId: string;
    entities: Array<Record<string, unknown>>;
  }) {
    return parseGenericResult(
      await this.callToolText(params.userId, "memory_link_entities", {
        entities: JSON.stringify(params.entities),
      }),
    );
  }

  async rebuildIndex(table: string) {
    return parseGenericResult(
      await this.callToolText(this.config.defaultUserId, "memory_rebuild_index", {
        table,
      }),
    );
  }

  async observe(params: {
    userId: string;
    messages: Array<{ role: string; content: string }>;
    sourceEventIds?: string[];
    sessionId?: string;
  }) {
    const text = await this.callToolText(params.userId, "memory_observe", {
      messages: params.messages,
      session_id: params.sessionId,
    });
    const payload = parseJsonText(text);
    const memories = Array.isArray(payload?.memories)
      ? payload.memories
          .map((entry) => asRecord(entry))
          .filter((entry): entry is Record<string, unknown> => Boolean(entry))
          .map((entry) => normalizeMemoryRecord(entry))
      : [];
    this.cacheMemories(params.userId, memories);
    return memories;
  }

  async createSnapshot(params: {
    userId: string;
    name: string;
    description?: string;
  }) {
    const text = await this.callToolText(params.userId, "memory_snapshot", {
      name: params.name,
      description: params.description ?? "",
    });
    return parseSnapshotCreated(text, params.name);
  }

  async listSnapshots(userId: string) {
    const text = await this.callToolText(userId, "memory_snapshots", {});
    return parseSnapshotList(text);
  }

  async rollbackSnapshot(params: {
    userId: string;
    name: string;
  }) {
    return parseGenericResult(
      await this.callToolText(params.userId, "memory_rollback", {
        name: params.name,
      }),
    );
  }

  async branchCreate(params: {
    userId: string;
    name: string;
    fromSnapshot?: string;
    fromTimestamp?: string;
  }) {
    return parseGenericResult(
      await this.callToolText(params.userId, "memory_branch", {
        name: params.name,
        from_snapshot: params.fromSnapshot,
        from_timestamp: params.fromTimestamp,
      }),
    );
  }

  async branchList(userId: string) {
    const text = await this.callToolText(userId, "memory_branches", {});
    return parseBranches(text);
  }

  async branchCheckout(params: {
    userId: string;
    name: string;
  }) {
    return parseGenericResult(
      await this.callToolText(params.userId, "memory_checkout", {
        name: params.name,
      }),
    );
  }

  async branchDelete(params: {
    userId: string;
    name: string;
  }) {
    return parseGenericResult(
      await this.callToolText(params.userId, "memory_branch_delete", {
        name: params.name,
      }),
    );
  }

  async branchMerge(params: {
    userId: string;
    source: string;
    strategy: string;
  }) {
    return parseGenericResult(
      await this.callToolText(params.userId, "memory_merge", {
        source: params.source,
        strategy: params.strategy,
      }),
    );
  }

  async branchDiff(params: {
    userId: string;
    source: string;
    limit: number;
  }) {
    return parseGenericResult(
      await this.callToolText(params.userId, "memory_diff", {
        source: params.source,
        limit: params.limit,
      }),
    );
  }

  private cacheMemories(userId: string, memories: MemoriaMemoryRecord[]) {
    for (const memory of memories) {
      if (!memory.memory_id) {
        continue;
      }
      this.memoryCache.set(this.memoryCacheKey(userId, memory.memory_id), memory);
    }
  }

  private memoryCacheKey(userId: string, memoryId: string) {
    return `${userId}::${memoryId}`;
  }

  private async callToolText(
    userId: string,
    name: string,
    args: Record<string, unknown>,
  ): Promise<string> {
    const session = this.getSession(userId);
    const result = await session.callTool(name, args);
    const text = extractToolText(result);
    if (text) {
      return text;
    }
    throw new Error(`Memoria tool '${name}' returned no text content.`);
  }

  private getSession(userId: string): MemoriaMcpSession {
    const key = `${this.config.backend}:${userId}`;
    const existing = this.sessions.get(key);
    if (existing?.isAlive()) {
      return existing;
    }
    existing?.close();
    const created = new MemoriaMcpSession(this.config, userId);
    this.sessions.set(key, created);
    return created;
  }
}
