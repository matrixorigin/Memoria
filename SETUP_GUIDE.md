# Memoria Setup Guide (for AI Agents)

This document provides step-by-step executable instructions for AI agents to help users install and configure Memoria. Follow each step in order.

## Step 1: Check Prerequisites

```bash
# Check Python version (need 3.11+)
python3 --version

# Check Docker and Docker Compose
docker --version
docker compose version
```

If Python < 3.11, guide user to install Python 3.11+.
If Docker is not installed, guide user to install Docker Desktop or Docker Engine.

## Step 2: Deploy MatrixOne Database

This is the most critical step. MatrixOne is the storage backend for all memories.

### Option A: Using docker-compose (recommended)

Clone or navigate to the Memoria repo, then:

```bash
# Start MatrixOne with data persistence and memory limit
docker compose up -d

# Wait for MatrixOne to be ready (takes ~30-60 seconds on first start)
# The healthcheck will verify it's accepting connections
docker compose ps
```

The `docker-compose.yml` provides:
- Port mapping: host `6001` → container `6001`
- Data persistence: `./data/matrixone/` mounted to `/mo-data`
- Memory limit: 2GB by default (configurable via `.env`)
- Auto-restart on failure
- Healthcheck every 10s

To customize, copy `.env.example` to `.env` and edit:
```bash
cp .env.example .env
# Edit .env to change port, data dir, memory limit, etc.
```

### Option B: Using docker run directly

```bash
# Check if MatrixOne is already running
docker ps --filter name=matrixone --format '{{.Names}} {{.Status}}'

# If not running, check if container exists but stopped
docker ps -a --filter name=matrixone --format '{{.Names}} {{.Status}}'

# If container exists but stopped:
docker start matrixone

# If no container exists, create one:
docker run -d \
  --name matrixone \
  -p 6001:6001 \
  -v $(pwd)/data/matrixone:/mo-data \
  --memory=2g \
  --restart unless-stopped \
  matrixorigin/matrixone:latest
```

### Verify MatrixOne is ready

MatrixOne needs ~30-60 seconds to initialize on first start. Verify with:

```bash
# Method 1: Check healthcheck status (if using docker-compose)
docker compose ps
# Look for "(healthy)" in the STATUS column

# Method 2: Try connecting directly
# If mysql client is available:
mysql -h 127.0.0.1 -P 6001 -u root -p111 -e "SELECT 1"

# Method 3: Check logs for readiness
docker logs memoria-matrixone 2>&1 | tail -20
# Look for log lines indicating the service is listening
```

### Common MatrixOne issues

**Container exits immediately:**
```bash
# Check logs for errors
docker logs memoria-matrixone 2>&1 | tail -30

# Common cause: data directory permissions
# Fix: ensure the data directory is writable
mkdir -p data/matrixone
chmod 777 data/matrixone
```

**Port 6001 already in use:**
```bash
# Find what's using the port
lsof -i :6001 || ss -tlnp | grep 6001

# Either stop the other process, or change the port:
# In .env: MO_PORT=6002
# Then: docker compose up -d
# And use: memoria init --db-url 'mysql+pymysql://root:111@localhost:6002/memoria'
```

**Out of memory:**
```bash
# Increase memory limit in .env:
# MO_MEMORY_LIMIT=4g
docker compose up -d
```

**Docker permission denied (Linux):**
```bash
# Option 1: Add user to docker group (recommended, requires re-login)
sudo usermod -aG docker $USER
newgrp docker

# Option 2: Run with sudo (not recommended for production)
sudo docker compose up -d
```

**Docker image pull fails (slow or timeout — common in China):**
```bash
# Option 1: Configure Docker mirror
# Create or edit /etc/docker/daemon.json:
sudo tee /etc/docker/daemon.json <<EOF
{
  "registry-mirrors": [
    "https://docker.1ms.run",
    "https://docker.xuanyuan.me"
  ]
}
EOF
sudo systemctl restart docker
docker compose up -d

# Option 2: Pull from alternative registry
docker pull swr.cn-north-4.myhuaweicloud.com/ddn-k8s/docker.io/matrixorigin/matrixone:latest
docker tag swr.cn-north-4.myhuaweicloud.com/ddn-k8s/docker.io/matrixorigin/matrixone:latest matrixorigin/matrixone:latest
docker compose up -d
```

