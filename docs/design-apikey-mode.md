# Apikey 认证模式 — 架构设计文档

> 版本: 1.0 | 日期: 2026-03-15 | 状态: 已实现

## 1. 背景与动机

Memoria 原有的 Token 模式（`Authorization: Bearer`）下，所有用户共享一个 MatrixOne 数据库，通过 `user_id` 字段实现逻辑隔离。这在单租户和小团队场景下足够，但在多租户 SaaS 场景中存在以下问题：

- 数据隔离不够彻底（逻辑隔离 vs 物理隔离）
- 无法为不同租户配置独立的数据库资源
- API Key 的生命周期管理耦合在 Memoria 内部

Apikey 模式通过引入外部认证服务，实现每用户独立数据库的物理隔离。

## 2. 设计目标

1. 新增 `X-API-Key` 认证方式，与现有 `Authorization: Bearer` 完全共存
2. 通过外部认证服务将 API Key 解析为用户专属的数据库连接信息
3. 每用户独立 MatrixOne 数据库，实现物理级数据隔离
4. 对现有 Token 模式零影响 — 不修改任何已有认证逻辑
5. 所有现有 API 端点自动支持两种认证模式

## 3. 架构概览

```
┌─────────────────────────────────────────────────────────────────┐
│                        客户端请求                                │
│  ┌─────────────────────┐    ┌──────────────────────┐           │
│  │ Authorization:       │    │ X-API-Key: <apikey>  │           │
│  │ Bearer <token>       │    │                      │           │
│  └──────────┬──────────┘    └──────────┬───────────┘           │
└─────────────┼──────────────────────────┼───────────────────────┘
              │                          │
              ▼                          ▼
┌─────────────────────────────────────────────────────────────────┐
│                   get_auth_context()                             │
│                   (dependencies.py)                              │
│                                                                  │
│  检查 X-API-Key 头 ──→ 有 ──→ _resolve_apikey()                │
│         │                         │                              │
│         ▼                         ▼                              │
│        无                  RemoteAuthService                     │
│         │                  POST /apikey/connection                │
│         ▼                         │                              │
│  get_current_user_id()            ▼                              │
│  (原有 token 逻辑)         ConnInfo(user_id, db_*)              │
│         │                         │                              │
│         ▼                         ▼                              │
│  AuthContext(                AuthContext(                         │
│    user_id,                   user_id,                           │
│    db_factory=共享DB          db_factory=用户专属DB               │
│  )                          )                                    │
└─────────────────────────────────────────────────────────────────┘
              │                          │
              ▼                          ▼
┌──────────────────────┐  ┌──────────────────────────┐
│   共享 MatrixOne DB   │  │  用户专属 MatrixOne DB    │
│   (memoria)           │  │  (memoria-user-xxx)      │
└──────────────────────┘  └──────────────────────────┘
```

## 4. 核心组件

### 4.1 AuthContext — 统一认证上下文

```python
@dataclass
class AuthContext:
    user_id: str
    db_factory: sessionmaker
```

所有 API 端点通过 `Depends(get_auth_context)` 获取认证上下文。`db_factory` 在 token 模式下指向共享数据库，在 apikey 模式下指向用户专属数据库。端点代码无需关心认证模式的差异。

**分发逻辑**（`get_auth_context`）：

```
请求进入 → 检查 X-API-Key 头
  ├─ 有 → _resolve_apikey() → 远程认证服务 → 用户专属 db_factory
  └─ 无 → 检查 Authorization 头 → 原有 token 校验 → 共享 db_factory
```

### 4.2 RemoteAuthService — 远程认证服务客户端

**文件**: `memoria/api/remote_auth_service.py`

职责：调用外部认证服务，将 API Key 解析为数据库连接信息。

```python
class RemoteAuthService:
    def resolve(self, apikey: str) -> ConnInfo:
        # POST /apikey/connection
        # Authorization: Bearer <apikey>
        # → ConnInfo(user_id, db_host, db_port, db_user, db_password, db_name)
```

