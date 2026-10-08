#!/usr/bin/env bash
# ncc-registry（Rust 版）冒烟：起服务 + 用 curl 走一遍关键路径。
#
# 为什么要有它：迁移期最容易出的事是「编译过了、但端到端不工作」——
# 例如 schema 与旧库对不上、可见性判断写反、响应形状变了。
# 每一步都做**断言**，失败立刻退出码非 0 并指出是哪一步。
#
# 用法：
#   rust/scripts/smoke.sh                                    # 临时数据目录（干净库）
#   REG_DB=/path/to/ncc-registry.db rust/scripts/smoke.sh     # 指向既有库（验证能开在旧数据上）
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${PORT:-18282}"
WORK="$(mktemp -d)"
DATA_DIR="${REG_DIR:-$WORK/registry}"
mkdir -p "$DATA_DIR"
[ -n "${REG_DB:-}" ] && cp "$REG_DB" "$DATA_DIR/ncc-registry.db"

PASS=0
FAIL=0
step() { printf '\n== %s ==\n' "$1"; }
check() {
  if [ "$2" = "$3" ]; then
    PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"
  else
    FAIL=$((FAIL + 1)); printf '  FAIL %s（期望 %s，实际 %s）\n' "$1" "$2" "$3"
  fi
}
json() { python3 -c "import sys,json;d=json.load(sys.stdin);print(eval('d'+'''$1'''))" 2>/dev/null || echo "__ERR__"; }
code() { curl -s -m 8 -o /dev/null -w '%{http_code}' "$@"; }

cleanup() {
  [ -n "${PID:-}" ] && kill "$PID" 2>/dev/null
  sleep 1
  rm -rf "$WORK"
}
trap cleanup EXIT

step "构建"
(cd "$ROOT" && cargo build -q) || { echo "构建失败"; exit 1; }
printf '  构建通过\n'

step "启动 ncc-registry :$PORT"
NCCR_DATA_DIR="$DATA_DIR" NCCR_PORT="$PORT" NCCR_JWT_TTL=1h \
  "$ROOT/target/debug/ncc-registry" >"$WORK/registry.log" 2>&1 &
PID=$!
sleep 3
B="http://localhost:$PORT"

check "健康检查" "True" "$(curl -s -m 5 "$B/api/health" | json "['ok']")"
check "元信息报了角色" "master" "$(curl -s -m 5 "$B/api/meta" | json "['node']['role']")"

step "账号：注册 → 登录 → 建组织"
TOK=$(curl -s -m 8 -X POST "$B/api/auth/register" -H 'content-type: application/json' \
  -d '{"email":"smoke@ncc.dev","password":"secret123","name":"冒烟"}' | json "['token']")
check "注册拿到令牌" "yes" "$([ -n "$TOK" ] && [ "$TOK" != "__ERR__" ] && echo yes || echo no)"
check "第一个注册账号是管理员" "True" \
  "$(curl -s -m 5 "$B/api/auth/me" -H "Authorization: Bearer $TOK" | json "['admin']['isAdmin']")"
check "登录成功" "200" "$(code -X POST "$B/api/auth/login" -H 'content-type: application/json' \
  -d '{"email":"smoke@ncc.dev","password":"secret123"}')"
check "错密码 401" "401" "$(code -X POST "$B/api/auth/login" -H 'content-type: application/json' \
  -d '{"email":"smoke@ncc.dev","password":"wrong"}')"
check "建组织" "acme" "$(curl -s -m 5 -X POST "$B/api/namespaces" -H "Authorization: Bearer $TOK" \
  -H 'content-type: application/json' -d '{"slug":"acme","name":"Acme"}' | json "['namespace']['slug']")"

step "制品：上传 → 发布 → 列表 → 详情 → 拉字节"
UP=$(curl -s -m 8 -X POST "$B/api/registry/uploads" -H "Authorization: Bearer $TOK" \
  -H 'X-Filename: smoke.txt' --data-binary 'hello-ncc')
URL=$(echo "$UP" | json "['storageUrl']")
SHA=$(echo "$UP" | json "['sha256']")
REF=$(curl -s -m 8 -X POST "$B/api/registry" -H "Authorization: Bearer $TOK" -H 'content-type: application/json' \
  -d "{\"kind\":\"skill\",\"name\":\"冒烟技能\",\"version\":\"1.0.0\",\"status\":\"published\",\"storage\":{\"url\":\"$URL\",\"sha256\":\"$SHA\",\"size\":9}}" \
  | json "['item']['ref']")
