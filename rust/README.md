# ncc-registry · Rust 版（axum）

本目录是 `ncc-registry` 的 **Rust 实现**：和根目录那份 Go 实现功能对齐、**共用同一个数据库与协议**。
两种实现可以交替使用同一个数据目录（账号、令牌、口令哈希、制品、加密配置都互通）。

```text
rust/
├─ crates/ncc-core/       两个服务共用的基础层（环境配置 / 密码学 / ID / 错误 / 作用域 /
│                         加密盒 / 字节存储 / 数据库连接与建表补列）—— 见文末「关于 ncc-core」
├─ crates/ncc-registry/   bin: ncc-registry
└─ scripts/smoke.sh       端到端冒烟：起服务 + curl 逐项断言
```

## 为什么是这套技术栈

| 选择 | 理由 |
|---|---|
| **axum 0.8**（+ tokio / tower-http） | 成熟、生态最厚的一线 Rust Web 框架：tower 中间件（CORS / 静态文件 / 追踪）直接可用；类型化提取器让「鉴权上下文」成为处理器签名的一部分，而不是靠全局中间件偷偷塞值 |
| **sqlx（SQLite，运行时查询）** | 不用 ORM 宏：schema 已经存在且必须逐字兼容，「运行时 SQL + `FromRow`」把列名对齐摆在明面上，也不会把「能开在旧库上」变成构建期依赖 |
| **手写 HS256 JWT / bcrypt / sha256** | Go 版就是手写的。引入 JWT 框架要花力气对齐它的默认行为（头部字节、无填充 base64、字段省略规则），而这些恰恰是**互通的前提** |

## 快速开始

```bash
cd rust
cargo build                # → target/debug/ncc-registry
cargo test                 # 86 项单测（临时 SQLite，不起服务）

NCCR_PORT=8282 ./target/debug/ncc-registry      # 数据默认落 ./data

bash scripts/smoke.sh                           # 27 项端到端断言
REG_DB=../data/ncc-registry.db bash scripts/smoke.sh   # 也可以直接指向既有库
```

控制台：Go 版用 `go:embed` 把控制台打进二进制，Rust 版从目录托管 ——
`NCCR_CONSOLE_DIR`（默认 `./web`）指向静态目录，目录不存在就不挂这条路由（API 不受影响）。
在仓库里跑时可以直接指向 Go 那套资源：`NCCR_CONSOLE_DIR=httpapi/web`。

## 与 Go 版的兼容性（这是本次重写的核心约束）

| 约束 | 做法 | 证据 |
|---|---|---|
| **同一份 schema** | `src/schema.rs` 的 DDL 是用现有 Go 代码把服务跑起来、建出库后 `sqlite3 .schema` dump 的 | Rust 服务直接开在 Go 建的库上，读出正确计数 |
| **老库补列** | `migrate` 分三阶段：建表 → `ALTER TABLE ADD COLUMN` 补列 → 建索引（与 GORM AutoMigrate 同理） | 单测覆盖；启动日志会打印「已补列 …」 |
| **同一套令牌** | HS256、头 `{"alg":"HS256","typ":"JWT"}`、载荷字段（`sub/email/kind/node/scope/iat/exp`）逐字段对齐 | Rust 发的令牌 Go 能验，反之亦然 |
| **同一套口令哈希** | bcrypt cost=10（`$2a$`），与 `golang.org/x/crypto/bcrypt` 互认 | Rust 注册的账号能在 Go 服务上登录 |
| **同一套 API-Key** | `ncc_<prefix>_<secret>`，库里只存 prefix 与整串 sha256 | 冒烟里验了只读 key 发布被拒 403 |
| **同一套响应约定** | 成功 `{...}`；失败 `{"error":{"code","message"}}`，code 与文案照搬 | 冒烟逐项断言 |
| **配置加密盒同构** | `enc:v1:` + base64(nonce‖ciphertext‖tag)，密钥 = HMAC-SHA256(节点密钥, `ncc-registry/config-content-v1`) | 冒烟直接查库断言「存的是密文」 |
| **时间列格式** | GORM 落库是 `2026-09-07 01:16:25.887763+08:00`（不是 RFC3339），`timeutil` 两种都认 | 单测解析/格式化往返 |

**为什么不做成「换实现也换数据」**：部署形态是「一个二进制 + 一个数据目录」，
用户的目录里已经有账号、制品、节点与加密过的配置。要求用户为了换语言而导数据，
等于把一次技术升级变成一次运维事故。

## 配置

前缀 `NCCR_`，与平台的 `NCC_` 区分开，两个服务可以同时跑在一台机器上；所有项都有可用默认值。

