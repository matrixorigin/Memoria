# Apikey 模式使用指南

## 概述

Memoria 支持两种远程认证模式：

| 模式 | 请求头 | 适用场景 |
|------|--------|----------|
| Token 模式 | `Authorization: Bearer <token>` | 所有用户共享一个数据库，通过 master key 管理 API key |
| Apikey 模式 | `X-API-Key: <apikey>` | 每个用户独立数据库，通过外部认证服务解析连接信息 |

两种模式可以同时启用，互不影响。

### Apikey 模式工作流程

```
客户端 → X-API-Key 头 → Memoria API → 远程认证服务 → 返回用户专属数据库连接
                                                        ↓
                                              连接到用户独立的 MatrixOne 数据库
```

---

## 一、API Server 部署

### 1. 配置 `.env` 文件

```bash
cp .env.example .env
```

Apikey 模式必填项：

```bash
# 远程认证服务地址（POST /apikey/connection 接口）
MEMORIA_REMOTE_AUTH_SERVICE_URL=http://127.0.0.1:8000

# Embedding 配置（server 端负责 embedding 计算）
EMBEDDING_PROVIDER=openai
EMBEDDING_MODEL=BAAI/bge-m3
EMBEDDING_DIM=1024
EMBEDDING_API_KEY=sk-your-key
EMBEDDING_BASE_URL=https://api.siliconflow.cn/v1
```

可选项：

```bash
# 连接信息缓存 TTL（秒），默认 60，0 = 不缓存
MEMORIA_CONN_CACHE_TTL=60

# Master Key（如果同时需要 token 模式的管理员操作）
MEMORIA_MASTER_KEY=your-master-key-here

# LLM（用于反思和实体抽取功能）
MEMORIA_LLM_API_KEY=your-llm-key
MEMORIA_LLM_BASE_URL=https://api.example.com/v1
MEMORIA_LLM_MODEL=gpt-4o-mini
```

> API Server 启动时会自动读取项目根目录的 `.env` 文件，无需通过命令行参数指定。

### 2. 启动 API Server

**本地开发模式**（需要已有 MatrixOne 数据库）：

```bash
# 安装依赖
pip install -e ".[dev,openai-embedding]"

# 启动
make dev
# 或直接：
python -m uvicorn memoria.api.main:app --reload --port 8100
```

**Docker 模式**（自带 MatrixOne）：

```bash
docker compose up -d
```

### 3. 验证服务状态

```bash
curl http://localhost:8100/health
# {"status": "ok", "database": "connected"}
```


---

## 二、远程认证服务要求

Memoria 的 apikey 模式依赖一个外部认证服务来解析 API key。该服务需要实现以下接口：

### 接口规范

```
POST /apikey/connection
Authorization: Bearer <apikey>
```

响应示例：

```json
{
  "user_id": "user-123",
  "db_host": "10.0.0.1",
  "db_port": 6001,
  "db_user": "local-moi-account:moi_root",
  "db_password": "p@ssw0rd",
  "db_name": "memoria-user-123"
}
```

> `db_user` 和 `db_password` 中可以包含特殊字符（如 `:` 和 `@`），Memoria 会自动进行 URL 编码。

### 响应码

| 状态码 | 含义 |
|--------|------|
| 200 | 认证成功，返回连接信息 |
| 401 | API key 无效或已过期 |
| 其他 | Memoria 返回 502 Bad Gateway |

---

## 三、MCP 客户端注册到 IDE

### 方式一：使用 `memoria init` 命令

在你的项目目录下执行：

```bash
memoria init --api-url http://localhost:8100 --apikey your-api-key
```

该命令会自动检测当前目录下的 IDE 配置（Kiro / Cursor / Claude），并写入对应的 `mcp.json`。

指定 IDE：

```bash
memoria init --api-url http://localhost:8100 --apikey your-api-key --tool kiro
memoria init --api-url http://localhost:8100 --apikey your-api-key --tool cursor
memoria init --api-url http://localhost:8100 --apikey your-api-key --tool claude
```

### 方式二：手动编辑 `mcp.json`

**Kiro** — `.kiro/settings/mcp.json`：

```json
{
  "mcpServers": {
    "memoria": {
      "command": "memoria-mcp",
      "args": ["--api-url", "http://localhost:8100", "--apikey", "your-api-key"]
    }
  }
}
```

**Cursor** — `.cursor/mcp.json`：

