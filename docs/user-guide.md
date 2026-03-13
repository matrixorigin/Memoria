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

Install the package, then add Memoria as an MCP server in your assistant's config.

### Install

```bash
pip install mo-memoria

# Only needed if using local embedding model (no external API):
pip install "mo-memoria[local-embedding]"    # Local sentence-transformers (~900MB download)

# If no NVIDIA GPU available, install CPU-only PyTorch first to avoid large CUDA dependencies:
pip install torch --index-url https://download.pytorch.org/whl/cpu
pip install "mo-memoria[local-embedding]"
```

### Start the MCP server

`memoria-mcp` supports two modes selected by whether `--api-url` is provided:

**Embedded mode** — direct DB access, no separate server needed:
```bash
memoria-mcp --db-url "mysql+pymysql://root:111@localhost:6001/memoria" --user alice
```

**Remote mode** — proxy to a deployed Memoria REST API:
```bash
memoria-mcp --api-url "https://memoria-host:8100" --token "sk-your-key..."
```

Remote mode supports all tools available in embedded mode. The actual tool set depends on the server tier — shared instances may have fewer features than dedicated instances.

All options:
```
--api-url   Memory service URL (enables remote mode)
--token     API key for remote mode
--db-url    Database URL for embedded mode (or set MEMORIA_DB_URL env var)
--user      Default user ID (default: "default")
--transport stdio | sse  (default: stdio)
```

### Kiro

`.kiro/settings/mcp.json`:
```json
{
  "mcpServers": {
    "memoria": {
      "command": "memoria-mcp",
      "args": ["--api-url", "https://memoria-host:8100", "--token", "sk-your-key..."]
    }
  }
}
```

### Cursor

`.cursor/mcp.json`:
```json
{
  "mcpServers": {
    "memoria": {
      "command": "memoria-mcp",
      "args": ["--api-url", "https://memoria-host:8100", "--token", "sk-your-key..."]
    }
  }
}
```

### Claude Desktop

`claude_desktop_config.json`:
```json
{
  "mcpServers": {
    "memoria": {
      "command": "memoria-mcp",
      "args": ["--api-url", "https://memoria-host:8100", "--token", "sk-your-key..."]
    }
  }
}
```

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
