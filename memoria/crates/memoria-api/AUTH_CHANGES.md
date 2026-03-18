# API Key 认证支持 — 变更说明

## 概述

本次变更为 Memoria API 引入了 API key 认证机制，使普通用户可以通过 `sk-...` 格式的密钥访问 API，
同时建立了 master key 与 API key 之间的权限隔离模型。

涉及 **7 个文件**，净增 **57 行**。

## 变更内容

### 1. AuthUser 结构体重构（auth.rs）

**改动前：** `AuthUser(pub String)` — 仅携带 `user_id`，不区分认证方式。

**改动后：**

```rust
pub struct AuthUser {
    pub user_id: String,
    pub is_master: bool,   // true = master key 认证 / 开发模式, false = API key 认证
}
```

新增 `require_master()` 方法，admin 和 key 管理路由通过此方法强制要求 master 权限。

### 2. Bearer 解析逻辑重构（auth.rs）

**改动前：** 先判断 `master_key` 是否配置，配了才解析 `Authorization` 头。API key 无法在未配置
master_key 的环境中工作。

**改动后：** 先判断请求是否携带 `Bearer` token，再决定如何校验：

```
请求带了 Bearer？
  ├─ 是 → 校验 master key（如果配了）→ 校验 API key → 都不匹配则 401
  └─ 否 → master_key 配了？
            ├─ 是 → 401 "Missing Bearer token"
            └─ 否 → X-User-Id fallback（开发模式向后兼容）
```

### 3. API key 认证函数（auth.rs）

新增 `validate_api_key()` 函数：
- 对 raw key 做 SHA-256 哈希后查 `mem_api_keys` 表
- 校验 `is_active = 1` 和 `expires_at`（NULL 表示永不过期，非 NULL 则校验是否已过期）
- 认证通过后异步更新 `last_used_at`（fire-and-forget，不阻塞请求）

### 4. Admin 路由权限收紧（routes/admin.rs）

所有 `/admin/*` 路由（共 10 个 handler）均在入口处加上了 `auth.require_master()?;`：

| 路由 | Handler |
|---|---|
| `GET /admin/stats` | `system_stats` |
| `GET /admin/users` | `list_users` |
| `GET /admin/users/:id/stats` | `user_stats` |
| `DELETE /admin/users/:id` | `delete_user` |
| `POST /admin/users/:id/reset-access-counts` | `reset_access_counts` |
| `POST /admin/governance/:id/trigger` | `trigger_governance` |
| `POST /admin/users/:id/strategy` | `set_user_strategy` |
| `GET /admin/users/:id/keys` | `list_user_keys` |
| `DELETE /admin/users/:id/keys` | `revoke_all_user_keys` |
| `POST /admin/users/:id/params` | `set_user_params` |

`/v1/health/*` 路由不需要 master 权限，仅做了解构模式的适配。

### 5. Key 管理路由权限修复（routes/auth.rs）

| 路由 | 改动 |
|---|---|
| `POST /auth/keys` (create) | 加上 `require_master()?;`，只有 master 能创建 key |
| `GET /auth/keys/:id` (get) | 权限判断从硬编码 `user_id != "admin"` 改为 `!is_master` |
| `PUT /auth/keys/:id/rotate` | 权限判断从 `!state.master_key.is_empty()` 改为 `is_master` |
| `DELETE /auth/keys/:id` (revoke) | 同上 |

**修复前的 bug：** `rotate_key` 和 `revoke_key` 用 `!state.master_key.is_empty()` 判断是否允许
跨用户操作，这只检查了"服务端是否配置了 master_key"，而不是"这次请求是否用 master_key 认证"。
任何有效 API key 都能跨用户操作。

### 6. 普通路由适配（memory.rs, governance.rs, sessions.rs, snapshots.rs）

纯机械性改动：`AuthUser(user_id)` → `AuthUser { user_id, .. }`，`AuthUser(_)` → `AuthUser { .. }`。
行为不变。

## 认证矩阵

### 配置了 master_key 的环境（生产）

| 请求方式 | user_id 来源 | is_master | 可访问 /admin | 可访问 /v1 |
|---|---|---|---|---|
| `Bearer <master_key>` + `X-User-Id: alice` | X-User-Id 头 | `true` | ✓ | ✓ |
| `Bearer sk-xxxx`（有效 API key） | 数据库 key owner | `false` | ✗ (403) | ✓ |
| `Bearer sk-xxxx`（已过期） | — | — | ✗ (401) | ✗ (401) |
| `Bearer wrong-token` | — | — | ✗ (401) | ✗ (401) |
| 无 Authorization 头 | — | — | ✗ (401) | ✗ (401) |

### 未配置 master_key 的环境（开发/测试）

| 请求方式 | user_id 来源 | is_master | 可访问 /admin | 可访问 /v1 |
|---|---|---|---|---|
| `Bearer sk-xxxx`（有效 API key） | 数据库 key owner | `false` | ✗ (403) | ✓ |
| 无 Authorization + `X-User-Id: alice` | X-User-Id 头 | `true` | ✓ | ✓ |
| 无 Authorization，无 X-User-Id | 默认 "default" | `true` | ✓ | ✓ |

## TODO

### 开发模式下匿名请求拥有 master 权限

**现状：** 未配置 `master_key` 时，不携带 Bearer token 的请求会被标记为 `is_master: true`，
可以访问 `/admin/*` 和 `POST /auth/keys` 等管理接口。

**原因：** 这是项目一直以来的行为——不配 `master_key` 意味着"开发模式，一切放开"。
现有几十个测试用例依赖此行为（`spawn_server()` 不配 master_key，直接裸访问 `/admin/*`）。

**风险：** 如果生产环境遗漏配置 `master_key`，所有管理接口将对匿名请求开放。

**后续可选方案：**

1. **启动时校验（推荐）：** 当检测到 `master_key` 为空时，打印醒目的 WARNING 日志，
   提示管理接口处于开放状态。生产部署通过 CI/CD 或配置校验强制要求配置 `master_key`。

2. **引入 AuthPrincipal 枚举：** 将 `is_master: bool` 替换为显式枚举
   `Master / ApiKey / LegacyOpen`，`require_master()` 仅接受 `Master`，
   开发模式下 admin 路由返回 403。需同步修改相关测试。

3. **环境变量开关：** 新增 `ALLOW_OPEN_MODE=true` 环境变量，
   仅当显式声明时才允许无 master_key 运行，否则启动时 panic。
