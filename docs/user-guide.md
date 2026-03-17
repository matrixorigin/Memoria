# User Guide

## How It Works

Memoria is a **headless memory backend**. It stores and retrieves memories for AI assistants and applications. There is no user registration UI — an admin creates users and issues API keys, then users (or their applications) authenticate with those keys.

```
Admin creates user → issues API key → user/app authenticates with key → store/retrieve memories
```

## Getting an API Key

Your platform admin creates your account:

```bash
curl -X POST https://memoria-host:8100/auth/keys \
  -H "Authorization: Bearer ADMIN_MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"user_id": "alice", "name": "alice-laptop"}'
```

The response includes a `raw_key` (shown only once). This is your API key.

You can have multiple keys (e.g., one per device). List them:
```bash
curl https://memoria-host:8100/auth/keys \
  -H "Authorization: Bearer sk-your-key..."
```

Get a single key's details (including `last_used_at`, `expires_at`):
```bash
curl https://memoria-host:8100/auth/keys/KEY_ID \
  -H "Authorization: Bearer sk-your-key..."
```

Rotate a key (revokes old, issues new with same name/expiry — atomic):
```bash
curl -X PUT https://memoria-host:8100/auth/keys/KEY_ID/rotate \
  -H "Authorization: Bearer sk-your-key..."
# Response includes new raw_key — save it immediately
```

Revoke a key:
```bash
curl -X DELETE https://memoria-host:8100/auth/keys/KEY_ID \
  -H "Authorization: Bearer sk-your-key..."
```

---

## For AI Assistants (MCP)

### Install