**缓存策略**：
- 按 API Key 前 12 字符作为缓存键
- TTL 可配置（`MEMORIA_CONN_CACHE_TTL`，默认 60 秒）
- 线程安全（`threading.Lock`）

**特殊字符处理**：
- `db_user` 和 `db_password` 可能包含 `:` 和 `@` 等特殊字符
- `ConnInfo.db_url` 属性使用 `urllib.parse.quote_plus()` 编码

### 4.3 Per-User Engine Cache — 用户数据库连接池

**文件**: `memoria/api/database.py`

```python
@lru_cache(maxsize=128)
def _get_user_engine(host, port, user, password, db_name) -> Engine:
    # 每个 (host, port, user, password, db_name) 组合缓存一个 Engine
    # db_name 校验: [a-zA-Z0-9_\-]+（允许连字符）

def get_user_session_factory(host, port, user, password, db_name) -> sessionmaker:
    # 首次调用时自动创建 memory 表（ensure_tables + governance 基础表）
    # 后续调用直接返回缓存的 sessionmaker
```

**表初始化**：首次连接用户数据库时，自动创建所有必要的表（`mem_memories`、`memory_graph_nodes`、`memory_graph_edges`、治理基础表等）。通过 `_user_db_initialized` 集合确保每个 `db_name` 只初始化一次。

### 4.4 Middleware 适配

**文件**: `memoria/api/middleware.py`

速率限制中间件同时支持两种认证头：

```python
x_api_key = request.headers.get("x-api-key", "")
if x_api_key:
    api_key = x_api_key
elif auth.startswith("Bearer "):
    api_key = auth[7:]
```

两种模式使用相同的速率限制策略，按 key 前 12 字符作为限流键。


### 4.5 MCP Server 适配

**文件**: `memoria/mcp_local/server.py`

`HTTPBackend`（远程模式）新增 `--apikey` 参数支持：

- `--token`: 请求时使用 `Authorization: Bearer <token>` 头
- `--apikey`: 请求时使用 `X-API-Key: <apikey>` 头
- 两者互斥，不能同时使用

### 4.6 CLI 适配

**文件**: `memoria/cli.py`

`memoria init` 命令新增 `--apikey` 参数：

```bash
# Token 模式
memoria init --api-url http://localhost:8100 --token sk-xxx

# Apikey 模式
memoria init --api-url http://localhost:8100 --apikey your-key
```

生成的 `mcp.json` 中 args 会包含对应的认证参数。`--token` 和 `--apikey` 互斥校验在 `cmd_init()` 入口处完成。

## 5. 配置项

| 环境变量 | 默认值 | 说明 |
|----------|--------|------|
| `MEMORIA_REMOTE_AUTH_SERVICE_URL` | `""` | 远程认证服务地址。为空时 apikey 模式不可用（返回 501） |
| `MEMORIA_CONN_CACHE_TTL` | `60` | 认证结果缓存 TTL（秒）。0 = 不缓存 |

这两个配置项仅影响 apikey 模式，对 token 模式无任何影响。

## 6. 治理策略差异

| 行为 | Token 模式 | Apikey 模式 |
|------|-----------|-------------|
| 自动治理调度 | ✅ 启动时创建（每小时/每天/每周） | ❌ 不启动 |
| 按需治理 | ✅ `POST /v1/consolidate` 等 | ✅ 相同接口，相同 cooldown |
| 治理范围 | 共享数据库中该用户的数据 | 用户专属数据库的全部数据 |

**设计决策**：apikey 模式下不启动自动治理调度器，原因：
1. 每个用户有独立数据库，服务端无法预知所有用户的数据库连接
2. 治理调度器需要持有数据库连接，无法为动态数量的用户维护连接池
3. 用户可通过 MCP 工具（`memory_governance`）或 REST API 按需触发