| 变量 | 默认 | 说明 |
|---|---|---|
| `NCCR_ROLE` | `master` | `master`（权威）\| `worker`（边缘，必须配 `NCCR_MASTER_URL`） |
| `NCCR_PORT` | `8282` | 监听端口 |
| `NCCR_DATA_DIR` | `./data` | 数据根；相对路径按它解析 |
| `NCCR_DB_PATH` | `<data>/ncc-registry.db` | SQLite 库文件 |
| `NCCR_BLOB_DIR` | `<data>/blobs` | 制品字节（`/blobs` 静态公开） |
| `NCCR_CONSOLE_DIR` | `./web` | 控制台静态目录 |
| `NCCR_CONSOLE` | `true` | 是否托管控制台 |
| `NCCR_INVITE_CODE` | 空（开放注册） | 设了就要邀请码；逗号分隔多码 |
| `NCCR_NODE_TTL` | `60s` | 心跳超时即判离线（不落库，现场算） |
| `NCCR_JWT_TTL` | `168h` | 会话有效期 |
| `NCCR_CORS_ORIGINS` | 空（放开） | 逗号分隔来源白名单 |
| `NCCR_EXEC_ALLOW` | `wasm` | 允许的远程执行引擎（写错会在启动时报错） |

## 迁移状态

| 族 | 端点 | 状态 |
|---|---|---|
| 健康 / 元信息 | `/api/health`、`/api/meta` | ✅ |
| 账号 | `/api/auth/{meta,register,login,me,keys,key-scopes}` | ✅ |
| 命名空间 | `/api/namespaces/{mine,living}` | ✅ |
| 制品 | `/api/registry`（kinds / uploads / 列表 / 详情 / 发布 / 改 / 删 / 签名 / download / bytes） | ✅ |
| 节点 | `/api/nodes`（列表 / kinds / offers / discover / regions / heartbeat / links / 注销） | ✅ |
| 授权 | `/api/grants` | ✅ |
| 托管配置 | `/api/configs`（kinds / bundle / CRUD / revisions / rollback，含 secret 落库加密） | ✅ |
| 分享 | `/api/shares` + 公开页 `/s/{token}`、`/s/{token}/raw` | ✅ |
| Agent 名片 | `/api/agent-cards` + 公开页 `/a/{token}` | ✅ |
| 其余 | traces / kb / mem / ckpt / 通用记录仓 / index / access / p2p / conn / exec / cluster / feedback / admin | ⏳ 回 **501**（不是 404），原实现文件写在对应模块的文件头 |

## 已知取舍（想改就改这里）

1. **只支持 SQLite**：与 Go 版一致（节点本来就是「一个二进制 + 一个 SQLite 文件」）。
2. **只支持本地磁盘字节**：Go 版的 `storage` 抽象留了对象存储的口子，Rust 版暂时只有本地实现。
3. **手写的解压与 multipart**：Agent 名片要读 `.hur` 包（gzip / zip / DEFLATE）与上传表单，
   工作区不便再加依赖，于是自实现了这三种块类型与极简 multipart。
   代价：不校验 gzip CRC/ISIZE、只认单 gzip 成员与非 zip64、超 64MB 直接报错。
4. **`exp` 边界**：`exp` 时刻本身算过期（Go 是 `exp < now`），差 1 秒，对可用性无影响。
5. **编译期还有 `never used` 警告**：那些是已实现但尚未接线的数据访问函数，
   对应上表「尚未迁移」的族，随补齐自然消失，不用 `allow(dead_code)` 掩盖。

## 关于 `ncc-core`（两侧各存一份，改动要同时落两处）

平台服务与内网节点服务各自需要这一层，但**两个仓库必须各自自包含**：
本仓的边界约定是「不 import 平台私有代码」，反过来同理，所以它是有意复制的一份，
而不是跨仓依赖（依赖会让两个仓库互相锁死版本，而它们的发布节奏本来就不同）。
另一份在 `ncc-platform/server-rs/crates/ncc-core`，两边内容逐字相同。

## 测试与验证

```text
cargo test               # 86 项单测（ncc-core 26 + ncc-registry 60）
bash scripts/smoke.sh    # 27 项端到端断言
```

单测都是「真 SQLite + 真 SQL」：建临时库、跑 `schema::DDL`、断言行为；权限、可见性、
token 只存哈希、secret 加密、老库补列这些边界都有覆盖。

跨实现验证（已实测）：Rust 服务开在 Go 建的老库上正常；Rust 注册的账号能在 Go 服务登录，
Rust 上传发布的制品、注册的节点在 Go 服务里能列出/discover。