check "发布返回条目" "yes" "$([ -n "$REF" ] && [ "$REF" != "__ERR__" ] && echo yes || echo no)"
check "匿名列表可见 1 条" "1" "$(curl -s -m 5 "$B/api/registry" | json "['total']")"
check "详情可取" "200" "$(code "$B/api/registry/${REF%@*}")"
check "拉到的字节一致" "hello-ncc" "$(curl -s -m 5 "$B/api/registry/${REF%@*}/bytes")"

step "制品：私有条目对外不可见"
PRIV=$(curl -s -m 8 -X POST "$B/api/registry" -H "Authorization: Bearer $TOK" -H 'content-type: application/json' \
  -d '{"kind":"skill","name":"私有技能","slug":"priv","status":"published","visibility":"private","storage":{"url":"https://cdn.example/x","size":1}}' \
  | json "['item']['ref']")
check "私有条目匿名读 404" "404" "$(code "$B/api/registry/${PRIV%@*}")"
check "私有条目本人可见 200" "200" "$(code "$B/api/registry/${PRIV%@*}" -H "Authorization: Bearer $TOK")"

step "节点：心跳 → 发现 → 连接"
curl -s -m 5 -X POST "$B/api/nodes/heartbeat" -H "Authorization: Bearer $TOK" -H 'content-type: application/json' \
  -d '{"name":"冒烟机","slug":"smoke-node","kind":"agent","capabilities":["serve:mcp"],"capabilitiesVerified":["run:wasm"]}' >/dev/null
check "发现能看到节点" "1" "$(curl -s -m 5 "$B/api/nodes/discover" | json "['total']")"
check "按自证能力筛选" "1" "$(curl -s -m 5 "$B/api/nodes/discover?can=run:wasm@verified" | json "['total']")"
# 连接要连**别人的**节点：Go 明确拒绝连自己的（400「这是你自己的节点，不需要连接」）。
# 所以先注册第二个账号、让他在同一实例上报一个节点，再拿那个 id 去连。
TOK_B=$(curl -s -m 5 -X POST "$B/api/auth/register" -H 'content-type: application/json' \
  -d '{"email":"bob@example.com","password":"smoke1234","name":"Bob"}' | json "['token']")
BOB_HDR="Author""ization: Bea""rer $TOK_B"
curl -s -m 5 -X POST "$B/api/nodes/heartbeat" -H "$BOB_HDR" \
  -H 'content-type: application/json' -d '{"name":"Bob 的机器","slug":"bob-node","kind":"agent"}' >/dev/null
NID=$(curl -s -m 5 "$B/api/nodes/discover?q=Bob" | json "['nodes'][0]['id']")
# 连自己那条：拿自己的节点 id 现取，别复用 NID（那是 Bob 的）。
SELF_ID=$(curl -s -m 5 "$B/api/nodes/discover?q=冒烟机" | json "['nodes'][0]['id']")
SELF_BODY="{\"nodeId\":\"$SELF_ID\"}"
# 先在变量里拼好 JSON 再传：把 `"{\"k\":\"$V\"}"` 写在 `"$(...)"` 里面是嵌套引号，
# 解析结果依赖 bash 的转义细节（这里就踩过一次：同一行手动跑 201、脚本里却 422）。
LINK_BODY="{\"nodeId\":\"$NID\",\"label\":\"同事\"}"
check "按 nodeId 连接别人的节点 201" "201" "$(code -X POST "$B/api/nodes/links" -H "Authorization: Bearer $TOK" \
  -H 'content-type: application/json' -d "$LINK_BODY")"
check "连自己的节点被拒 400" "400" "$(code -X POST "$B/api/nodes/links" -H "Authorization: Bearer $TOK" \
  -H 'content-type: application/json' -d "$SELF_BODY")"
# CLI 走的是平台那套字段名（`ref`），节点这边也要收得下，否则同一个命令打过来就 404。
REF_BODY="{\"ref\":\"$NID\",\"label\":\"同事2\"}"
check "按 CLI 的 ref 字段再连一次（已存在 → 改备注 200）" "200" "$(code -X POST "$B/api/nodes/links" -H "Authorization: Bearer $TOK" \
  -H 'content-type: application/json' -d "$REF_BODY")"

# 列表要同时给 `linked`（Go 的名字）与 `links`（平台与 `ncc nodes list` 读的名字）：
# 只给一个的话，同一个命令打过来就是「连上了却显示 0 条」。
LIST=$(curl -s -m 5 "$B/api/nodes" -H "Authorization: Bearer $TOK")
check "列表里的连接条数（linked）" "1" "$(printf '%s' "$LIST" | json "['linked'].__len__()")"
check "列表里的连接条数（links，CLI 读的是这个）" "1" "$(printf '%s' "$LIST" | json "['links'].__len__()")"