判断逻辑在 `memoria/api/main.py` 的 `lifespan` 中：

```python
if not _s.remote_auth_service_url:
    # Token 模式：启动自动治理
    scheduler = MemoryGovernanceScheduler(backend=backend)
    await scheduler.start()
# Apikey 模式：不启动，用户按需触发
```

## 7. 安全考量

### 7.1 认证隔离
- 两种认证模式完全独立，互不影响
- Token 模式的 master key、API key 哈希表等逻辑未做任何修改
- Apikey 模式的认证完全委托给外部服务

### 7.2 数据库连接安全
- `db_name` 校验：`[a-zA-Z0-9_\-]+`，防止 SQL 注入
- `db_user` / `db_password` 使用 `quote_plus()` 编码，防止连接字符串注入
- 每用户 Engine 通过 `@lru_cache(maxsize=128)` 缓存，避免连接泄漏

### 7.3 速率限制
- 两种模式共享同一套速率限制策略
- Master key 豁免速率限制（用于管理和基准测试）

### 7.4 错误处理
- 远程认证服务返回 401 → Memoria 返回 401（Invalid or expired API key）
- 远程认证服务不可达或返回其他错误 → Memoria 返回 502（Bad Gateway）
- `REMOTE_AUTH_SERVICE_URL` 未配置时使用 apikey → 返回 501（Not Implemented）

## 8. 修改文件清单

| 文件 | 变更类型 | 说明 |
|------|----------|------|
| `memoria/api/remote_auth_service.py` | 新增 | 远程认证服务客户端（ConnInfo、缓存、resolve） |
| `memoria/api/dependencies.py` | 修改 | 新增 AuthContext、get_auth_context、_resolve_apikey |
| `memoria/api/database.py` | 修改 | 新增 _get_user_engine、get_user_session_factory、_ensure_user_tables |
| `memoria/api/main.py` | 修改 | 治理调度器条件启动（非 apikey 模式才启动） |
| `memoria/api/middleware.py` | 修改 | 速率限制支持 X-API-Key 头 |
| `memoria/api/routers/memory.py` | 修改 | 所有端点改用 get_auth_context 依赖 |
| `memoria/api/routers/snapshots.py` | 修改 | 同上 |
| `memoria/api/routers/user_ops.py` | 修改 | 同上 |
| `memoria/mcp_local/server.py` | 修改 | HTTPBackend 支持 --apikey 参数 |
| `memoria/cli.py` | 修改 | memoria init 支持 --apikey 参数 |
| `memoria/config.py` | 修改 | 新增 remote_auth_service_url、conn_cache_ttl 配置 |
| `tests/unit/test_apikey_auth.py` | 新增 | 25 个单元测试覆盖核心逻辑 |

## 9. 向后兼容性

- 所有现有 API 端点的请求/响应格式不变
- Token 模式的认证逻辑（`get_current_user_id`、`require_admin`）未修改
- 原有的 `/auth/keys` 管理接口不受影响
- 未配置 `REMOTE_AUTH_SERVICE_URL` 时，系统行为与修改前完全一致
- 自动治理调度器在 token 模式下照常运行

## 10. 测试覆盖

测试文件：`tests/unit/test_apikey_auth.py`

| 测试类别 | 覆盖内容 |
|----------|----------|
| RemoteAuthService | resolve 成功、401 处理、缓存命中/过期、ConnInfo.db_url 特殊字符编码 |
| AuthContext 分发 | X-API-Key 走 apikey 路径、Bearer 走 token 路径、两者都缺返回 401 |
| _resolve_apikey | 未配置返回 501、认证失败返回 401、服务不可达返回 502 |
| 数据库连接 | db_name 校验（拒绝注入、允许连字符）、表初始化幂等性 |
| Middleware | X-API-Key 前缀提取用于限流 |
| CLI | --token 和 --apikey 互斥校验 |
| Config | remote_auth_service_url / conn_cache_ttl 默认值和环境变量覆盖 |