Download the latest binary from [GitHub Releases](https://github.com/matrixorigin/Memoria/releases):

```bash
# Linux (x86_64)
curl -LO https://github.com/matrixorigin/Memoria/releases/latest/download/memoria-x86_64-unknown-linux-gnu.tar.gz
tar xzf memoria-x86_64-unknown-linux-gnu.tar.gz
sudo mv memoria /usr/local/bin/

# macOS (Apple Silicon)
curl -LO https://github.com/matrixorigin/Memoria/releases/latest/download/memoria-aarch64-apple-darwin.tar.gz
tar xzf memoria-aarch64-apple-darwin.tar.gz
sudo mv memoria /usr/local/bin/
```

### Quick setup with `memoria init`

Run `memoria init --tool <name>` in your project directory to write the MCP config + steering rules:

```bash
cd your-project

# Embedded mode (direct DB)
memoria init --tool kiro --db-url "mysql+pymysql://root:111@localhost:6001/memoria"

# Remote mode (connect to existing server)
memoria init --tool kiro --api-url "https://memoria-host:8100" --token "sk-your-key..."

# With embedding config
memoria init --tool kiro --embedding-provider openai \
             --embedding-base-url https://api.siliconflow.cn/v1 \
             --embedding-api-key sk-... \
             --embedding-model BAAI/bge-m3 \
             --embedding-dim 1024
```

This creates MCP config + steering rules for your detected AI tool. Restart the tool afterwards.

For local (on-device) embedding without an API key, see the [Local Embedding Guide](local-embedding.md).

### Check status and update rules

```bash
memoria status          # show config files, rule versions, bundled version
memoria update-rules    # update steering rules to match current binary version
```

After upgrading the Memoria binary, run `memoria update-rules` to sync the steering rules, then restart your AI tool.

### MCP server modes

`memoria mcp` supports two modes:

**Embedded mode** — direct DB access, no separate server needed:
```bash
memoria mcp --db-url "mysql+pymysql://root:111@localhost:6001/memoria" --user alice
```

**Remote mode** — proxy to a deployed Memoria REST API:
```bash
memoria mcp --api-url "https://memoria-host:8100" --token "sk-your-key..."
```

All options:
```
--api-url   Memory service URL (enables remote mode)
--token     API key for remote mode
--db-url    Database URL for embedded mode (or set MEMORIA_DB_URL env var)
--user      Default user ID (default: "default")
--transport stdio | sse  (default: stdio)
```

### Manual MCP config (if not using `memoria init`)

**Kiro** — `.kiro/settings/mcp.json`:
```json
{
  "mcpServers": {
    "memoria": {
      "command": "memoria",
      "args": ["mcp", "--api-url", "https://memoria-host:8100", "--token", "sk-your-key..."]
    }
  }
}
```

**Cursor** — `.cursor/mcp.json`: same structure as above.

**Claude Desktop** — `claude_desktop_config.json`: same structure as above.

### Available MCP Tools

Once connected, your AI assistant can use these tools:

| Tool | Description |
|------|-------------|
| `memory_store` | Store a memory (fact, preference, decision) |
| `memory_retrieve` | Retrieve relevant memories for a query |
| `memory_search` | Semantic search over all memories |
| `memory_correct` | Correct an existing memory (by ID or semantic search) |
| `memory_purge` | Delete a memory or bulk-delete by topic |
| `memory_profile` | Get memory-derived profile summary |
| `memory_snapshot` | Create a named snapshot |
| `memory_snapshots` | List all snapshots |
| `memory_rollback` | Restore to a previous snapshot |
| `memory_branch` | Create an isolated memory branch |
| `memory_checkout` | Switch to a branch |
| `memory_diff` | Preview changes before merging a branch |
| `memory_merge` | Merge a branch back into main |
| `memory_branch_delete` | Delete a branch |
| `memory_branches` | List all branches |
| `memory_extract_entities` | Extract named entities and build entity graph (proactive) |
| `memory_link_entities` | Write entity links from your own extraction results |
| `memory_consolidate` | Detect contradictions, fix orphans (30min cooldown) |
| `memory_reflect` | Synthesize insights from memory clusters (2h cooldown) |
| `memory_governance` | Clean stale/low-confidence memories (1h cooldown) |
| `memory_rebuild_index` | Rebuild vector index (only when governance reports needed) |

---

## For Applications (REST API)

Standard HTTP with Bearer token auth. Works with any language.

### Python

```python
import httpx

client = httpx.Client(
    base_url="https://memoria-host:8100",
    headers={"Authorization": "Bearer sk-your-key..."},
)

# Store a memory
client.post("/v1/memories", json={
    "content": "User prefers dark mode",
    "memory_type": "profile",
})

# Retrieve relevant memories
memories = client.post("/v1/memories/retrieve", json={
    "query": "UI preferences",
    "top_k": 5,
}).json()

# Batch store
client.post("/v1/memories/batch", json={
    "memories": [
        {"content": "Project uses React 18"},
        {"content": "Deployment target is AWS ECS"},
    ]
})

# Correct a memory by ID
client.put("/v1/memories/MEMORY_ID/correct", json={
    "new_content": "User prefers light mode now",
    "reason": "User changed preference",
})

# Correct a memory by semantic search (no ID needed)
client.post("/v1/memories/correct", json={
    "query": "UI mode preference",
    "new_content": "User prefers light mode now",
    "reason": "User changed preference",
})

# Delete a memory
client.delete("/v1/memories/MEMORY_ID")

# Create a snapshot
client.post("/v1/snapshots", json={
    "name": "before-migration",
    "description": "Snapshot before DB migration",
})

# Compare snapshot with current state
diff = client.get("/v1/snapshots/before-migration/diff").json()
```

### JavaScript

```javascript
const API = "https://memoria-host:8100";
const headers = {
  "Authorization": "Bearer sk-your-key...",
  "Content-Type": "application/json",
};

// Store
await fetch(`${API}/v1/memories`, {
  method: "POST", headers,
  body: JSON.stringify({ content: "User prefers dark mode" }),
});

// Retrieve
const res = await fetch(`${API}/v1/memories/retrieve`, {
  method: "POST", headers,
  body: JSON.stringify({ query: "UI preferences" }),
});
const memories = await res.json();
```

### cURL

```bash
# Store
curl -X POST https://memoria-host:8100/v1/memories \
  -H "Authorization: Bearer sk-your-key..." \
  -H "Content-Type: application/json" \
  -d '{"content": "User prefers Python", "memory_type": "profile"}'

# Retrieve
curl -X POST https://memoria-host:8100/v1/memories/retrieve \
  -H "Authorization: Bearer sk-your-key..." \
  -H "Content-Type: application/json" \
  -d '{"query": "programming language"}'

# List memories (cursor pagination)
curl "https://memoria-host:8100/v1/memories?limit=20" \
  -H "Authorization: Bearer sk-your-key..."

# Profile
curl https://memoria-host:8100/v1/profiles/me \
  -H "Authorization: Bearer sk-your-key..."
```

---

## Memory Types

| Type | Use Case | Example |
|------|----------|---------|
| `semantic` | Facts, decisions, architecture choices (default) | "Project uses PostgreSQL 15" |
| `profile` | User/agent preferences | "User prefers concise responses" |
| `procedural` | How-to knowledge, workflows | "Deploy by running make deploy" |
| `working` | Temporary context for current task | "Currently debugging auth module" |
| `tool_result` | Results from tool executions | "Last test run: 94 passed" |

---

## Enterprise Integration

Memoria is designed as a headless backend — your platform handles user identity, Memoria handles memory.

### Integration Flow

```
┌─────────────┐     ┌──────────────┐     ┌──────────────┐
│  Your SSO   │────▶│  Your App    │────▶│  Memoria    │
│  (LDAP/OIDC)│     │  Backend     │     │  API         │
└─────────────┘     └──────────────┘     └──────────────┘
                         │                      │
                    1. User logs in         3. Store/retrieve
                    2. Map to Memoria         memories
                       user_id
```

### User Provisioning

Your backend calls this once per new user:

```python
def provision_memoria_user(internal_user_id: str, display_name: str) -> str:
    """Create Memoria user and return API key."""
    resp = httpx.post(
        f"{MEMORIA_URL}/auth/keys",
        headers={"Authorization": f"Bearer {MASTER_KEY}"},
        json={"user_id": internal_user_id, "name": display_name},
    )
    return resp.json()["raw_key"]  # Store this in your user DB
```

### Multi-Tenant Isolation

Every API query is automatically scoped to the `user_id` derived from the API key. Users cannot access each other's memories. No additional configuration needed.

### SaaS Platform Pattern

```python
# Your SaaS backend — proxy pattern
@app.post("/api/memories")
def store_memory(request, current_user):
    memoria_key = get_memoria_key(current_user.id)
    resp = httpx.post(
        f"{MEMORIA_URL}/v1/memories",
        headers={"Authorization": f"Bearer {memoria_key}"},
        json=request.json(),
    )
    return resp.json()
```

Or give the API key directly to the client (e.g., for MCP configuration) if your threat model allows it.
