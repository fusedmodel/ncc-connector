# 更新日志（CHANGELOG）

> [English](CHANGELOG.md) | 中文

`ncc-registry` —— 内网托管节点：单二进制 + 可嵌入的 Go 库。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。
「怎么用」看 [`README.zh-CN.md`](README.zh-CN.md)；**本文件只回答「这一版比上一版多了什么」**。

> 本仓库原先寄居在 [`ncc`](https://github.com/fusedmodel/ncc) 的 `ncc-registry/` 目录下、
> 与 CLI 共用一份 CHANGELOG。独立出来后**从 `0.1.0` 起走自己的版本线** ——
> 所以 `0.1.0` 记的是「独立那一刻已经具备的全部能力」，不是新增功能。

---

## [未发布]

### 新增 · 连接通道（`ncc conn`）：通信基础设施

任务（`/api/exec/runs`）回答「跑一条命令」；通道回答「**在一台机器上连着干一段活**」。
一条通道 = 目标机上一个工作目录 + 一段有效期 + 一条审计线索，上面可以反复执行命令、推/拉文件。

- 接口：`POST/GET /api/conn/connections`、`GET|DELETE /api/conn/connections/<id>`、
  `POST …/<id>/exec`、`POST|GET …/<id>/files?path=<相对>`；能力位 `conn`，scope `conn:read|write`，
  审计动作 `conn.open|exec|put|close`。
- **执行器不重复写第二遍**：通道上的 `exec` 复用 `/api/exec/*` 那条路（`internal/execrun`：限额、
  环境变量白名单、进程组整组回收、超时、日志截断），只是记录多一个 `connId` —— 所以 `GET …/<id>`
  能回答「这条通道上跑过什么」。默认**同步**返回并**带上日志尾巴**（通道上一半用法是跑条命令看结果）。
- **默认关**：`NCCR_CONN_ALLOW=1` 才开（通道能跑任意命令、写文件 = 最高权限）；未开时建连 403，
  连字节都不收。相关配置：`NCCR_CONN_DIR`（默认 `<data>/conn`）、`NCCR_CONN_TTL`（默认 1h，单次上限 8h）。
- **文件面锁在工作目录**：只收相对路径，`..` / 绝对路径 / 空字节一律 400，拼完再复核前缀
  （`safeJoin`）。推文件返回 `sha256`，拉文件在响应头里回同一个指纹。
- **过期是推导的，不是状态翻转**：`state = open | closed | expired` 由 `Status` + `ExpiresAt` 算出。
  ⚠️ 一开始把过期改写成 `closed`，结果「过期」与「被人关掉」在接口上分不出来（都是 410、状态都是
  closed）—— 现已分开：`410 conn_expired` / `410 conn_closed`。
- **关闭 ≠ 删除**：`close` 后**账本仍然可读**（`GET …/<id>` 不再走「可用性守卫」，只做归属判断），
  可选项分开：`?purge=1` 删工作目录。
- 启动时只**数一下**已经过期的通道并打日志（`staleConns`），不再改写数据。
- 冒烟：`scripts/conn-smoke.sh`（57 项：默认关 / 文件面门禁 / 会话语义 / 指纹 / CLI 串联 / 关闭与过期分开）。

### 新增 · Agent 名片（Agent Share）：把「我设计好的 Agent」点到点交给指定的人

与云端的 `/api/agent-cards` **同形** —— 同一份 `ncc agent` 客户端把目标切到本节点就能用，客户端一行不用改。
接受的 Agent 可以只要一半：把包**装到本机**，或只把节点**收进连接表**。

- 接口：`POST/GET /api/agent-cards`、`GET /api/agent-cards/<token>[/blob]`、`POST …/accept`、
  `DELETE /api/agent-cards/<token>`（撤销），人读落地页 `GET|POST /a/<token>`（noindex）。
- **本仓规矩更保守**：`token` **只存 sha256**（与分享链接、接入票据一致），明文只在创建那一次回显；
  因此列表里回不出可点的链接 —— 如实给 hint，而不是编一个打不开的地址。
- 字节住 `s.Blob`（`agent-cards/<id>.hur`）；包内 `hur.json` 与 `sha256` 都由服务端**从收到的字节**
  算/读（不信客户端报的元信息），接受方核对指纹通过才装。
- 撤销 = **标记 revoked + 删字节**（记录留着：作者在 `ncc agent ls` 里看得到「已撤销」，
  对方点开得到 410 revoked 而不是含糊的 404）。
- ⚠️ 踩过的坑：**名额用完只该拦 `accept`** —— 一开始读名片 / 取字节也拦，结果 `--uses 1` 的名片
  刚 accept 完、紧接着下载就 410，等于把已经拿到名额的人关在门外（冒烟里钉了这条）。
- 冒烟：`scripts/agent-share-smoke.sh`（37 项，含“库里查不到 token 明文”）。

### 新增 · 云电脑（Remote Cloud Computer）：把本节点变成能接活的机器

一台云电脑 = 一台能替别人跑东西的 ncc 节点。调用方用 `ip/port/key` 把它登记成沙箱环境，
然后把 HUR 包或 OS 敏感任务丢过来跑。

- 接口：`GET /api/exec/kinds`（公开：每个引擎 `enabled`+`why`、runner 就绪、限额、标签）、
  `POST /api/exec/runs`（JSON=命令 / 原样字节=.hur）、`GET /api/exec/runs[/:id][/log]`、
  `DELETE /api/exec/runs/:id`（取消 / `?purge=1` 删记录与工作目录）。
- **默认只放行 `wasm`**（内置 HUR 沙箱）；`process`（在本机跑命令：docker build / 编译）与
  `container`（在镜像里跑）必须运维显式开 `NCCR_EXEC_ALLOW` —— 没放行的引擎**连字节都不收**（403）。
  引擎名写错**启动就报错**，不静默忽略。
- 每条任务 **reason 必填**（审计账本要能回答「谁让这台机器干了什么、为什么」）。
- 子进程环境变量是**白名单**（PATH/HOME/TMPDIR/LANG + `NCC_EXEC_*`）—— 绝不继承服务端
  env（那里面有 JWT secret、库路径）。
- 超时 / 取消 / 截断如实记：`timeout` 是独立状态；日志超上限继续跑但 `logTruncated=true`；
  取消走**进程组整组回收**（`sh -c "docker build…"` 被杀之后 docker 客户端不会还活着），
  且一旦 `canceled` 就不允许被后到的完成事件改回 `succeeded`。
- 节点在 `/api/meta` 的 node 里以**自证**报 `run:wasm/process/container`（由本机事实推导）
  → `ncc nodes discover --can run:container` 挑到的是真的能跑的机器。
- 冒烟：`scripts/exec-smoke.sh`（54 项）。

### 新增 · 索引（Index）：接住平台推来的副本，并能就地匹配

平台是索引的**权威**，节点是**副本** —— 内网不出网也能用一句需求找人。

- **`POST /api/index`** 接收平台推来的索引（`id` = 平台索引 id，作为 `SourceID` 幂等去重）：同一条重推
  是**更新**而不是又一条；带 `provider` / `source` 嵌套结构一并接住，字段缺失时从来源继承。
- **`GET /api/index`**（频道可前缀匹配、关键词、类型）与 **`GET /api/index/channels`**（频道清单与条数）、
  **`GET /api/match`**（本地打分：频道 / 分类 / 标签 / 正文词 / 区域 / 类型，**频道与区域不能单独召回**）。
- **评分不出平台**：节点的匹配排序**没有**信誉权重，响应里也明写了这一点 —— 不把不存在的数据装成有。
- 能力位 `index`（`GET /api/meta`）+ 作用域 `index:read` / `index:write`。

---


### 新增 · 托管状态：知识库 / 记忆 / 检查点

三样属于 **Agent**（而不是属于某个包）的东西。**有意不做成制品类型** —— 包是**能力**
（内容寻址、有签名、"装上就能跑"），状态是**数据**：会被改写、会长大、默认私有、有独立生命周期。
包只**声明**自己要什么（`state{}`，harness-use 规则 R11），字节住在本节点。

- **`kb` 知识库**（`model.KbDoc` + `KbRevision`）：命名空间下的文档（`@ns/slug`），带 type / format /
  summary / tags / `source`（这篇知识从哪来），**每次写入追加一版**（历史永不改写）；
  `checksum` 是正文的 `sha256`。检索是**关键词加权**（标题 3 / 摘要 2 / 正文 1）—— 接口里明写，
  因为管它叫"检索"却不说清是哪种，就是在误导。单篇上限 1 MB（知识库是**语料**，不是文件堆；
  大文件该走制品）。
- **`mem` 记忆**（`model.MemEntry`）：键值 + `subject`（谁的记忆：`self`、某条流水线…）+ `kind` +
  `tags` + `source`（trace id / 检查点引用 / 人手写）+ `confidence`（千分位，避免浮点）+ `pinned` +
  `revision` + `expiresAt`。唯一性在 `(命名空间, subject, key)`：同键再写就是**更新**（Revision+1）。
  TTL **读时判定**（过期即视为不存在），`gc` 才真正删掉。**记忆设计上就没有公开档** ——
  公开"记忆"本身不合语义，所以是"没有这一档"，不是"还没做"。单值上限 64 KB。
- **`ckpt` 检查点**（`model.Checkpoint`）：不可变快照 —— 字节进 blob、元数据进库 —— 带 `label`
  （episode / step / run / release / handoff / manual）、`step`、`subjectRef` + `subjectVersion`
  （对齐哪个制品版本）、`parent`（血缘，回溯带环保护）与自由 `meta`。创建时可以声明 `digest` + `size`，
  也可以两个都留空稍后传字节；**服务端上传时重算 `sha256` 并拒收不符**，已有字节的点不能再传。
  `prune` 每个 subject 只留最新 N 个：其余标 `pruned` 并只删字节 —— 元数据留下，历史不留无法解释的空洞。
- **可见性**：默认私有；`kb` 与 `ckpt` 有显式公开档，`mem` 没有；匿名只看公开文档，带凭据看
  「公开 ∪ 我的 ∪ 被授权的」，`all=1`（仅管理员）才扩到全节点。写永远要命名空间成员身份 ——
  被授权者只有读。store 层 **fail-closed**（没有可见范围就 `WHERE 1 = 0`），handler 再按行复核（纵深防御）。
- **新增授权种类 `state`** 覆盖这三样（有意只给一个粒度：语义上它们就是"我的 Agent 的状态"，
  真要"只放知识库不放记忆"再拆）。
- **新增能力 `kb` / `mem` / `ckpt`**，新增作用域 `kb:read|write`、`mem:read|write`、`ckpt:read|write`；
  `/api/meta` 报 `kbDocs` / `memEntries` / `checkpoints`。
- **新增接口**：`/api/kb`（`kinds`、`bundle`、列表、`POST`、`GET|PATCH|DELETE <ref>`、`<ref>/revisions`）、
  `/api/mem`（`kinds`、`lookup`、列表、`PUT`、`DELETE :id`、`gc`）、
  `/api/ckpt`（`kinds`、列表、`POST`、`PUT :id/blob`、`GET :id`、`:id/bytes`、`:id/lineage`、
  `:id/prune`、`DELETE :id`）。检查点字节走**短时签名地址**（域前缀 `ckpt:`，让一种资源的签名
  永远顶替不了另一种）**或**能读这条的凭据。
- **`scripts/state-smoke.sh`** 用独立节点 + 独立 `NCC_HOME` 端到端跑通（80 项断言）：取回时核对摘要、
  字节不符被拒、已有字节不能覆写、TTL 读时过期、按包声明拉取知识库、跨账号 403、
  "被授权者只有读"，以及**不碰真实 `~/.ncc`** 的自检。

### 新增 · 运行轨迹（Trace）：能力评估与后训练数据集

轨迹 = **真跑过什么**（Agent 会话 / HUR 执行）。同一份数据回答两件事：**这个版本好不好**
（成功率 / 耗时 / token / 花费 / 人工结论）与**能不能拿来训练**（JSONL 导出，带结论、奖励、切分）。

- **新文档规范 `ncc-trace/v1`**（`model/trace.go`）：`kind`（`agent` / `hur-run`）、
  `subject`（`ref` + `version` —— “改版之后变好没有”的分组键）、`steps`、`model`/`usage`、
  `labels`、`tags`、`payload`、`redaction`、`digest`。
- **`payload` 由采集方声明**：`digest`（默认，只有哈希与结构）/ `preview`（截断预览）/ `full`（原文）。
  服务端**只如实记录**，不补全也不降级；校验会拦“标成 digest 却带原文”的自相矛盾。
- **默认私有，没有“公开轨迹”这一档**：可见 = 我的命名空间 ∪ 被我授权 `trace` 的人；
  `mine=1` 收窄；管理员也要 `all=1` 才看全节点。
- **文档不可变 + 标注只追加**：采集方算 `digest`，服务端重算核对，不一致直接拒；
  评测标注进单独的 `trace_labels` 表 —— 打分永远不改写被判断的事实。
- **摘要跨语言一致**（`model.TraceDigestCore`）：用长度前缀拼接而不是 JSON 序列化
  （浮点、HTML 转义、键序在 Go 与 Rust 之间不保证一致），两端与 CLI 测试钉同一个 `sha256:` 向量。
- **新接口**：`GET /api/traces/kinds`、`POST /api/traces`（按 `(命名空间, traceId)` 幂等：
  同 id 同摘要 → `duplicates`；同 id 不同摘要 → `trace_conflict`）、`GET /api/traces`、
  `GET /api/traces/:id`、`POST|GET /api/traces/:id/labels`、`GET /api/traces/stats`
  （成功率 / 耗时分位 / token 与花费 / 按版本分组 / 标注覆盖率 / 结论分布 / 失败归类）、
  `GET /api/traces/export`（JSONL + 数据集摘要，被 `limit` 截断时回 `X-NCC-Truncated`）、
  `DELETE /api/traces/:id`。
- **新作用域** `trace:read` / `trace:write` / `trace:label`（`label` 刻意分开：采集是 Agent 日常，
  打分是一次评测动作）；**新授权种类** `trace`；`/api/meta` 新增 **`trace` 能力声明**，
  能力词表新增 `trace`（“这台节点收运行轨迹”，别名 `traces` / `telemetry`）。
- **上限**：单条 2 MB、2000 步、单批 500 条、单次导出 20000 条；被截断时如实上报，不静默少给。
- 测试：`model/trace_test.go`（摘要向量、校验、聚合）与 `store/trace_test.go`
  （幂等、冲突、fail-closed 可见性、过滤、导出截断、标注投影）。

### 变更 · `hur` 的 kind 标签对齐写死的 HUR 定义

`HUR` = **Harness-Use Runtime** —— 那是**运行时**（定义：支撑 harness 完成 LLM 调用、工具编排、
上下文管理、多 provider 接入、运行评估等任务）；`kind=hur` 的条目是**给它用的包**。
所以标签不再把 HUR 本身叫“包规范”，两端现在给出一模一样的字符串：
`Harness-Use Runtime 官方包（kind=agent 的包就是一个 Agent）`。
定义出处：`ncc-platform/prd/ncc-harness.md` §0 📌（写死）。

### 新增 · HUR 制品：上传校验与「加签」

- **`PUT /api/registry/<ref>/signature`**（需 `registry:publish`）：给**已发布**的 `kind=hur` 制品
  附着/替换签名。只收 `signature` 对象而不收整个 manifest —— 产物字节没变，收 manifest 就等于
  允许顺手改权限面与产物摘要，那是「换包」不是「加签」。
- **入库校验（`httpapi/hursign.go`）**：`kind=hur` 现在要求**自描述**（`manifest.hur` 的
  `spec` / `id` / `artifact.sha256`），并做两条交叉核对：
  - 上传字节 sha256 ≠ `manifest.hur.artifact.sha256` → `digest_mismatch`；
  - `signature.sha256` ≠ 制品摘要 → `signature_mismatch`；
  - `keynum` 缺失 / 非 minisign / 有 `url` 无 `sigSha256` → `bad_signature`。
- **刻意不做**：不验密码学（要 Minisign 全套 + 受信公钥列表，属下载方判断）、
  **不用本节点密钥代签** —— 「谁签的」必须由发布者自己的设备说了算。
- **副本只读**：分发到本节点的副本不能在本地加签（`replica_readonly`），要走源头节点。
- 控制台目录行为 `kind=hur` 且带签名的条目加上「已签名（keynum…）」徽章（本地条目才有
  manifest；worker 上报的远端条目不带，所以不显示，而非表示未签）。

## [0.1.0] — 2026-09-24

首个独立版本：仓库从 `ncc` 拆出来（`module github.com/fusedmodel/ncc-registry`），
既发布二进制，也发布可被 `go get` 的库。

### 变更 · 拆分为独立仓库与独立库

- **module 路径**：`github.com/fusedmodel/ncc/ncc-registry` → `github.com/fusedmodel/ncc-registry`。
  旧路径下所有代码都在 `internal/`，**模块外一个包也 import 不到** —— 也就是说作为库它从来就是不可用的。
- **五个包提到顶层**，成为公开 API：`config` · `model` · `storage` · `store` · `httpapi`；
  `p2p` 与 `secretbox` 留在 `internal/`（`httpapi` 照旧 import 它们，同模块内合法，外部拿不到）。
- **新增 `httpapi.NewServer` 与 `(*Server).Close`**：`NewRouter` 只返回路由，拿不到句柄，
  于是它启的后台循环（worker 心跳 / master 清理过期 worker / 可被打洞入口）没有任何停止机制 ——
  进程退出无所谓，但作为库反复创建就会一直漏协程。`NewRouter` 保留原签名，内部委托给 `NewServer`。
- 仓库自带 CI（gofmt / vet / build / 冒烟）与 Release（六个平台的二进制 + `checksums.txt` + GHCR 镜像）。

### 修复 · 二进制入口 `cmd/ncc-registry` 一直不存在

`README.md`、`deploy/Dockerfile`、`scripts/smoke.sh` 三处都在 `go build ./cmd/ncc-registry`，
但**这个目录从来没有被提交过**（初始提交 22 个文件全是 `internal/`）—— 也就是说
文档里的构建命令一直是坏的，二进制压根编不出来。现已补上 `cmd/ncc-registry/main.go`
（`config.Load` → `store.Open` → `storage.NewLocal` → `httpapi.NewServer`，含优雅退出）。

### 修复 · Windows 下制品 URL 带反斜杠（`storage.Local`）

`safeName` 用 `filepath.Clean` 清洗对象名，而对象名是**斜杠分隔**的标识（它要进 URL、
也要在 master / worker 之间原样传递）。Windows 上 `filepath.Clean("/a")` 得到 `\a`，
`TrimPrefix(clean, "/")` 再剥不掉那个反斜杠，于是 `PublicURL` 产出 `…/blobs/\a` 这种非法地址；
把它填进 JSON 请求体就变成 400。现改用 `path`（斜杠语义）清洗、只在落盘时 `filepath.FromSlash`。
顺带拒绝含 `\` 或 `:` 的对象名。**macOS / Linux 上行为完全不变。**

### 修复 · 冒烟脚本在 Windows 上的 3 项路径断言

`scripts/smoke.sh` 拿 `NCCR_*` 的 Git-Bash 路径（`/tmp/xxx`）比对接口回显的
Windows 绝对路径（`C:\Users\…`），必然不等。属脚本的路径显示差异，不是被测行为；
CI 跑在 ubuntu 上不受影响。当前共 **167 项检查**。

### 能力 · 制品托管与多节点

- 账号 / 命名空间 / 发布 / 检索 / 下载 / 分发；`sha256` 校验、`@命名空间/slug` 稳定引用。
- **master / worker**：权威在 master（账号、制品、节点目录）；worker 是边缘托管点，
  自己也托管制品与节点并定期上报目录。客户端只认 master 一个地址 ——
  查目录看聚合结果，下载时 master 去持有者那里把字节代理回来。
- **集群写**：把条目**分发**（replicate）到 worker，下架时**回收**（revoke）副本。
- 存储三处可分别指定：`NCCR_DATA_DIR` / `NCCR_BLOB_DIR` / `NCCR_DB_PATH`
  （内网常见诉求：字节放 NAS、库存本地 SSD）。

### 能力 · 托管节点与 Agent 发现

- 用户把内网的 Agent / 服务**注册 + 心跳**托管进来，声明「我是谁、在哪、能干什么」。
- 同一信任域内互相发现、收进连接表、按区域聚合；`/api/nodes/route` 回答
  「这个能力该找哪个节点要」。

### 能力 · 接入与授权（连接 ≠ 授权）

- 一条内网短链（或 key+secret）就能把一个 Agent 加进来，兑换出的是**最小权限的节点令牌**。
- 私有制品 / 私有节点 / 非公开配置要显式 `grant`，撤销立即生效（支持按命名空间限定）。

### 能力 · 配置托管

- 团队的网络 / 基础设施 / Agent 配置作为一等资源：版本历史、回滚、按环境成组拉取。
- 10 种类型、7 种格式、单条上限 128 KB；`secret=true` 的配置**静态加密**
  （AES-256-GCM，密钥由本节点 `jwt-secret` 派生，密文前缀 `enc:v1:`）——
  只备份数据库是安全的；反过来换机器 / 丢数据目录就解不开（设计意图，不是缺陷）。

### 能力 · 分享链接与节点治理

- **分享**：把一条制品变成临时下载地址发出去，对方不用登录、不用装 CLI；可限次 / 限时 / 撤销。
- **治理（admin）**：本节点第一个注册的账号自动成为管理员，同时签发机器凭据 `AK-…`；
  管用户 / 节点 / 服务（禁用启停、重置密码、摘除、归档），每个动作都进审计。

### 能力 · 节点侧 P2P（打洞条件判断面）

- `GET /api/p2p/self`：在**这台机器**上出 NAT 画像与结论；`POST /api/p2p/check`：
  与一个已知映射地址**真实对打**（0 字节，不传业务）；`GET|POST /api/p2p/serve`：
  开一个只应答 STUN Binding 请求的可被打洞入口（默认**关**，`NCCR_P2P_SERVE=1` 才随服务启动）。
- **实测结论**：纯被动应答在「地址/端口相关过滤」的 NAT 上收不到任何包 ——
  过滤孔必须自己先发才开，所以入口默认带**反向打洞**（每 300 ms 向对端发一个 Binding 请求）。
- 字节面（真正的数据传输）尚未接上，见 `ncc` 仓库的 `prd/ncc-p2p-data.md`。