```json
{
  "mcpServers": {
    "memoria": {
      "command": "memoria-mcp",
      "args": ["--api-url", "http://localhost:8100", "--apikey", "your-api-key"]
    }
  }
}
```

**Claude** — `.claude/mcp.json`：

```json
{
  "mcpServers": {
    "memoria": {
      "command": "memoria-mcp",
      "args": ["--api-url", "http://localhost:8100", "--apikey", "your-api-key"]
    }
  }
}
```

> 注意：`--apikey` 和 `--token` 互斥，不能同时使用。

---

## 四、`memoria-mcp` 启动参数

| 参数 | 说明 | 示例 |
|------|------|------|
| `--api-url` | Memoria REST API 地址（启用远程模式） | `http://localhost:8100` |
| `--token` | Token 模式认证（`Authorization: Bearer`） | `sk-your-token` |
| `--apikey` | Apikey 模式认证（`X-API-Key` 头） | `your-api-key` |
| `--db-url` | 数据库 URL（嵌入模式，直连数据库） | `mysql+pymysql://root:111@localhost:6001/memoria` |
| `--user` | 默认用户 ID（默认 `default`） | `alice` |
| `--transport` | 传输协议：`stdio`（默认）或 `sse` | `stdio` |

### 三种运行模式

```bash
# 嵌入模式：直连数据库，无需 API Server
memoria-mcp --db-url "mysql+pymysql://root:111@localhost:6001/memoria"

# 远程 Token 模式：通过 API Server，共享数据库
memoria-mcp --api-url "http://localhost:8100" --token "sk-your-token"

# 远程 Apikey 模式：通过 API Server，每用户独立数据库
memoria-mcp --api-url "http://localhost:8100" --apikey "your-api-key"
```

---

## 五、测试 Apikey 模式

### 使用 curl 测试

```bash
# 存储记忆
curl -X POST http://localhost:8100/v1/memories \
  -H "X-API-Key: your-api-key" \
  -H "Content-Type: application/json" \
  -d '{"content": "用户偏好深色模式", "memory_type": "profile"}'

# 检索记忆
curl -X POST http://localhost:8100/v1/memories/retrieve \
  -H "X-API-Key: your-api-key" \
  -H "Content-Type: application/json" \
  -d '{"query": "UI 偏好", "top_k": 5}'

# 查看用户画像
curl http://localhost:8100/v1/profiles/me \
  -H "X-API-Key: your-api-key"
```

### 使用 MCP 客户端测试

配置好 `mcp.json` 后，重启 IDE，然后在对话中使用 memory 相关工具：

- `memory_store` — 存储记忆
- `memory_retrieve` — 检索相关记忆
- `memory_search` — 语义搜索
- `memory_correct` — 修正记忆
- `memory_purge` — 删除记忆

---

## 六、与 Token 模式的区别

| 特性 | Token 模式 | Apikey 模式 |
|------|-----------|-------------|
| 认证头 | `Authorization: Bearer <token>` | `X-API-Key: <apikey>` |
| 数据库 | 所有用户共享一个数据库 | 每用户独立数据库 |
| 用户隔离 | 通过 `user_id` 字段隔离 | 物理数据库隔离 |
| 自动治理 | 后台定时调度（每小时/每天/每周） | 仅按需触发（无自动调度） |
| API Key 管理 | 通过 master key + `/auth/keys` 接口 | 由外部认证服务管理 |
| 适用场景 | 单租户 / 小团队 | 多租户 SaaS / 企业级 |

---

## 七、`memoria init` 完整参数

```bash
memoria init [选项]

连接选项：
  --db-url URL          嵌入模式数据库 URL
  --api-url URL         远程模式 API 地址
  --token TOKEN         Token 模式认证
  --apikey APIKEY       Apikey 模式认证（与 --token 互斥）
  --user USER           默认用户 ID（默认 "default"）

IDE 选项：
  --tool {kiro,cursor,claude}   目标 IDE（可重复，默认自动检测）
  --force                       覆盖已自定义的规则文件
  --dir DIR                     项目目录（默认当前目录）

Embedding 选项（仅嵌入模式需要）：
  --embedding-provider PROVIDER   embedding 提供商（openai / local）
  --embedding-model MODEL         模型名称
  --embedding-dim DIM             向量维度
  --embedding-api-key KEY         API key
  --embedding-base-url URL        API 基础 URL
```

> 远程模式（`--api-url`）下不需要 embedding 参数，因为 embedding 由 server 端处理。
