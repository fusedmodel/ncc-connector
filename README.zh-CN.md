# ncc-registry · 内网托管节点

> [English](README.md) | 中文

一个**单二进制**的内网 Registry 服务：把「**制品托管**」「**配置托管**」「**分享**」
「**节点托管**」「**Agent 发现与互联**」「**节点治理**」收在一个进程里，
并支持**多节点**（一个 `master` + 若干 `worker`）横向铺开。

它属于 [`ncc`](https://github.com/fusedmodel/ncc) 这个开源/可分发的部分：Go 单二进制、SQLite 单文件、
内置 Web 控制台，不依赖平台私有代码，也不引入外部数据库或对象存储就能跑。
变更历史见本仓库的 [`CHANGELOG.md`](CHANGELOG.md)。

```bash
go build -o dist/ncc-registry ./cmd/ncc-registry
NCCR_PORT=8282 ./dist/ncc-registry          # → http://localhost:8282
```

它同时是一个**可被 import 的 Go 库**（`module github.com/fusedmodel/ncc-registry`）——
想在自己的进程里起一个内网 Registry，见下方[「作为 Go 库使用」](#作为-go-库使用)。

## 两件事（能力总表）

| 能力 | 说明 | 主要接口 |
|---|---|---|
| **制品托管** | 账号 / 命名空间 / 发布 / 检索 / 下载 / 分发；`sha256` 校验、`@命名空间/slug` 稳定引用 | `/api/auth/*`、`/api/registry*` |
| **内网托管节点** | 用户把内网的 Agent / 服务**注册 + 心跳**托管进来，声明「我是谁、在哪、能干什么」 | `/api/nodes/heartbeat`、`/api/nodes` |
| **Agent 发现与互联** | 在同一信任域内发现彼此、收进连接表、按区域聚合；问「这个能力该找哪个节点要」 | `/api/nodes/discover`、`/api/nodes/links`、`/api/nodes/route` |
| **接入：key/secret 或内网短链** | 一条内网短链（或 key+secret）就能把一个 Agent 加进来；兑换出的是**最小权限的节点令牌** | `/api/access/*`、`/j/:key` |
| **授权：连接 ≠ 授权** | 私有制品 / 私有节点 / 非公开配置要显式 `grant`；撤销立即生效（支持按命名空间限定） | `/api/grants*` |
| **配置托管** | 团队的网络 / 基础设施 / Agent 配置作为一等资源：版本历史、回滚、按环境成组拉取、敏感值静态加密 | `/api/configs*` |
| **分享链接** | 把一条制品变成**临时下载地址**发出去：对方不用登录、不用装 CLI；可限次 / 限时 / 撤销 | `/api/shares*`、`/s/:token` |
| **Agent 名片（Agent Share）** | 把**我设计好的 Agent** 点到点交给指定的人：一条链接既能把包**装到对方本机**，也能把**节点收进对方的连接表**。与云端同形（同一份 `ncc agent` 客户端直接可用）；token 只存 sha256 | `/api/agent-cards*`、`/a/:token` |
| **云电脑（Remote Cloud Computer）** | 把本节点变成**替别人跑东西**的机器：登记沙箱环境后，HUR 包走 **wasm 沙箱**，OS 敏感任务（`docker build && docker push`）走 **process / 容器**。**默认只放行 wasm**，其余要运维显式开；每条任务都要 reason，日志/退出码/超时/截断如实回报，取消**进程组整组回收** | `/api/exec/kinds`、`/api/exec/runs*` |
| **连接通道（`ncc conn`）** | 到某台 Cloud instance 的一条**会话**：目标机上一个工作目录 + TTL + 账本。上面能反复执行命令、**推/拉文件**（锁在工作目录里），并把这一批动作（`conn run` = 推文件 + 跑脚本）一次做完。**默认关**（`NCCR_CONN_ALLOW=1` 才开）、每个动作都要 reason、**过期与关闭分开说**；执行复用云电脑那套执行器。⚠️ 客户端还有**SSH 通道**（`ncc conn --ssh`，目标端只要有 sshd）—— 那种通道**不需要本节点参与** | `/api/conn/connections*` |
| **节点管理（admin）** | 节点管理员管**用户 / 节点 / 服务**（禁用启停、重置密码、摘除、归档），每个动作都进审计 | `/api/admin/*` |
| **多节点（master/worker）** | worker 注册 + 心跳上报本地目录；master 聚合目录、做能力路由、代理字节，还能把制品**分发**到 worker 并在下架时**回收** | `/api/cluster*` |
| **打洞条件（P2P）** | 在**这台机器**上判断跨网可不可达：NAT 画像 + 与对端映射真实对打（0 字节）；可选开一个**只应答 STUN** 的可被打洞入口 | `/api/p2p/self`、`/api/p2p/check`、`/api/p2p/serve` |
| **运行轨迹（Trace）** | 采集**真跑过什么**（Agent 会话 / HUR 执行），用于**能力评估**（按制品版本看成功率、耗时、token、花费、人工结论）与**后训练数据集**导出（JSONL + 奖励/得分/切分）。默认私有、显式采集、默认只有摘要 | `/api/traces*` |
| **状态（kb / mem / ckpt）** | 托管 **Agent 自己的三样状态**：知识库（语料，可检索、改一次留一版）、记忆（键值 + TTL + 来源）、检查点（不可变快照 + 血缘）。**默认私有**（记忆没有公开档）；包用 `hur.json` 的 `state{}`（规则 R11）声明要哪些，字节住在本节点 | `/api/kb*` `/api/mem*` `/api/ckpt*` |
| **索引（Index）** | 接住平台推过来的**索引副本**（谁 / 在哪 / 能干什么 / 要什么，带频道与来源 id，幂等去重），并就地提供匹配查询 —— 内网不出网也能一句需求找人 | `/api/index`（`GET` 浏览 / `POST` 接收）、`/api/index/channels`、`/api/match` |

## 架构

```mermaid
flowchart TB
  subgraph net["一个内网 / 一个信任域"]
    M["master（权威节点）<br/>账号 · 制品 · 节点目录 · 集群视图<br/>+ 能力路由 / 字节代理"]
    W1["worker A（边缘托管点）<br/>自己的制品与节点"]
    W2["worker B"]
    N1["托管节点：alice 的 Agent"]
    N2["托管节点：某服务"]
  end
  CLI["ncc CLI / 任意 Agent"] -->|"login / join / status / publish / install"| M
  W1 -->|"POST /api/cluster/heartbeat（含本地目录）"| M
  W2 -->|"同上报"| M
  N1 -->|"POST /api/nodes/heartbeat"| M
  N2 -->|"POST /api/nodes/heartbeat"| W1
  M -. "GET <worker>/api/registry/@ns/slug/bytes（代理字节）" .-> W2
```

**权威在 master**：账号、制品、节点目录都以 master 为准；worker 是**边缘托管点**——
它自己也托管制品与节点，并定期把「我这里有什么」报给 master。客户端（CLI / Agent）
只要认识 master 一个地址：查目录看聚合结果，下载时 master 会去持有者那里把字节代理回来；
要让内网某个/全部 worker 也持有副本，master 把条目**分发**过去（见 §集群写）。

## 快速开始

### ① 单节点（最小形态）

```bash
cd ncc-registry
go build -o dist/ncc-registry ./cmd/ncc-registry
NCCR_DATA_DIR=./data ./dist/ncc-registry          # master，默认 :8282
# 控制台 http://localhost:8282（含「节点管理」区块）
```

### ② master + worker（多节点）

```bash
# 终端 1：master
NCCR_PORT=8282 NCCR_DATA_DIR=./data/master NCCR_NODE_NAME=office-master \
  NCCR_NODE_REGION=上海-内网 ./dist/ncc-registry

# 终端 2：worker（在另一台机器上就用它的内网 IP）
NCCR_ROLE=worker NCCR_PORT=8283 NCCR_DATA_DIR=./data/worker-a \
  NCCR_NODE_NAME=office-worker-a NCCR_NODE_REGION=上海-内网 \
  NCCR_MASTER_URL=http://127.0.0.1:8282 NCCR_HEARTBEAT=15s ./dist/ncc-registry
```

worker 启动即 `join`，之后每 `NCCR_HEARTBEAT` 心跳一次，把本地目录一并报上去。
master 侧 30s 清理一次失联 worker（`4 × NCCR_NODE_TTL`）及其目录。

> 字节放独立盘 / NAS：加 `NCCR_BLOB_DIR=/mnt/nas/ncc-blobs`（库存哪儿用 `NCCR_DB_PATH`），
> 见 [目录与存储](#目录与存储)。

### ③ CLI 接进来（Agent 插件视角）

```bash
N=./cli/target/release/ncc                    # 或装好的 ncc

# 登录某个 ncc-registry 节点
$N --base http://localhost:8282 registry login --email you@corp.com --password '***'

# 把这台机器作为一个节点托管进去（--daemon 常驻心跳）
$N registry join --kind agent --name my-mac --region 上海-内网 --capabilities mcp,api

# 看本节点 / 集群 / 我的节点
$N registry status
$N registry nodes --kind agent                # 发现本实例上可连接的节点
$N registry catalog                           # 聚合目录（本节点 + 各 worker）
$N registry route @alice/hotel-skill          # 这个能力该找哪个节点要
$N registry leave --name my-mac               # 下线我的节点

# 接入票据 / 分发 / 下架回收 / 授权
$N registry ticket create --label "给 alice 的 Agent" --uses 1 --expires 7
$N registry ticket list && $N registry ticket rm TK-…
$N registry replicate @alice/hotel-skill --to all
$N registry rm @alice/hotel-skill --yes
$N grant set --user @bob --kind artifact      # 连接 ≠ 授权：拿私有东西要显式授权
$N grant set --user @bob --kind node

# 分享：把一条制品变成临时下载地址（对方不用登录、不用装 CLI）
$N registry share create @alice/hotel-skill --label "给合作方" --uses 1 --expires 7
$N registry share list                        # 我发的（--all 需管理员）

# 节点治理（本节点第一个注册的账号就是管理员；机器用 admin key/secret）
$N registry admin login --key AK-… --secret …
$N registry admin overview | users | nodes | services | audit
$N registry admin disable bob@corp.com --note "违规发布"
$N registry admin rotate --label ops          # 轮换 admin key/secret（旧的立即失效）

# 既有的制品命令照旧可用（同一个 HTTP 契约）
$N publish --file ./hotel.SKILL.md --kind skill --name "Hotel Skill" --slug hotel-skill --replicate all
$N search skill --tag hotel
$N install @alice/hotel-skill                 # 即使是 worker 上的，也走 master 代理
```

### ④ 用一条短链把别人加进来

```bash
# 签发一次（key + secret + 内网短链；secret 只显示这一次）
$N registry ticket create --label "给 alice 的 Agent" --uses 1 --expires 7
#  → key NK-7F3A2C · secret ab1b98… · http://localhost:8282/j/NK-7F3A2C#ab1b98…

# 把链接发给对方，对方一条命令直接接入（含入网）
$N registry add 'http://localhost:8282/j/NK-7F3A2C#ab1b98…' --join --kind agent --region 上海-内网
# 或分开填 key/secret：
$N registry add --base http://localhost:8282 --key NK-7F3A2C --secret ab1b98… --join
```

浏览器打开链接会看到接入页（自动读取 fragment 里的 secret，给出可复制的命令 + 一键验证）。

## 概念与边界

- **托管节点 vs 集群节点**：托管节点是*用户内网里的东西*（Agent / 服务，`/api/nodes/*`）；
  集群节点是*跑 ncc-registry 的实例本身*（`/api/cluster/*`）。两者都在，但别混。
- **节点自己声明类型**：`service`（服务）/ `agent`（为人服务的 Agent）/ `assigned`（被分配的 Agent），
  在上报时给出（`ncc registry join --kind`），不是连接方决定的。
- **注册与心跳是同一件事**：第一次上报即注册，之后每次上报续租 `LastSeen`；
  在线与否由 `NCCR_NODE_TTL` 判定，**不落库**（避免心跳写放大）。
- **连接 ≠ 授权**：`/api/nodes/links`（`ncc nodes link`）只是我这边的「找得到」清单，
  同一实例即信任域，不需要对方审批；取私有制品仍要另配授权（见 Roadmap）。
- **目录权威不下放**：master 是目录权威；worker 上报的是「我这里有」的声明，
  聚合只用于发现与路由，不替代权威记录。
- **治理与资产是两套权**：`/api/admin/*`（管人、管节点）与普通接口（看/发制品、上报心跳）
  走**两套门禁**。管理员身份有两种，等价：本节点**第一个注册的账号**（自带 `IsAdmin`），
  或一把**机器凭据** `AK-…` + secret。别把治理权塞进作用域体系 —— 那是资产权。
- **分享 ≠ 授权**：分享是**临时放行**（按链接、可限次/限时/撤销，拿到字节即结束），
  授权是**长期按人**（`ncc grant`）。分享不改变制品本身的可见性。
- **归档 ≠ 删除**：管理员处理服务条目只改 `status=archived`（从目录消失），
  字节与版本历史保留 —— 删不删是条目归属者的事。

## 接入：key/secret 与内网短链

内网里加一个 Agent，不该要求对方去注册账号、拼 `--base`。签一张**接入票据**即可：

| 形态 | 长什么样 | 给谁用 |
|---|---|---|
| key + secret | `NK-7F3A2C` + 32 位 secret | 手工填（key 短、可念；secret 只显示一次，库里只存 sha256） |
| 接入短链 | `http://host:8282/j/NK-7F3A2C#<secret>` | 直接发链接，对方 `ncc registry add '<链接>' --join` 一条命令接入 |

- **secret 放 URL fragment（`#`）**：浏览器不会把它发给服务端，因此不进访问日志、不进 `Referer`。
  所以短链自带凭据，而服务端从未拥有完整凭据。
- 票据可限次（`--uses`）、可过期（`--expires`）、可删除；兑换记在 `used_count` 上。
- 兑换出的是**节点令牌**（JWT `kind=node`），作用域默认 `nodes:write, registry:read, registry:download`：
  只能续租**它自己的那个节点**、只能读公开制品，不能发布、不能进别人的命名空间。
- 兑换时可以顺手入网（请求带 `node` 字段）—— `ncc registry add … --join` 就是这一步。
- 节点归属**签发者**的命名空间（默认个人空间）：Agent 接进来后直接出现在你的「我的节点」里。

## 授权：连接 ≠ 授权

连接（`/api/nodes/links`）只解决「找得到」；要「拿得到」必须有 `Grant`：

| 类型 | 放开什么 |
|---|---|
| `artifact` | 拉取我命名空间下的私有 / 草稿制品（可按命名空间限定） |
| `node` | 在 discover 里看到并连接我的私有托管节点 |
| `config` | 读取我的非公开配置（配置托管，见下一节） |

```bash
ncc grant set --user @bob --kind artifact            # 全部命名空间的制品
ncc grant set --user @bob --kind artifact --ns @team # 只放开 @team
ncc grant set --user @bob --kind node                # 私有节点可见/可连
ncc grant set --user @bob --kind config              # 非公开配置可读（不可写）
ncc grant list            # 我给出的（--in 看别人给我的）
ncc grant rm <id>         # 撤销，立即生效
```

私有条目的字节地址是**短时签名地址**（`HMAC(secret, ref|exp)`，默认 10 分钟）：
因为 `ncc download` 拉字节时不会再带 `Authorization`，所以拿到元数据的那一刻服务端就给它一条
能自证的 URL —— 未授权者既拿不到元数据，也伪造不了签名。

## 分享：把一条制品变成临时下载地址

内网里要把一个产物给同事 / 给外部合作方看，最轻的做法不是「给他一个账号」，而是**一条链接**：

```bash
ncc registry share create @team/report --label "给合作方" --uses 1 --expires 7
#  → 说明页  http://host:8282/s/<token>
#    直链    http://host:8282/s/<token>/raw
```

| 入口 | 用途 | 计数 |
|---|---|---|
| `GET /s/<token>` | 说明页（这是什么、还能用几次、下载按钮） | 不计数 |
| `GET /s/<token>/raw` | 直接下发字节（`curl -OJ` / Agent） | **只有它计数** |
| `GET /s/<token>/raw?meta=1` | 只取元数据（`sha256` / 大小 / 引用） | 不计数 |

- **token 只存 sha256**（与接入票据的 secret 同规矩），32 位随机串，只在创建时返回一次。
- 可限次（`--uses`）、可过期（`--expires`）、可撤销（`ncc registry share rm`）：
  撤销 / 过期 / 用尽即失效（`410 share_expired`）。
- **创建分享不是提权**：只有本来就能读这条制品的人能分享它（否则 403）。
- 分享**不改变制品的可见性**：私有制品分享给 A，不代表 A 从此能搜到它 —— 那要 `ncc grant`。
- 管理员可以看全部分享（`ncc registry share list --all`）并撤销任意一条。

```bash
ncc registry share list            # 我发的（--all 需管理员）
ncc registry share info <链接>      # 看一条链接的状态（公开，不消耗次数）
ncc registry share rm <SH-…|链接>   # 撤销，立即失效
```

## 节点管理（admin）：用户 / 节点 / 服务

一台内网 registry 需要有人管：**谁在这台节点注册过、有哪些节点挂着、哪些服务在对外**。
这就是 `/api/admin/*`，进审计，且与普通接口是**两套门禁**。

### 谁能管（两种身份，等价）

| 身份 | 从哪来 | 怎么用 |
|---|---|---|
| **人** | 本节点**第一个注册的账号**（自动 `IsAdmin`） | `ncc registry login --email …` 后直接用 `ncc registry admin …` |
| **机器** | 管理员首次出现时**自动签发**的 `AK-…` + secret（可轮换） | `ncc registry admin login --key AK-… --secret …`（写入本机配置），或直接带 `X-NCC-Admin-Key`/`X-NCC-Admin-Secret` 头 |

```bash
# 注册本节点第一个账号时会打印一次 admin 凭据
ncc --base http://host:8282 register --email you@corp.com --password '***'
#   → 👑 admin key AK-XXXXXX · admin secret ****（只显示这一次）

ncc registry admin login --key AK-XXXXXX --secret ****   # 写进 ~/.ncc/config.json（0600）
ncc registry admin status                                # 我是不是管理员、本机凭据能不能用
ncc registry admin rotate --label ops                    # 轮换：新 secret 生效、旧的立即失效
```

### 管什么

| 对象 | 能做什么 | CLI |
|---|---|---|
| **用户** | 看全部账号（含被禁用的）；禁用 / 启用（旧令牌立即失效，本人登录会看到原因）；重置密码（服务端生成，只显示一次） | `admin users` · `admin disable\|enable` · `admin passwd` |
| **节点** | 看全部托管节点（含私有与离线，带归属者邮箱）；摘除任意节点（指向它的连接记录一并清理） | `admin nodes` · `admin rm-node <ND-…>` |
| **服务** | 一次看全两类：节点侧 `kind=service`（正在跑的）与制品侧 `kind=api`（声明/交付的接口）；**节点摘除 / 制品归档** | `admin services` · `admin rm-service <ND-…\|@ns/slug>` |
| **审计** | 谁在什么时候把谁怎么了（actor 是用户还是机器凭据、目标、IP、备注） | `admin audit [--action user.disable]` |

```bash
ncc registry admin overview
ncc registry admin users --q bob
ncc registry admin disable bob@corp.com --note "违规发布"
ncc registry admin passwd bob@corp.com          # → 新密码只显示这一次
ncc registry admin services                     # 节点侧 + 制品侧一起看
ncc registry admin rm-service @team/hotel-api   # 制品侧：归档（字节保留）
ncc registry admin audit --limit 20
```

两条硬规则（服务端强制）：**不能禁用自己的账号**（自锁保护），
**不能禁用最后一个可用管理员**（否则这台节点再也没人能管）。

> 控制台首页也有「节点管理（管理员）」区块：填上 admin key/secret 即可在网页里做上面这些事
> （凭据只存在本机浏览器 localStorage，不会发给其它域）。

## 配置托管（团队的网络 / 基础设施配置）

一个团队的网络段、网关、模型端点、CI 变量……**不是制品，也不该塞进制品**：它们会被反复修改、
需要版本与回滚、默认不能公开，而且经常夹着凭据。所以配置在这里是一等资源。

| | 制品 Artifact | 配置 Config |
|---|---|---|
| 形态 | 可分发的文件（字节进 blob） | 会被就地修改的文档（内容进库） |
| 默认可见性 | `public` | **`private`** |
| 迭代方式 | 换 `version` 再发一版 | **就地改 + 每次写入留一版历史** |
| 跨节点 | 可 fan-out 到 worker（副本 / 回收） | **不参与 fan-out**（权威数据，只在被指向的节点上维护） |
| 敏感值 | 公开即人可见 | `secret=true` → 内容**静态加密**，默认打码 |

```bash
ncc registry config kinds                       # 类型（network/gateway/infra/agent/ci/security…）+ 格式 + 环境
ncc registry config set @team/network --file ./network.yaml \
    --kind network --env prod --summary "内网网段/DNS/VLAN" --tags network,dns --note "初始版本"
ncc registry config list --mine                 # 我的全部（含私有；内容默认打码）
ncc registry config get @team/network           # 元数据 + sha256（不发明文）
ncc registry config get @team/network --reveal --out ./network.yaml   # 明文落盘
ncc registry config history @team/network       # 谁在什么时候改了什么
ncc registry config rollback @team/network --to 2                     # 回滚（作为新版本写回）
ncc registry config bundle --ns @team --env prod --out ./conf         # 整套拉取（Agent 的第一跳）
ncc registry config rm @team/network --yes
```

**权限三条判定**（缺一不可，服务端与 CLI 同一套）：

| 动作 | 需要什么 |
|---|---|
| 读公开配置 | `visibility=public` 且 `status=active` → 谁都能读 |
| 读非公开配置 | 作用域 `config:read` **且**（命名空间成员 **或** 拿到 `config` 授权） |
| 写入 / 回滚 / 删除 | 作用域 `config:write` **且** 命名空间成员（外部只有读授权，不给写） |

**给 Agent 的长效凭据**：签一张限定作用域的接入票据，兑换出来的节点令牌就只做这些事——
票据的 `Sub` 是**签发者本人**，所以 Agent 是「代表你在团队空间里管配置」，不是另开一个身份：

```bash
ncc registry ticket create --label agent-conf --scopes config:read,config:write,nodes:write
# 对方：ncc registry add '<短链>' --join
# 之后 Agent 就可以：ncc registry config set @team/network --file ./new.yaml --note "Agent 改的"
```

**敏感值不再靠自觉**：`--secret` 的配置在落库前用 AES-256-GCM 加密（密钥由本节点的
`jwt-secret` 派生）。因此备份库文件而不带 `jwt-secret` 是安全的；反过来说，**换机器或丢了数据目录
就解不开这些密文**（这是设计意图）。校验和按**明文**算，Agent 拿到明文后可以自己复核。

**成组拉取（bundle）**是 Agent 落地基础设施的第一步：`--env prod` 会同时命中 `prod` 与 `any`
（通用项），每条都带建议文件名（`team-network.prod.yaml`）与 `sha256`。默认**跳过 `secret` 配置**
—— 一次把凭据全下到磁盘不是好默认，要用就显式 `--secrets --reveal`。

> **权威位置**：配置是**被指向的那个节点**的库里的数据（与制品的 fan-out 不同）。
> 多节点共享配置请把 Agent 指向 master；CLI 在 worker 上操作时会给出提示。

## 集群写：分发（replicate）与回收（revoke）

```bash
ncc publish --file ./x.SKILL.md --kind skill --name X --replicate all   # 发布即分发到全部 worker
ncc registry replicate @alice/x --to office-worker-a                   # 事后补分发
ncc registry rm @alice/x --yes                                         # 下架 + 回收各处副本
```

- master 把「条目 + 短时签名地址」推给 worker（`POST /api/cluster/ingest`，集群 token 鉴权）；
- worker 自己去拉字节并**校验 sha256**，落成 `origin=replica` 的副本（本地不可改，改要走源头）；
- master 侧记一份分发台账（`replica_targets`）—— 下架时据此回收，**不依赖 worker 心跳是否已上报**
  （心跳有延迟，刚分发完就下架得能收干净）；全部回收成功才清账；
- 回收只删副本，worker 自己发布的条目不受影响（`revoke` 只动 `origin=replica` 的行）。

## 目录与存储

节点只用本地磁盘，**放哪里都能配**（内网部署最常见的诉求：字节放 NAS / 独立盘，库存本地 SSD）：

| 目录 | env | 默认 | 装什么 |
|---|---|---|---|
| 数据根 | `NCCR_DATA_DIR` | `./data` | `node-id`、`jwt-secret`，以及下面两项的默认落脚点 |
| 制品字节 | `NCCR_BLOB_DIR` | `<data>/blobs` | **上传写这里、下载从这里读**（经 `/blobs/*` 公开） |
| 库文件 | `NCCR_DB_PATH` | `<data>/ncc-registry.db` | SQLite 库（含 `-wal` / `-shm`） |

- **相对路径按数据根解析**，不是按当前工作目录 —— 换个目录启动不会忽地换地方；
  解析完成后统一转成**绝对路径**，启动日志与 `GET /api/meta` 里报出的就是真正生效的路径：

  ```console
  $ NCCR_DATA_DIR=/srv/ncc NCCR_BLOB_DIR=/mnt/nas/ncc-blobs NCCR_DB_PATH=/srv/ssd/ncc.sqlite ./ncc-registry
    数据根   /srv/ncc
    制品字节 /mnt/nas/ncc-blobs   （上传写这里，下载从这里读）
    库文件   /srv/ssd/ncc.sqlite
  ```

- 服务启动时会自动建目录（含库文件的父目录）；目录不可写时直接报错退出，不会静默回退。
- 字节目录里就是普通文件（文件名随机化），**可以直接拿系统工具看、拷、备份**：

  ```bash
  ls /mnt/nas/ncc-blobs                      # 每个制品一个文件
  ```

- **备份**：库 + 字节目录（可能在不同盘上），或整包 `NCCR_DATA_DIR`（默认布局下二者都在里面）。
  身份与密钥在数据根下，**丢了两样都会换身份**：`node-id` 变了在集群里就是个新节点。
- **共享/只读目录**：`NCCR_BLOB_DIR` 指向挂载的共享目录时，多个节点可以共看同一批字节，
  但“写入”仍各自都在自己那份（本服务不做多写者协调）。

## 配置（`NCCR_*`）

| 变量 | 默认 | 说明 |
|---|---|---|
| `NCCR_ROLE` | `master` | `master`（权威节点）\| `worker`（边缘托管点） |
| `NCCR_PORT` | `8282` | 监听端口（刻意与平台的 8181 错开，两者可同机共存） |
| `NCCR_DATA_DIR` | `./data` | 数据根（自动创建）：`node-id` + `jwt-secret`，以及下面两项的默认落脚点 |
| `NCCR_BLOB_DIR` | `<data>/blobs` | **制品字节目录**：上传写这里、下载从这里读（经 `/blobs/*` 公开）；相对路径按数据根解析 |
| `NCCR_DB_PATH` | `<data>/ncc-registry.db` | SQLite 库文件（可与数据根分开，比如库存本地 SSD、字节放 NAS） |
| `NCCR_PUBLIC_URL` | `http://localhost:<port>` | 别人怎么访问本节点（下载 URL、控制台、集群上报都用它） |
| `NCCR_NODE_NAME` | 主机名 | 节点名 |
| `NCCR_NODE_REGION` | 空 | 节点区域（如 `上海-内网`；发现与区域聚合按它分类） |
| `NCCR_NODE_ID` | 自动生成并持久化 | 本节点 id（改它等于换一个节点身份） |
| `NCCR_JWT_SECRET` | 自动生成并持久化 | HS256 密钥（生产建议显式设置） |
| `NCCR_JWT_TTL` | `168h` | 登录态有效期 |
| `NCCR_ACCESS_TTL` | `720h` | 接入票据兑换出的节点令牌有效期（票据本身带过期时间时取更短的那个） |
| `NCCR_MASTER_URL` | 空 | **worker 必填**：master 地址 |
| `NCCR_CLUSTER_TOKEN` | 空 | 配了则 worker 注册/心跳必须带 `X-NCC-Cluster-Token`；空 = 内网开放接入 |
| `NCCR_HEARTBEAT` | `15s` | worker 心跳间隔 |
| `NCCR_NODE_TTL` | `60s` | 托管节点/worker 的在线判定窗口（master 按 `4×` 清理 worker） |
| `NCCR_INVITE_CODE` | 空 | 空 = 内网开放注册；设了则注册必须带邀请码（逗号分隔多个） |
| `NCCR_CONSOLE` | `true` | 是否托管内置 Web 控制台 |
| `NCCR_P2P_SERVE` | `false` | 随服务开启**可被打洞入口**（一个 UDP socket，只应答 STUN Binding；默认关） |
| `NCCR_P2P_STUN` | 内置多台 | STUN 列表（逗号分隔）—— 用自己的可达 STUN，NAT 画像与打洞都靠它 |
| `NCCR_P2P_TURN` | 空 | 自托管 TURN 列表。**红线**：TURN 必须客户自托管 —— 云端托管面不进数据路径 |
| `NCCR_CONN_ALLOW` | `false` | **是否开放连接通道（`ncc conn`）**。通道能跑任意命令、写文件 = 最高权限，所以**默认关**；未开时建连一律 403 |
| `NCCR_CONN_DIR` | `<data>/conn` | 通道工作目录的根（每条通道一个子目录，文件面锁在里面） |
| `NCCR_CONN_TTL` | `1h` | 通道默认有效期（建连时 `ttlSec` 可覆盖，单次上限 8h） |
| `NCCR_CORS_ORIGINS` | 空 | 跨域白名单（逗号分隔，`*` 全放行） |

> 约定：`NCCR_*` 与平台的 `NCC_*` 互不干扰，两套服务可以并排跑在同一台机器上。

## API 速查

公开（读）：

| 方法/路径 | 说明 |
|---|---|
| `GET /api/health` · `GET /api/meta` | 存活与本节点自述（角色 / 节点 id / 规模 / 控制台地址） |
| `GET /api/meta` 的 `kind` 与 `capabilities` | **节点声明自己的能力**（`node`；`registry` / `config` / `share` / `nodes` / `grants` / `access` / `cluster` / `admin` / `p2p` / `trace` / `kb` / `mem` / `ckpt` / `store` / `index` / `exec` / `conn`）。CLI / MCP 按这份清单放行命令 —— 声明了 `services` / `profile` 那天，同名命令在本节点上就直接可用 |
| `GET /api/registry/kinds` | 制品类型与数量 |
| `GET /api/registry?q=&kind=&tag=&namespace=&page=&size=` | 目录检索（本节点权威） |
| `GET /api/registry/<@ns/slug\|A-…>` | 制品详情 |
| `GET /api/registry/<ref>/download` | 下载元数据（`url` / `sha256` / `size` / `via`） |
| `GET /api/registry/<ref>/bytes` | 真正的字节流（本节点有就发；没有就从 worker 代理） |
| `GET /api/nodes/discover?kind=&region=&q=` | 本实例上可连接的公开节点 |
| `GET /api/nodes/regions` | 区域覆盖（各区域在线 / 总数） |
| `GET /api/nodes/route?ref=` | 能力路由：谁持有这个制品 + 统一入口地址 |
| `GET /api/access/tickets/:key` | 票据概要（公开，不含 secret） |
| `GET /s/:token` | 分享落地页（公开，不计数） |
| `GET /s/:token/raw[?meta=1]` | 分享直链：下发字节（**计数**）/ 只看元数据（不计数） |
| `GET /api/shares/info/:token` | 一条分享的状态（公开，不计数） |
| `GET /api/configs/kinds` | 配置类型 / 格式 / 环境目录（含各类数量与上限） |
| `GET /api/configs?namespace=&kind=&env=&tag=&q=&page=&size=` | 配置目录（匿名只看公开；带凭据加自己的与被授权的） |
| `GET /api/configs/<@ns/slug\|C-…>?reveal=1&revision=N` | 取一份配置（**默认打码**；`reveal=1` 才出明文） |
| `GET /api/configs/<ref>/revisions` | 版本历史（含作者 / 变更说明 / 校验和） |
| `GET /j/:key` | 接入短链落地页（secret 在 fragment，服务端看不到） |
| `GET /api/cluster` · `GET /api/cluster/workers` | 集群总览（master + 各 worker） |
| `GET /api/cluster/directory?q=&kind=&tag=` | 聚合目录（本地 + 远端，条目带 `via`；本地条目带 `replicas`） |
| `GET /api/traces/kinds` | 轨迹词表与上限（类型 / 状态 / 内容级别 / 步骤类型 / 标注键 / 结论 / 切分） |
| `POST /api/index` · `GET /api/index?channel=&q=&prefix=&limit=` | 接收平台推来的**索引副本**（按来源 id 幂等）/ 浏览（频道可前缀匹配） |
| `GET /api/index/channels` · `GET /api/match?intent=&channel=&region=&kind=&want=` | 频道清单 / **就地匹配**（内网不出网也能用；响应里没有评分 —— 平台不向节点下发评分数据） |
| `GET /api/kb/kinds` | 知识库类型 / 格式 / 状态与上限（并**明说检索是关键词加权**） |
| `GET /api/kb?namespace=&kind=&tag=&q=&archived=&page=&size=` | 知识库目录。匿名只看公开；带凭据看「公开 ∪ 我的命名空间 ∪ 被授权的」（管理员 `all=1` 看全节点） |
| `GET /api/kb/<@ns/slug\|KD-…>?revision=N` | 一篇文档（含正文，`checksum` = 正文的 `sha256`）；`revision=N` 取历史版本 |
| `GET /api/kb/<ref>/revisions` | 版本历史（谁在什么时候改了什么，不含正文） |
| `GET /api/kb/bundle?namespace=&kind=&tag=` | 成组拉取（每篇带 `content` + `checksum`），给 Agent 落地用 |
| `GET /api/mem/kinds` · `GET /api/ckpt/kinds` | 记忆种类与上限 / 检查点粒度与上限 |

需登录（`Authorization: Bearer <JWT 或 ncc_ API-Key>`）：

| 方法/路径 | 说明 |
|---|---|
| `POST /api/auth/register` · `POST /api/auth/login` | 注册（自动开个人命名空间）/ 登录 |
| `GET /api/auth/me` · `PATCH /api/auth/me` | 当前身份 / 改名改密 |
| `GET\|POST\|DELETE /api/auth/keys[/:id]` · `GET /api/auth/key-scopes` | API-Key 与作用域 |
| `GET /api/namespaces/mine` · `POST /api/namespaces` | 我的命名空间 / 建组织命名空间 |
| `POST /api/registry/uploads` | 上传字节（raw body + `X-Filename`；响应含 `sha256`） |
| `POST /api/registry` · `PATCH/DELETE /api/registry/<ref>` | 创建 / 修改 / 删除条目 |
| `PUT /api/registry/<ref>/signature` | **加签**：给已发布的 `kind=hur` 制品附着/替换签名（只收 `signature` 对象；签在客户端做，本节点只做摘要核对并无损落盘） |
| `GET /api/nodes` | 我的托管节点 + 我连接的节点 |
| `POST /api/nodes/heartbeat`（别名 `POST /api/namespaces/living`） | 托管节点注册 + 心跳 |
| `DELETE /api/nodes/:id` | 下线我的节点 |
| `POST /api/nodes/links` · `PATCH\|DELETE /api/nodes/links/:id` | 连接 / 改 Name 标签 / 断开 |
| `GET /api/grants?direction=outgoing\|incoming` · `POST /api/grants` · `DELETE /api/grants/:id` | 分发授权（`artifact` \| `node` \| `config`）：连接 ≠ 授权 |
| `POST /api/access/redeem` | 用 key + secret 兑换节点令牌（可选同时入网：body 带 `node`） |
| `GET\|POST /api/access/tickets` · `DELETE /api/access/tickets/:id` | 签发 / 列出 / 删除接入票据 |
| `POST /api/cluster/replicate` | 把制品分发到 worker（`targets: "all"` 或名称/id 列表） |
| `POST /api/cluster/join` · `POST /api/cluster/heartbeat` | worker 注册 / 心跳（master 侧） |
| `POST /api/cluster/ingest` · `POST /api/cluster/revoke` | 节点间：落副本 / 回收副本（集群 token 鉴权） |
| `POST /api/configs` · `PATCH/DELETE /api/configs/<ref>` | 创建 / 改内容（加版本）/ 删除配置（需 `config:write` + 成员身份） |
| `POST /api/configs/<ref>/rollback` | 回滚到某一版（作为新版本写回，历史不改写） |
| `GET /api/configs/bundle?namespace=&env=&kind=&tag=&secrets=1&reveal=1` | 成组拉取（`env` 命中 `prod` 与 `any`；默认跳过 `secret`） |
| `POST /api/traces` · `GET /api/traces[?…]` · `GET /api/traces/:id` | 上传轨迹（按 `(命名空间, traceId)` 幂等）/ 列出 / 看一条（默认范围：我的 + 被授权的） |
| `POST /api/traces/:id/labels` · `GET /api/traces/stats` · `GET /api/traces/export` | 追加评测标注（只追加，绝不改写原轨迹）/ 聚合结论 / 导出数据集（JSONL + 数据集摘要） |
| `POST /api/kb` · `PATCH\|DELETE /api/kb/<ref>` | 写一篇（存在即新版本）/ 改名 / 归档 / 恢复 / 删除（需 `kb:write` **且**是命名空间成员） |
| `GET /api/mem/lookup?namespace=&subject=&key=` | 按 key 精确读一条记忆（Agent 读记忆的主路径） |
| `GET /api/mem?namespace=&subject=&prefix=&kind=&tag=&source=&pinned=&expired=&limit=` | 列出记忆（过期的默认不算，`expired=1` 才给；**记忆没有公开档**） |
| `PUT /api/mem` · `DELETE /api/mem/:id` · `POST /api/mem/gc` | 写一条（`(命名空间, subject, key)` upsert，Revision+1；`ttl_days>0` 设过期、`0` 清空）/ 删一条 / 真正清掉已过期的 |
| `POST /api/ckpt` · `PUT /api/ckpt/:id/blob` | 建检查点（`digest`+`size` 一起给，或两个都空 = 先建元数据）/ 传字节（服务端**重算 sha256 并拒收不符**；已有字节不能再传） |
| `GET /api/ckpt[?ref=&label=&tag=&q=&status=&limit=]` · `GET /api/ckpt/:id` | 列出 / 看一个（响应带**短时签名** `bytesUrl`） |
| `GET /api/ckpt/:id/bytes` · `GET /api/ckpt/:id/lineage` · `DELETE /api/ckpt/:id` | 字节流（签名地址或可读凭据）/ 沿 `parent` 回溯血缘 / 删除（元数据 + 字节） |
| `POST /api/ckpt/prune?ref=&keep=N` | 每个 subject 只留最新 N 个：其余标 `pruned`、删字节、**元数据留下**（历史不留无法解释的空洞） |
| `POST /api/shares` · `GET /api/shares[?mine=1\|all=1]` · `DELETE /api/shares/:id` | 建 / 列 / 撤销分享链接（`all=1` 需管理员；只能撤自己的，管理员可撤任意） |
| `POST/GET /api/conn/connections` · `GET/DELETE …/<id>` | 连接通道：建 / 列 / 看细节（**含账本**）/ 关闭（`?purge=1` 连工作目录一起删）。需 `NCCR_CONN_ALLOW=1` |
| `POST …/<id>/exec` · `POST/GET …/<id>/files?path=<相对>` | 在通道上执行（`{cmd,reason,…}`，默认同步返回并带日志尾巴）/ 推或拉文件（锁在通道工作目录里） |

打洞条件（P2P；判断面，**不搬运业务字节**，需登录）：

| 方法/路径 | 说明 |
|---|---|
| `GET /api/p2p/self` | 本节点 NAT 画像 + 结论 + ICE 配置 + 入口状态（在哪台机器上跑就看哪台） |
| `POST /api/p2p/check` `{peer, waitSec}` | 与一个已知映射地址真实对打（0 字节）；本地拿不到映射时返回 `503 p2p_probe_failed` |
| `GET /api/p2p/serve` | 可被打洞入口状态（`mapped` / `requestsTaken` / `responsesSeen` / `peers`） |
| `POST /api/p2p/serve` `{on, peer?}` | 开/关入口；`peer`（`ip:port`，可逗号分隔）是**反向打洞**对端，纯 `{on:true}` 不覆盖已配的 `peer` |

需**节点管理员**（会话管理员账号，或 `X-NCC-Admin-Key` + `X-NCC-Admin-Secret`）：

| 方法/路径 | 说明 |
|---|---|
| `GET /api/admin/overview` | 用户 / 管理员 / 节点（按类型）/ 服务 / 制品 / 配置 / 分享 / 审计 计数 |
| `GET /api/admin/users?q=&limit=&offset=` | 全部账号（含被禁用），带各自节点与制品数 |
| `PATCH /api/admin/users/:id` | 禁用 / 启用（`disabled` + `adminNote`） |
| `POST /api/admin/users/:id/password` | 重置密码（不给 `password` 则由服务端生成并只返回一次） |
| `GET /api/admin/nodes?kind=&region=&q=` | 全部托管节点（含私有与离线，带 `ownerEmail`） |
| `DELETE /api/admin/nodes/:id` | 摘除节点（并清理指向它的连接记录） |
| `GET /api/admin/services?source=all\|node\|artifact&q=` | 服务一览：`nodeServices`（kind=service）+ `apiArtifacts`（kind=api） |
| `DELETE /api/admin/services/:ref` | 服务处理：`ND-…` → 摘除节点；`@ns/slug` → 归档制品 |
| `GET /api/admin/audit?action=&limit=&offset=` | 审计日志 |
| `GET /api/admin/keys` · `POST /api/admin/keys/rotate?label=` | 机器凭据列表 / 轮换（新 secret 只返回一次，旧的立即失效） |

作用域：`registry:read|download|publish`、`nodes:read|write`、`config:read|write`、`trace:read|write|label`、
`kb:read|write`、`mem:read|write`、`ckpt:read|write`、`p2p:read|write`、`keys:write`（写蕴含读）。

错误体统一 `{"error":{"code":"…","message":"…"}}`，HTTP 状态码同步语义
（400 参数 / 401 未认证 / 403 无权限 / 404 不存在 / 409 冲突 / 413 过大 / 502 节点不可达）。

## 托管状态：知识库 / 记忆 / 检查点

三样属于 **Agent 自己**（不是属于某个包）的东西：它该照着读的语料、它该记住的小结论、它能交接或回滚的快照。

```bash
# 知识库 —— 内容进库、可检索、改一次留一版
ncc kb set refund --title "退款流程" --summary "标准步骤" --tag support \
    --content "1. 核对订单号  2. 确认到账方式"
ncc kb ls --namespace @team          # 或：ncc kb search 退款
ncc kb get @alice/refund             # 加 --revision 1 取历史版本
ncc kb archive @alice/refund         # 归档的默认不出现在列表与检索里

# 记忆 —— 键值，同键即更新，过期在**读时**生效
ncc mem set timezone "Asia/Shanghai" --source manual
ncc mem get timezone                 # Agent 读记忆的主路径
ncc mem set timezone "UTC+8"         # 同一个键 = 更新（Revision+1）
ncc mem set scratch "临时结论" --ttl-days 1
ncc mem gc                           # 真正删掉已过期的

# 检查点 —— 不可变快照 + 血缘
ncc ckpt save --name snap-1200 --ref @alice/agent --label run --file ./state.tar
ncc ckpt save --name snap-1400 --ref @alice/agent --file ./state2.tar --parent-last
ncc ckpt lineage <id>                # 一路回溯到起点
ncc ckpt pull <id> --out ./restore.tar   # **落盘前核对摘要**
ncc ckpt prune --ref @alice/agent --keep 5
```

**为什么这三样不是 HUR 包**（第一个会被问到的问题）：包是**能力** —— 代码 + 清单，内容寻址、有签名、
"装上就能跑"；状态是**数据** —— 会被改写、会长大、默认私有、生命周期跟着 Agent 走。
把知识库打进包会立刻自相矛盾：包的全部意义就是字节**不该变**（digest 证明"没被改"），
而知识库天经地义要变。包能做的是**声明**自己需要什么，于是"这个 Agent 需要哪些库、哪些记忆、
哪些检查点"本身也变成可签名、可分发的：

```jsonc
// hur.json（harness-use 包清单）—— 规则 R11
"state": {
  "kb":          [{ "ref": "@team/manual", "mode": "read" }],   // read | write | readwrite，或 "*"
  "memory":      { "subject": "self", "kinds": ["fact"], "ttl_days": 30 },
  "checkpoints": { "enabled": true, "label": "run", "keep_local": 2 }
}
```

`ncc kb pull --package ./agent` 就是两者的接缝：读声明、按 `checksum` 增量把那几个库拉到
`~/.ncc/kb`；包没声明就**明确拒绝**，不替它猜。（别与 `permissions.local.kb` 混：那个是**包内只读的本地文件**，
`state.kb` 是**节点上托管、会被改写、要授权**的库。）

**可见性**（与轨迹同一套，只差一处）：

| | 知识库 | 记忆 | 检查点 |
|---|---|---|---|
| 默认 | 私有 | **私有，且没有公开档** | 私有 |
| 公开档 | 有（`--public`，匿名可读） | 设计上就没有 | 有 |
| 谁能读 | 公开 ∪ 我的命名空间 ∪ 拿到 `state` 授权的 | 成员 ∪ 拿到 `state` 授权的 | 公开 ∪ 成员 ∪ 授权 |
| 谁能写 | 只有命名空间成员 | 只有命名空间成员 | 只有命名空间成员 |
| 管理员 | 要 `all=1` | 要 `all=1` | 要 `all=1` |

授权只开一个种类（`state`）覆盖三样：语义上它们是"我的 Agent 的状态"，先给一个粒度，
真要"只放知识库不放记忆"再拆。

**诚实边界**（详见 `ncc-platform/prd/ncc-state.md`）：检索是**关键词加权**（标题 3 / 摘要 2 / 正文 1），
**不是向量检索**；**不跨节点 fan-out**（状态住在你写进去的那台节点）；同步只有拉（按 checksum 增量）；
记忆是同键覆盖（后写胜）；检查点字节按原样存（不加密）—— 敏感内容别放进来。

## Web 控制台

`GET /` 是内置的单文件控制台（`httpapi/web/index.html`，随二进制 embed，无构建步骤）：
本节点身份与规模、集群 worker 列表（在线状态 / 制品数 / 最近心跳）、聚合目录（可搜索）、
托管节点发现（含区域覆盖）、**节点管理（管理员）**（填 admin key/secret 后可在网页里禁用账号、
重置密码、摘除节点、归档服务条目、撤销分享、轮换凭据），以及 CLI / HTTP 的接入速查。

## 部署

### Docker Compose（master + worker 示例）

```bash
cd deploy
docker compose up -d --build            # master :8282，worker :8283
```

`deploy/docker-compose.yml` 里两个服务共用同一镜像、不同 `NCCR_ROLE`；
数据分别落在具名卷里。生产上把 worker 部署到各内网机器，`NCCR_MASTER_URL` 指向 master 即可。

### 裸二进制 / systemd

```bash
NCCR_DATA_DIR=/var/lib/ncc-registry \
NCCR_NODE_NAME=office-master \
NCCR_NODE_REGION=上海-内网 \
NCCR_CLUSTER_TOKEN=<随机串> \
  /usr/local/bin/ncc-registry
```

单进程 + 单文件目录：备份 = 打包 `NCCR_DATA_DIR`（库 + `blobs/` + `node-id` + `jwt-secret`）。

## 作为 Go 库使用

```bash
go get github.com/fusedmodel/ncc-registry
```

公开包（`p2p` / `secretbox` 是内部实现，不对外）：

| 包 | 作用 |
|---|---|
| `config` | `Load()` 读 `NCCR_*` 环境变量（目录 / 身份 / 密钥都会落盘），也可自己填 `Config` 结构体 |
| `model` | 全部资源模型（用户 / 制品 / 节点 / 配置 / 分享 / 授权…） |
| `store` | `Open(path)` 打开 SQLite 并自动迁移；所有读写方法都挂在 `*Store` 上 |
| `storage` | `Storage` 接口 + `NewLocal` 本地磁盘驱动（换成 S3/Ceph 实现同一接口即可） |
| `httpapi` | `NewServer` / `NewRouter` —— 把上述几样组装成 HTTP 服务 |

最小嵌入（自己控配置、自己管生命周期）：

```go
package main

import (
	"log"
	"net/http"

	"github.com/fusedmodel/ncc-registry/config"
	"github.com/fusedmodel/ncc-registry/httpapi"
	"github.com/fusedmodel/ncc-registry/storage"
	"github.com/fusedmodel/ncc-registry/store"
)

func main() {
	// ① 想沿用环境变量就用 config.Load()，想自己造就直接填结构体。
	cfg, err := config.Load()
	if err != nil {
		log.Fatal(err)
	}

	st, err := store.Open(cfg.DBPath) // 内含 AutoMigrate，建表不用另做
	if err != nil {
		log.Fatal(err)
	}
	blob, err := storage.NewLocal(cfg.BlobDir, cfg.PublicURL)
	if err != nil {
		log.Fatal(err)
	}

	// ② NewServer 返回句柄 + 路由。句柄是用来在退出时收后台资源的
	//    （集群心跳 / 过期 worker 清理 / 可被打洞入口），别丢。
	srv, handler := httpapi.NewServer(cfg, st, blob)
	defer srv.Close() // 可重复调用；不关数据库 —— 谁 open 谁 close

	log.Fatal(http.ListenAndServe(cfg.Addr, handler))
}
```

想挂在自己的路由树里、或者只用一部分能力（比如只要 `store` 读写、
不要它自带的 HTTP 面），就按需取用上表里的包 —— 它们之间没有隐藏的全局状态。

> 注意：`NewRouter` 是 `NewServer` 的薄包装，只返回路由、不返回句柄。
> 进程退出时无所谓；但**反复创建**（测试、多实例）请用 `NewServer` + `Close`，
> 否则后台循环会一直漏。

## 冒烟测试

除了上面的集群冒烟（`scripts/smoke.sh`），`scripts/state-smoke.sh` 端到端跑**状态**这一整条链
（知识库 / 记忆 / 检查点）：构建本仓库、用独立端口与数据目录起一个节点、注册两个账号，
然后同时打 HTTP 与 `ncc` CLI —— 覆盖检查点取回时的摘要核对、字节不符被拒、TTL 读时过期、
按包声明拉取知识库、跨账号 403、"被授权者只有读"、以及**不碰真实 `~/.ncc`** 的隔离自检（80 项）。

```bash
bash scripts/smoke.sh      # 自带启停：master + worker 两节点，端口 18282/18283
```

覆盖：集群注册与心跳、制品注册/上传/发布/检索/下载、托管节点心跳与区域覆盖、
聚合目录（`via=worker`）、能力路由、**master 代理 worker 字节且 `sha256` 一致**、
**接入票据（短链形态 / 令牌最小权限 / 错 secret 与次数用尽被拒）**、
**授权（私有制品与私有节点：未授权不可见 → 授权后可见可取 → 撤销后立即失效）**、
**集群写（发布即分发 → worker 副本 sha256 一致且不可本地改 → 下架回收副本）**、
**存储目录可配置（字节/库/数据根各指一处，且默认布局不变）**。

当前 **167 项检查**，在 Linux / CI 上全绿。在 **Windows + Git Bash** 下会有 3 项失败：
那是脚本拿 `NCCR_DATA_DIR` 等的 Git-Bash 路径（`/tmp/xxx`）去比对接口回显的
Windows 绝对路径（`C:\Users\…`），属脚本的路径显示差异，不是被测行为 ——
所以 CI 跑在 ubuntu 上，本地 Windows 看到这 3 项红可以忽略。

## 与其它组件的关系

下面几个组件都在**别的仓库**里 —— 本仓库不依赖它们中的任何一个，只是共用同一套 HTTP 契约。

| 组件 | 在哪 | 关系 |
|---|---|---|
| `ncc` CLI（`cli/`） | [`fusedmodel/ncc`](https://github.com/fusedmodel/ncc) | 官方客户端。`ncc registry …` 是面向本服务的命令组；`publish/search/install/nodes/living` 等既有命令复用同一 HTTP 契约 |
| `@fusedmodel/ncc-cli` | 同上（`packages/ncc-cli`） | npm 包装，装的是同一个 `ncc` 二进制 |
| 平台侧 | [`fusedmodel/ncc-platform`](https://github.com/fusedmodel/ncc-platform)（私有） | 云端 Registry 与产品页。**本服务不依赖它**：契约同构、代码独立 |
| `agent/` | 同 `ncc` 仓库 | 把 NCC 能力以 MCP 暴露给任意 Agent 的 harness manifest（与本服务配合使用） |

## Roadmap（尚未实现，按需推进）

- **制品签名与版本锁定**：目前是 `sha256` 校验 + 版本号，未做发布者签名。
- **字节面增强**：本地磁盘 → S3 兼容对象存储（Ceph RGW / MinIO）；worker 侧缓存策略与失效。
- **跨网互联**：目前是同内网直连 HTTP；跨网需要打洞/中继。选型与实测已收敛：
  `pion/webrtc` + 控制面信令 + 客户自托管 TURN，见 `ncc` 仓库的 `prd/ncc-p2p-data.md`
  （实验装置在同一个仓库的 `spike/p2p-transport/`，本机实测直连建连 ~90ms / ~50 MB/s、relay-only 建连 ~2s）。
  **本节点已具备 P2P 判断面**：`/api/p2p/self|check|serve`（CLI：`ncc registry p2p self|check|serve`，
  `NCCR_P2P_SERVE=1` 随服务开入口）—— 在这台机器上出 NAT 画像、与对端映射真实对打、并可选开一个
  只应答 STUN 的可被打洞入口。**注意**：入口的 `peer`（对端映射）必须由信令下发才可长期可用
  （每个 socket 的映射都不同）；手写 `ncc registry p2p serve --peer ip:port` 只用于演示排障。
  实测结论：本机 NAT 过滤为 `address_and_port_dependent` 时**纯被动应答收不到任何包**，必须双方同时发。
  字节面（真正的传输）尚未接上，见 PRD 的 P2.2。
- **票据的可观测性**：票据使用记录（谁、何时、哪台机器兑换）目前只记最后使用时间与次数，
  没有逐次审计；节点令牌无法单独吊销（改票据作用域或换密钥需重签）。