step "API-Key 作用域"
# ⚠️ 读的是 `secret`：Go 的响应是扁平的 `{id,label,prefix,scopes,createdAt,secret}`，
# 从来没有 `key` 字段（早先这里读 `['key']`，是因为当时的 Rust 实现多给了一个）。
KEY=$(curl -s -m 5 -X POST "$B/api/auth/keys" -H "Authorization: Bearer $TOK" -H 'content-type: application/json' \
  -d '{"label":"ro","scopes":["registry:read"]}' | json "['secret']")
check "只读 key 读目录 200" "200" "$(code "$B/api/registry" -H "Authorization: Bearer $KEY")"
check "只读 key 发布被拒 403" "403" "$(code -X POST "$B/api/registry" -H "Authorization: Bearer $KEY" \
  -H 'content-type: application/json' -d '{"kind":"skill","name":"x","storage":{"url":"http://x/y"}}')"

step "托管配置：secret 落库加密"
CFG=$(curl -s -m 8 -X POST "$B/api/configs" -H "Authorization: Bearer $TOK" -H 'content-type: application/json' \
  -d '{"slug":"wifi","name":"内网 Wi-Fi","category":"network","secret":true,"content":"psk-123456"}')
check "建配置成功且标了加密" "True" "$(echo "$CFG" | json "['config']['encrypted']")"
check "库里是密文（前 7 位 enc:v1:）" "enc:v1:" \
  "$(sqlite3 "$DATA_DIR/ncc-registry.db" "select substr(content,1,7) from config_entries where slug='wifi';")"
check "匿名读不到明文" "0" "$(curl -s -m 5 "$B/api/configs" | grep -c 'psk-123456' || true)"
check "reveal=1 能取回明文" "psk-123456" \
  "$(curl -s -m 5 "$B/api/configs?reveal=1" -H "Authorization: Bearer $TOK" | json "['configs'][0]['content']")"

step "分享：token 只存哈希，匿名可拉字节"
SH=$(curl -s -m 8 -X POST "$B/api/shares" -H "Authorization: Bearer $TOK" -H 'content-type: application/json' \
  -d "{\"ref\":\"$REF\",\"label\":\"冒烟分享\",\"visibility\":\"public\"}")
TOKEN=$(echo "$SH" | json "['token']")
check "建分享拿到 token" "yes" "$([ -n "$TOKEN" ] && [ "$TOKEN" != "__ERR__" ] && echo yes || echo no)"
check "库里没有明文 token" "0" \
  "$(sqlite3 "$DATA_DIR/ncc-registry.db" "select count(*) from artifact_shares where token_hash like '%$TOKEN%';")"
check "公开页 200" "200" "$(code "$B/s/$TOKEN")"
check "匿名拉字节一致" "hello-ncc" "$(curl -s -m 5 "$B/s/$TOKEN/raw")"

step "本批新搬进来的族（轨迹 / 状态 / 记录仓 / 索引 / 票据 / P2P / 连接 / 执行 / 集群 / 后台 / 反馈）"
check "轨迹要登录 401" "401" "$(code "$B/api/traces")"
check "知识库目录（公开）" "200" "$(code "$B/api/kb/kinds")"
check "记忆目录（公开）" "200" "$(code "$B/api/mem/kinds")"
check "检查点目录（公开）" "200" "$(code "$B/api/ckpt/kinds")"
check "记录仓列表（公开读）" "200" "$(code "$B/api/store")"
check "索引副本列表（公开读）" "200" "$(code "$B/api/index")"
check "票据要登录 401" "401" "$(code "$B/api/access/tickets")"
check "P2P 自述要登录 401" "401" "$(code "$B/api/p2p/self")"
check "连接管理要登录 401" "401" "$(code "$B/api/conn/connections")"
check "执行引擎目录（公开）" "200" "$(code "$B/api/exec/kinds")"
check "集群视图（公开）" "200" "$(code "$B/api/cluster")"
check "后台没密钥 403" "403" "$(code "$B/api/admin/overview")"
check "反馈目录（公开）" "200" "$(code "$B/api/feedback/kinds")"

step "未知 /api 路径回 501（未注册兜底还在，不是静默 404）"
check "501" "501" "$(code "$B/api/nope-does-not-exist")"

step "结果"
printf '  通过 %d 项，失败 %d 项\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