**Docker not installed:**
```bash
# Linux (Ubuntu/Debian)
curl -fsSL https://get.docker.com | sh
sudo usermod -aG docker $USER
newgrp docker

# macOS — install Docker Desktop: https://docs.docker.com/desktop/install/mac-install/

# If Docker is not an option, use MatrixOne Cloud instead:
# Register at https://cloud.matrixorigin.cn (free tier available)
# Then: memoria init --db-url 'mysql+pymysql://user:pass@cloud-host:6001/db'
```

**Docker Compose not available:**
```bash
# Docker Compose V2 is included in Docker Desktop and recent Docker Engine.
# If 'docker compose' doesn't work, try:
docker-compose up -d

# Or install Docker Compose plugin:
sudo apt-get install docker-compose-plugin
```

## Step 3: Install Memoria

First, check if the user is already in a virtual environment:
```bash
python3 -c "import sys; print('venv' if sys.prefix != sys.base_prefix else 'system')"
```

If output is `system`, create and activate a virtual environment:
```bash
python3 -m venv .venv && source .venv/bin/activate
```

Then install:
```bash
# Install with local embedding support (recommended)
# - With NVIDIA GPU:
pip install 'memoria[local-embedding]'
# - Without GPU (CPU-only, avoids large CUDA dependencies):
pip install torch --index-url https://download.pytorch.org/whl/cpu
pip install 'memoria[local-embedding]'
```

**Important notes about local embedding:**
- First install downloads the embedding model (~80MB)
- On the first query, the model loads into memory — this takes a few seconds
- Subsequent queries are fast

If user does NOT want local embedding (e.g. wants to use OpenAI instead):
```bash
pip install memoria
```

## Step 4: Initialize for the User's AI Tool

Detect which AI tool the user is using, then run `memoria init` in their project directory.

### For Kiro users:
```bash
cd <project-directory>
mkdir -p .kiro
memoria init
```
This creates:
- `.kiro/settings/mcp.json` — MCP server config
- `.kiro/steering/memory.md` — steering rules

### For Cursor users:
```bash
cd <project-directory>
mkdir -p .cursor
memoria init
```
This creates:
- `.cursor/mcp.json` — MCP server config
- `.cursor/rules/memory.mdc` — rules for Cursor

### For Claude Code users:
```bash
cd <project-directory>
memoria init
```
This creates:
- `.mcp.json` — MCP server config
- `CLAUDE.md` — rules appended or created

### Custom database URL:
```bash
memoria init --db-url 'mysql+pymysql://root:111@localhost:6001/memoria'
```

### Using OpenAI embeddings instead of local:
```bash
memoria init --embedding-provider openai --embedding-api-key sk-...
```

## Step 5: Verify Setup

```bash
# Check configuration status
memoria status

# Test that MCP server can start
memoria-mcp --db-url "mysql+pymysql://root:111@localhost:6001/memoria"
# (Ctrl+C to stop after confirming it starts without errors)
```

## Step 6: Restart AI Tool

Tell the user to restart their AI tool (Kiro / Cursor / Claude Code) to pick up the new MCP configuration.

## Troubleshooting

### "Cannot connect to database"
```bash
# Check MatrixOne is running and healthy
docker compose ps
# or
docker ps --filter name=matrixone

# If not running:
docker compose up -d
# or
docker start matrixone

# Wait 30s for it to be ready, then retry
memoria init
```

### "sentence-transformers not installed"
```bash
# With NVIDIA GPU:
pip install 'memoria[local-embedding]'
# Without GPU (CPU-only):
pip install torch --index-url https://download.pytorch.org/whl/cpu
pip install 'memoria[local-embedding]'
```

### AI tool doesn't use memory after setup
1. Run `memoria status` to verify config files exist
2. Make sure the AI tool was restarted after `memoria init`
3. Check MCP server starts: `memoria-mcp --db-url "mysql+pymysql://root:111@localhost:6001/memoria"`

### Update rules after upgrading Memoria
```bash
pip install --upgrade memoria
memoria update-rules
# Restart AI tool
```
