# ncc-registry · 内网节点服务（Rust + axum）

本目录就是 `ncc-registry` 的**实现本体**（Rust + axum + sqlx）：单二进制 + 一个 SQLite 文件。

它是按「**共用同一个数据库与同一个协议**」这条硬约束重写出来的 —— 旧实现（Go，已删除，
见 git 历史）建的 `ncc-registry.db` 可以直接被这份二进制打开：账号、令牌、口令哈希、制品、
加密配置全都在原地继续用（老库缺的列启动时自动补）。

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
| **手写 HS256 JWT / bcrypt / sha256** | 旧实现就是手写的。引入 JWT 框架要花力气对齐它的默认行为（头部字节、无填充 base64、字段省略规则），而这些恰恰是**互通的前提** |

## 快速开始

```bash
cd rust
cargo build                # → target/debug/ncc-registry
cargo test                 # 290 项单测（ncc-core 26 + ncc-registry 264，临时 SQLite，不起服务）

NCCR_PORT=8282 ./target/debug/ncc-registry      # 数据默认落 ./data

bash scripts/smoke.sh                           # 40 项端到端断言
REG_DB=../data/ncc-registry.db bash scripts/smoke.sh   # 也可以直接指向既有库
```

控制台：Rust 版从**目录**托管（不是编译进二进制），页面在本目录的 `web/` 下。
`NCCR_CONSOLE_DIR`（默认 `./web`）指向它，目录不存在就不挂这条路由（API 不受影响）——
所以镜像里要带上 `rust/web`，compose/CI 里都这么做了。

## 与 Go 版的兼容性（这是本次重写的核心约束）

| 约束 | 做法 | 证据 |
|---|---|---|
| **同一份 schema** | `src/schema.rs` 的 DDL 是把旧实现跑起来建出库后 `sqlite3 .schema` dump 的 | 实测：Rust 服务直接开在旧实现建的库上，读出正确计数 |
| **老库补列** | `migrate` 分三阶段：建表 → `ALTER TABLE ADD COLUMN` 补列 → 建索引（与 GORM AutoMigrate 同理） | 单测覆盖；启动日志会打印「已补列 …」 |
| **同一套令牌** | HS256、头 `{"alg":"HS256","typ":"JWT"}`、载荷字段（`sub/email/kind/node/scope/iat/exp`）逐字段对齐 | 实测（Go 侧删除前）：两边互发互验都通过 |
| **同一套口令哈希** | bcrypt cost=10（`$2a$`），与 `golang.org/x/crypto/bcrypt` 互认 | 实测（Go 侧删除前）：用这份二进制注册的账号，旧实现也能登录 |
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
| 轨迹 | `/api/traces`（上报 / 查询 / 汇总） | ✅ |
| 三样状态 | `/api/{kb,mem,ckpt}`（kinds / CRUD / revisions / gc / prune / lineage / 签名 blob 地址） | ✅ |
| 通用记录仓 | `/api/store`（kinds / 声明 / 集合 / 记录 / 历史 / gc） | ✅ |
| 索引副本 | `/api/index`（列表 / 频道 / 接收平台推送 / 本地匹配） | ✅ |
| 接入票据 | `/api/access/*` + 公开加入页 `/j/{key}` | ✅ |
| P2P 控制面 | `/api/p2p/{self,check,serve}`（含手写 STUN 判 NAT） | ✅ |
| 连接与执行 | `/api/conn/*`、`/api/exec/*`（远程执行 + 解包 + 文件面） | ✅ |
| 集群 | `/api/cluster`（join / heartbeat / workers / directory / ingest / revoke / replicate） | ✅ |
| 后台 | `/api/admin`（overview / users / nodes / services / audit / keys / rotate） | ✅ |
| 反馈 | `/api/feedback` | ✅ |

## 已知取舍（想改就改这里）

1. **只支持 SQLite**：与旧实现一致（节点本来就是「一个二进制 + 一个 SQLite 文件」）。
2. **只支持本地磁盘字节**：`/blobs` 走本地目录。旧实现的 `storage` 抽象留了 S3/Ceph 的口子，
   但当时也只有一个本地驱动。
3. **手写的解压与 multipart**：Agent 名片要读 `.hur` 包（gzip / zip / DEFLATE）与上传表单，
   工作区不便再加依赖，于是自实现了这三种块类型与极简 multipart。
   代价：不校验 gzip CRC/ISIZE、只认单 gzip 成员与非 zip64、超 64MB 直接报错。
4. **`exp` 边界**：`exp` 时刻本身算过期（Go 是 `exp < now`），差 1 秒，对可用性无影响。
5. **编译期有少量 `never used` 警告**：都是已经写好的数据访问 API，目前只有测试在调
   （比如后台要用但界面还没接的计数函数）。刻意留着，不用 `allow(dead_code)` 掩盖；
   CI 的 clippy 也因此没有开 `-D warnings`。

## 关于 `ncc-core`（两侧各存一份，改动要同时落两处）

平台服务与内网节点服务各自需要这一层，但**两个仓库必须各自自包含**：
本仓的边界约定是「不 import 平台私有代码」，反过来同理，所以它是有意复制的一份，
而不是跨仓依赖（依赖会让两个仓库互相锁死版本，而它们的发布节奏本来就不同）。
另一份在 `ncc-platform/server/crates/ncc-core`，两边内容逐字相同。

## 测试与验证

```text
cargo test               # 290 项单测（ncc-core 26 + ncc-registry 264）
bash scripts/smoke.sh    # 40 项端到端断言
```

单测都是「真 SQLite + 真 SQL」：建临时库、跑 `schema::DDL`、断言行为；权限、可见性、
token 只存哈希、secret 加密、老库补列这些边界都有覆盖。

跨实现验证：Go 实现删除**之前**实测过——这份二进制开在旧实现建出的老库上正常、它注册的账号
旧实现能登录、它发布的制品与注册的节点旧实现能列出 / discover。现在 Go 侧代码已删，
能复现的只有前半条（拿旧实现留下的库直接起）；后半条的结论只作历史记录。
