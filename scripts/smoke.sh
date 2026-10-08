#!/usr/bin/env bash
# ncc-registry 冒烟测试：一个 master + 一个 worker，跑通
#   制品托管（注册 / 上传 / 发布 / 检索 / 下载）
#   Agent 托管节点（注册 + 心跳 + 发现）
#   多节点（worker 注册到 master、目录聚合、能力路由、master 代理 worker 的字节）
#
# 脚本自带启停，不依赖外部已跑的实例；端口用 18282/18283 以免撞上开发实例。
#
# 用法：bash scripts/smoke.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT_M="${PORT_M:-18282}"
PORT_W="${PORT_W:-18283}"
MASTER="http://127.0.0.1:${PORT_M}"
WORKER="http://127.0.0.1:${PORT_W}"
TMP="$(mktemp -d)"
WORK="${TMP}/work"

PASS=0
FAIL=0

cleanup() {
  [[ -n "${PID_M:-}" ]] && kill "${PID_M}" 2>/dev/null || true
  [[ -n "${PID_W:-}" ]] && kill "${PID_W}" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT

say()  { printf '\n\033[1m%s\033[0m\n' "$1"; }
good() { printf '  \033[32m✓\033[0m %s\n' "$1"; PASS=$((PASS + 1)); }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$1"; FAIL=$((FAIL + 1)); }

# check <描述> <期望> <实际>
check() {
  if [[ "$2" == "$3" ]]; then good "$1（$3）"; else bad "$1：期望 $2，实际 $3"; fi
}

# check_code <描述> <期望状态码> <curl 参数…> —— 不匹配时把响应体一并打出来，便于定位
BODY_FILE="$(mktemp)"
check_code() {
  local desc="$1" want="$2"
  shift 2
  local got
  got="$(curl -sS -o "${BODY_FILE}" -w '%{http_code}' "$@")"
  if [[ "${got}" == "${want}" ]]; then
    good "${desc}（${got}）"
  else
    bad "${desc}：期望 ${want}，实际 ${got}  body=$(head -c 200 "${BODY_FILE}")"
  fi
}

# jval <dot.path> —— 从 stdin 的 JSON 里取值（数组用数字段，如 items.0.id）
jval() {
  python3 -c '
import json, sys
d = json.load(sys.stdin)
for p in sys.argv[1].split("."):
    if p == "":
        continue
    d = d[int(p)] if isinstance(d, list) else d[p]
print(d if not isinstance(d, (dict, list)) else json.dumps(d, ensure_ascii=False))
' "$1"
}

# wait_http <url> <超时秒>
wait_http() {
  local url="$1" deadline=$((SECONDS + $2))
  while (( SECONDS < deadline )); do
    if curl -fsS "$url" >/dev/null 2>&1; then return 0; fi
    sleep 0.3
  done
  return 1
}

mkdir -p "${WORK}"

say "0. 构建 + 启动两节点（master:${PORT_M} · worker:${PORT_W}）"
( cd "${ROOT}/rust" && cargo build --release -q --bin ncc-registry && cp target/release/ncc-registry "${TMP}/ncc-registry" )

NCCR_PORT="${PORT_M}" NCCR_DATA_DIR="${TMP}/m" NCCR_NODE_NAME="smoke-master" \
  NCCR_NODE_REGION="测试-内网" \
  NCCR_BLOB_DIR="${TMP}/blobs-master" NCCR_DB_PATH="${TMP}/db/master.sqlite" \
  NCCR_P2P_STUN="127.0.0.1:9" \
  "${TMP}/ncc-registry" >"${TMP}/master.log" 2>&1 &
PID_M=$!

# worker 不配目录：跑默认布局（<data>/blobs），顺便验证默认值没变。
NCCR_ROLE=worker NCCR_PORT="${PORT_W}" NCCR_DATA_DIR="${TMP}/w" NCCR_NODE_NAME="smoke-worker" \
  NCCR_NODE_REGION="测试-内网" NCCR_MASTER_URL="${MASTER}" NCCR_HEARTBEAT=2s \
  "${TMP}/ncc-registry" >"${TMP}/worker.log" 2>&1 &
PID_W=$!

wait_http "${MASTER}/api/health" 15 && good "master 已就绪" || { bad "master 未就绪"; cat "${TMP}/master.log"; exit 1; }
wait_http "${WORKER}/api/health" 15 && good "worker 已就绪" || { bad "worker 未就绪"; cat "${TMP}/worker.log"; exit 1; }

say "1. 集群：worker 是否注册到 master"
for _ in $(seq 1 20); do
  WCOUNT="$(curl -sS "${MASTER}/api/cluster" | jval totals.workers || echo 0)"
  [[ "${WCOUNT}" == "1" ]] && break
  sleep 0.5
done
check "master 看到 1 个 worker" "1" "${WCOUNT}"
ROLE_W="$(curl -sS "${WORKER}/api/cluster" | jval role)"
check "worker 自述角色" "worker" "${ROLE_W}"
MASTER_ONLINE="$(curl -sS "${WORKER}/api/cluster" | jval master.online)"
check "worker 认为 master 在线" "True" "${MASTER_ONLINE}"

say "2. 制品托管：在 master 上注册 / 上传 / 发布 / 检索 / 下载"
curl -sS -X POST "${MASTER}/api/auth/register" -H 'Content-Type: application/json' \
  -d '{"email":"alice@corp.com","password":"smoke1234","name":"Alice"}' >"${TMP}/reg-m.json"
TOK_M="$(jval token <"${TMP}/reg-m.json")"
NS_M="$(curl -sS "${MASTER}/api/namespaces/mine" -H "Authorization: Bearer ${TOK_M}" | jval namespaces.0.slug)"
[[ -n "${TOK_M}" ]] && good "master 注册成功（@${NS_M}）" || { bad "master 注册失败"; cat "${TMP}/reg-m.json"; }

printf '# Hotel Skill\n\n冒烟测试用的能力制品。\n' >"${WORK}/hotel.SKILL.md"
UP_M="$(curl -sS -X POST "${MASTER}/api/registry/uploads" \
  -H "Authorization: Bearer ${TOK_M}" -H 'X-Filename: hotel.SKILL.md' \
  --data-binary @"${WORK}/hotel.SKILL.md")"
URL_M="$(printf '%s' "${UP_M}" | jval storageUrl)"
SHA_M="$(printf '%s' "${UP_M}" | jval sha256)"
[[ -n "${URL_M}" ]] && good "master 上传字节成功" || { bad "master 上传失败"; printf '%s\n' "${UP_M}"; }

CREATE_M="$(curl -sS -X POST "${MASTER}/api/registry" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d "{\"kind\":\"skill\",\"name\":\"Hotel Skill\",\"slug\":\"hotel-skill\",
  \"version\":\"1.0.0\",\"summary\":\"冒烟测试\",\"tags\":[\"smoke\",\"hotel\"],
  \"status\":\"published\",\"visibility\":\"public\",
  \"storage\":{\"url\":\"${URL_M}\",\"sha256\":\"${SHA_M}\",\"size\":43}}")"
REF_M="$(printf '%s' "${CREATE_M}" | jval item.ref)"
[[ -n "${REF_M}" ]] && good "master 发布制品成功：${REF_M}" || { bad "master 发布失败"; printf '%s\n' "${CREATE_M}"; }

FOUND="$(curl -sS "${MASTER}/api/registry?q=hotel&kind=skill" | jval total)"
check "master 目录检索命中 1 条" "1" "${FOUND}"
DL_SHA="$(curl -sS "${MASTER}/api/registry/@${NS_M}/hotel-skill/download" | jval sha256)"
check "master 下载元数据 sha256 一致" "${SHA_M}" "${DL_SHA}"

say "3. 托管节点：在 master 上把一个 Agent 节点托管进来 + 发现"
NODE_JSON="$(curl -sS -X POST "${MASTER}/api/nodes/heartbeat" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' \
  -d '{"name":"alice-agent","kind":"agent","region":"测试-内网","capabilities":["mcp","api"],"os":"darwin","arch":"arm64"}')"
NODE_ID="$(printf '%s' "${NODE_JSON}" | jval node.id)"
NODE_KIND="$(printf '%s' "${NODE_JSON}" | jval node.kind)"
NODE_STATUS="$(printf '%s' "${NODE_JSON}" | jval node.status)"
check "托管节点已注册（kind=agent）" "agent" "${NODE_KIND}"
check "托管节点在线" "online" "${NODE_STATUS}"
[[ -n "${NODE_ID}" ]] && good "节点 id：${NODE_ID}" || bad "节点注册失败"

REGION_ONLINE="$(curl -sS "${MASTER}/api/nodes/regions" | jval regions.0.online)"
check "区域覆盖里有 1 个在线节点" "1" "${REGION_ONLINE}"

say "4. 多节点：在 worker 上发布制品，看 master 能否聚合与代理字节"
curl -sS -X POST "${WORKER}/api/auth/register" -H 'Content-Type: application/json' \
  -d '{"email":"bob@corp.com","password":"smoke1234","name":"Bob"}' >"${TMP}/reg-w.json"
TOK_W="$(jval token <"${TMP}/reg-w.json")"
NS_W="$(curl -sS "${WORKER}/api/namespaces/mine" -H "Authorization: Bearer ${TOK_W}" | jval namespaces.0.slug)"
[[ -n "${TOK_W}" ]] && good "worker 注册成功（@${NS_W}）" || { bad "worker 注册失败"; cat "${TMP}/reg-w.json"; }

printf '# Edge Skill\n\n只在 worker 上存在的能力制品。\n' >"${WORK}/edge.SKILL.md"
SIZE_W="$(wc -c <"${WORK}/edge.SKILL.md" | tr -d ' ')"
UP_W="$(curl -sS -X POST "${WORKER}/api/registry/uploads" \
  -H "Authorization: Bearer ${TOK_W}" -H 'X-Filename: edge.SKILL.md' \
  --data-binary @"${WORK}/edge.SKILL.md")"
URL_W="$(printf '%s' "${UP_W}" | jval storageUrl)"
SHA_W="$(printf '%s' "${UP_W}" | jval sha256)"
CREATE_W="$(curl -sS -X POST "${WORKER}/api/registry" -H "Authorization: Bearer ${TOK_W}" \
  -H 'Content-Type: application/json' -d "{\"kind\":\"skill\",\"name\":\"Edge Skill\",\"slug\":\"edge-skill\",
  \"summary\":\"只在 worker 上\",\"tags\":[\"smoke\",\"edge\"],
  \"status\":\"published\",\"visibility\":\"public\",
  \"storage\":{\"url\":\"${URL_W}\",\"sha256\":\"${SHA_W}\",\"size\":${SIZE_W}}}")"
REF_W="$(printf '%s' "${CREATE_W}" | jval item.ref)"
[[ -n "${REF_W}" ]] && good "worker 发布制品成功：${REF_W}" || { bad "worker 发布失败"; printf '%s\n' "${CREATE_W}"; }

# master 的聚合目录要能看到 worker 的那条（等 worker 下一次心跳）
VIA=""
for _ in $(seq 1 30); do
  DIR="$(curl -sS "${MASTER}/api/cluster/directory?q=edge")"
  VIA="$(printf '%s' "${DIR}" | python3 -c '
import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    raise SystemExit(0)
for it in d.get("items", []):
    if it.get("via", {}).get("role") == "worker":
        print(it["via"]["nodeName"])
        break
')"
  [[ -n "${VIA}" ]] && break
  sleep 0.5
done
check "master 聚合目录看到 worker 的制品（via=worker）" "smoke-worker" "${VIA}"

ROUTE_NODE="$(curl -sS "${MASTER}/api/nodes/route?ref=@${NS_W}/edge-skill" | jval candidates.0.nodeName)"
check "能力路由指向持有者" "smoke-worker" "${ROUTE_NODE}"

# master 代理 worker 的字节：客户端只认识 master 一个地址
DL_URL="$(curl -sS "${MASTER}/api/registry/@${NS_W}/edge-skill/download" | jval url)"
if [[ "${DL_URL}" == *":${PORT_M}/api/registry/@${NS_W}/edge-skill"*"/bytes" ]]; then
  good "下载地址指向 master 自己的代理端点（${DL_URL}）"
else
  bad "下载地址没有落在 master 上：${DL_URL}"
fi
PROXIED_SHA="$(curl -sS "${DL_URL}" | shasum -a 256 | awk '{print $1}')"
check "master 代理回来的字节与原文件 sha256 一致" "${SHA_W}" "${PROXIED_SHA}"

say "5. 集群总览"
TOT="$(curl -sS "${MASTER}/api/cluster")"
check "集群节点数（master + 1 worker）" "2" "$(printf '%s' "${TOT}" | jval totals.nodes)"
check "集群在线 worker 数" "1" "$(printf '%s' "${TOT}" | jval totals.workersOnline)"

say "6. 接入票据：key/secret 与内网短链"
TK="$(curl -sS -X POST "${MASTER}/api/access/tickets" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"label":"冒烟-给 carol 的 Agent","uses":1,"expiresInDays":1}')"
TKEY="$(printf '%s' "${TK}" | jval ticket.key)"
TSECRET="$(printf '%s' "${TK}" | jval secret)"
TLINK="$(printf '%s' "${TK}" | jval link)"
[[ -n "${TKEY}" && -n "${TSECRET}" ]] && good "签发票据成功：${TKEY}" || { bad "签发票据失败"; printf '%s\n' "${TK}"; }
case "${TLINK}" in
  *"/j/${TKEY}#${TSECRET}") good "接入短链形态正确（secret 在 fragment）" ;;
  *) bad "接入短链形态不对：${TLINK}" ;;
esac
check_code "短链落地页可访问" 200 "${MASTER}/j/${TKEY}"
check "票据概要（公开，不含 secret）" "${TKEY}" "$(curl -sS "${MASTER}/api/access/tickets/${TKEY}" | jval ticket.key)"

REDEEM="$(curl -sS -X POST "${MASTER}/api/access/redeem" -H 'Content-Type: application/json' \
  -d "{\"key\":\"${TKEY}\",\"secret\":\"${TSECRET}\",\"node\":{\"name\":\"carol-agent\",\"kind\":\"agent\",\"region\":\"测试-内网\",\"capabilities\":[\"mcp\"]}}")"
NTOK="$(printf '%s' "${REDEEM}" | jval token)"
NODE_ID_T="$(printf '%s' "${REDEEM}" | jval node.id)"
NODE_NS_T="$(printf '%s' "${REDEEM}" | jval node.namespace.slug)"
check "节点令牌已签发" "True" "$(printf '%s' "${REDEEM}" | python3 -c 'import json,sys;print(bool(json.load(sys.stdin).get("token")))')"
check "票据归属命名空间（= 签发者）" "${NS_M}" "${NODE_NS_T}"
[[ -n "${NODE_ID_T}" ]] && good "兑换即入网：节点 ${NODE_ID_T}" || bad "兑换未返回节点"

HEART="$(curl -sS -X POST "${MASTER}/api/nodes/heartbeat" -H "Authorization: Bearer ${NTOK}" \
  -H 'Content-Type: application/json' -d '{"name":"carol-agent","slug":"carol-agent"}')"
check "节点令牌可用于心跳续租" "carol-agent" "$(printf '%s' "${HEART}" | jval node.slug)"
PUB_CODE="$(curl -sS -o /dev/null -w '%{http_code}' -X POST "${MASTER}/api/registry" -H "Authorization: Bearer ${NTOK}" \
  -H 'Content-Type: application/json' -d '{"kind":"skill","name":"Nope","slug":"nope","status":"published","storage":{"url":"http://x/y.md"}}')"
check "节点令牌不能发布（最小权限）" "403" "${PUB_CODE}"
check_code "secret 错误时拒绝兑换" 401 -X POST "${MASTER}/api/access/redeem" \
  -H 'Content-Type: application/json' -d "{\"key\":\"${TKEY}\",\"secret\":\"deadbeef\"}"
check_code "票据次数用尽后失效（uses=1）" 403 -X POST "${MASTER}/api/access/redeem" \
  -H 'Content-Type: application/json' -d "{\"key\":\"${TKEY}\",\"secret\":\"${TSECRET}\"}"

say "7. 授权：连接 ≠ 授权（私有制品 / 私有节点）"
curlv() { curl -sS "$@"; }
curl -sS -X POST "${MASTER}/api/auth/register" -H 'Content-Type: application/json' \
  -d '{"email":"carol@corp.com","password":"smoke1234","name":"Carol"}' >"${TMP}/reg-c.json"
TOK_C="$(jval token <"${TMP}/reg-c.json")"
printf '# Private Skill\n\n只给被授权的人。\n' >"${WORK}/priv.SKILL.md"
UP_P="$(curlv -X POST "${MASTER}/api/registry/uploads" -H "Authorization: Bearer ${TOK_M}" \
  -H 'X-Filename: priv.SKILL.md' --data-binary @"${WORK}/priv.SKILL.md")"
CREATE_P="$(curlv -X POST "${MASTER}/api/registry" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d "{\"kind\":\"skill\",\"name\":\"Private Skill\",\"slug\":\"priv-skill\",
  \"status\":\"published\",\"visibility\":\"private\",\"tags\":[\"priv\"],
  \"storage\":{\"url\":\"$(printf '%s' "${UP_P}" | jval storageUrl)\",\"sha256\":\"$(printf '%s' "${UP_P}" | jval sha256)\"}}")"
PREF="$(printf '%s' "${CREATE_P}" | jval item.ref)"
[[ -n "${PREF}" ]] && good "alice 发布私有制品：${PREF}" || bad "私有发布失败"

check_code "未授权时看不到私有制品" 404 "${MASTER}/api/registry/@${NS_M}/priv-skill" -H "Authorization: Bearer ${TOK_C}"
GRANT="$(curlv -X POST "${MASTER}/api/grants" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"ref":"@carol","kind":"artifact","note":"冒烟测试"}')"
GID="$(printf '%s' "${GRANT}" | jval grant.id)"
check_code "授权后可见" 200 "${MASTER}/api/registry/@${NS_M}/priv-skill" -H "Authorization: Bearer ${TOK_C}"
# 私有条目的字节地址是短时签名地址：客户端拿到它就能取，不再需要带凭据。
SIGNED_URL="$(curl -sS "${MASTER}/api/registry/@${NS_M}/priv-skill/download" -H "Authorization: Bearer ${TOK_C}" | jval url)"
case "${SIGNED_URL}" in
  *"exp="*"sig="*) good "私有条目给的是签名地址" ;;
  *) bad "私有条目没有给签名地址：${SIGNED_URL}" ;;
esac
check_code "凭签名地址（无凭据）可取字节" 200 "${SIGNED_URL}"
check_code "伪造签名的地址被拒" 404 "${MASTER}/api/registry/@${NS_M}/priv-skill/bytes?exp=4102444800&sig=deadbeef"
check_code "公开条目仍是稳定地址（无签名）" 200 "${MASTER}/api/registry/@${NS_M}/hotel-skill/bytes"
check "授权列表（给出的）含该条" "${GID}" "$(curl -sS "${MASTER}/api/grants?direction=outgoing" -H "Authorization: Bearer ${TOK_M}" | jval grants.0.id)"
check "授权列表（收到的）含该条" "${GID}" "$(curl -sS "${MASTER}/api/grants?direction=incoming" -H "Authorization: Bearer ${TOK_C}" | jval grants.0.id)"

# 私有节点：不给 node 授权时不可见、不可连接
curl -sS -X POST "${MASTER}/api/nodes/heartbeat" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"name":"alice-private-svc","kind":"service","region":"测试-内网","visibility":"private"}' >/dev/null
PRIV_FOUND="$(curl -sS "${MASTER}/api/nodes/discover?q=alice-private" -H "Authorization: Bearer ${TOK_C}" | jval total)"
check "未授权看不到私有节点" "0" "${PRIV_FOUND}"
curl -sS -X POST "${MASTER}/api/grants" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"ref":"@carol","kind":"node"}' >/dev/null
PRIV_FOUND2="$(curl -sS "${MASTER}/api/nodes/discover?q=alice-private" -H "Authorization: Bearer ${TOK_C}" | jval total)"
check "node 授权后私有节点可见" "1" "${PRIV_FOUND2}"
curl -sS -X DELETE "${MASTER}/api/grants/${GID}" -H "Authorization: Bearer ${TOK_M}" >/dev/null
check_code "撤销授权立即生效（制品）" 404 "${MASTER}/api/registry/@${NS_M}/priv-skill" -H "Authorization: Bearer ${TOK_C}"

say "8. 集群写：发布分发（replicate）与下架回收（revoke）"
printf '# Fanout Skill\n\n分发到 worker 的副本。\n' >"${WORK}/fanout.SKILL.md"
UP_F="$(curlv -X POST "${MASTER}/api/registry/uploads" -H "Authorization: Bearer ${TOK_M}" \
  -H 'X-Filename: fanout.SKILL.md' --data-binary @"${WORK}/fanout.SKILL.md")"
CREATE_F="$(curlv -X POST "${MASTER}/api/registry" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d "{\"kind\":\"skill\",\"name\":\"Fanout Skill\",\"slug\":\"fanout-skill\",
  \"status\":\"published\",\"tags\":[\"fanout\"],\"replicate\":\"all\",
  \"storage\":{\"url\":\"$(printf '%s' "${UP_F}" | jval storageUrl)\",\"sha256\":\"$(printf '%s' "${UP_F}" | jval sha256)\"}}")"
FREF="$(printf '%s' "${CREATE_F}" | jval item.ref)"
check "发布即分发：worker 收到副本" "True" "$(printf '%s' "${CREATE_F}" | jval replicated.0.ok)"
check "副本字节大小一致" "$(wc -c <"${WORK}/fanout.SKILL.md" | tr -d ' ')" "$(printf '%s' "${CREATE_F}" | jval replicated.0.size)"

W_ITEM="$(curl -sS "${WORKER}/api/registry/@${NS_M}/fanout-skill")"
check "worker 本地可见副本" "replica" "$(printf '%s' "${W_ITEM}" | jval item.origin)"
check "worker 副本标注来源引用" "${FREF}" "$(printf '%s' "${W_ITEM}" | jval item.replicaOf)"
W_SHA="$(curl -sS "${WORKER}/api/registry/@${NS_M}/fanout-skill/bytes" | shasum -a 256 | awk '{print $1}')"
check "worker 副本字节 sha256 与本地一致" "$(printf '%s' "${UP_F}" | jval sha256)" "${W_SHA}"
check_code "副本不可在 worker 上改" 403 -X PATCH \
  "${WORKER}/api/registry/@${NS_M}/fanout-skill" -H "Authorization: Bearer ${TOK_W}" \
  -H 'Content-Type: application/json' -d '{"summary":"hack"}'

DEL="$(curlv -X DELETE "${MASTER}/api/registry/@${NS_M}/fanout-skill" -H "Authorization: Bearer ${TOK_M}")"
check "下架已广播到 worker" "True" "$(printf '%s' "${DEL}" | jval revoked.0.ok)"
check "worker 上的副本已回收" "True" "$(printf '%s' "${DEL}" | jval revoked.0.removed)"
check_code "回收后 worker 查不到该制品" 404 "${WORKER}/api/registry/@${NS_M}/fanout-skill"

say "9. 存储目录可配置（字节 / 库 / 数据根各指一处）"
META_DIRS="$(curl -sS "${MASTER}/api/meta")"
check "meta 报出的字节目录 = 配置值" "${TMP}/blobs-master" "$(printf '%s' "${META_DIRS}" | jval storage.blobDir)"
check "meta 报出的库文件 = 配置值" "${TMP}/db/master.sqlite" "$(printf '%s' "${META_DIRS}" | jval storage.dbPath)"
check "meta 报出的数据根 = 配置值" "${TMP}/m" "$(printf '%s' "${META_DIRS}" | jval storage.dataDir)"
BLOB_M="$(find "${TMP}/blobs-master" -type f | head -1)"
if [[ -n "${BLOB_M}" ]]; then good "上传的字节确实落在 NCCR_BLOB_DIR（$(basename "${BLOB_M}")）"; else bad "NCCR_BLOB_DIR 里没有字节"; fi
if [[ -f "${TMP}/db/master.sqlite" ]]; then good "库文件确实落在 NCCR_DB_PATH"; else bad "NCCR_DB_PATH 没有库文件"; fi
if [[ -f "${TMP}/m/node-id" ]]; then good "身份/密钥仍在数据根下（node-id / jwt-secret）"; else bad "数据根下没有 node-id"; fi
# 默认布局：worker 没配目录，副本字节应落在 <data>/blobs
W_BLOB="$(find "${TMP}/w/blobs" -type f 2>/dev/null | head -1)"
if [[ -n "${W_BLOB}" ]]; then good "未配置时默认布局不变（<data>/blobs 收到副本字节）"; else bad "worker 的默认字节目录里没有字节"; fi

say "10. 配置托管（团队网络 / 基础设施配置 + 版本 + 加密 + 授权 + 票据）"
# 10.1 类型目录
CFG_KINDS="$(curl -sS "${MASTER}/api/configs/kinds")"
check "配置类型目录含 network" "network" "$(printf '%s' "${CFG_KINDS}" | jval kinds.0.kind)"
check "配置类型目录含 secret 上限信息" "128" "$(printf '%s' "${CFG_KINDS}" | jval limits.bytes | awk '{print int($1/1024)}')"

# 10.2 创建（默认私有）+ 打码读取
cat >"${WORK}/network.yaml" <<'YAML'
subnet: 10.20.0.0/16
gateway: 10.20.0.1
dns: [10.20.0.53, 10.20.0.54]
YAML
cjson() { python3 -c 'import json,sys; print(json.dumps({"namespace":sys.argv[1],"slug":sys.argv[2],"name":sys.argv[3],"kind":sys.argv[4],"environment":sys.argv[5],"format":sys.argv[6],"content":open(sys.argv[7]).read(),"note":"冒烟"}))' "$@"; }
NEW_CFG="$(curlv -X POST "${MASTER}/api/configs" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' \
  -d "$(cjson "${NS_M}" network 网络配置 network prod yaml "${WORK}/network.yaml")")"
check "创建配置" "@${NS_M}/network" "$(printf '%s' "${NEW_CFG}" | jval config.ref)"
check "默认私有" "private" "$(printf '%s' "${NEW_CFG}" | jval config.visibility)"
check "写回执直接带内容（写的人刚给的）" "subnet: 10.20.0.0/16" "$(printf '%s' "${NEW_CFG}" | jval config.content | head -1)"
MASKED="$(curl -sS "${MASTER}/api/configs/@${NS_M}/network" -H "Authorization: Bearer ${TOK_M}")"
check "读默认打码" "True" "$(printf '%s' "${MASKED}" | jval config.masked)"
check "打码时仍给 64 位 checksum" "64" "$(printf '%s' "${MASKED}" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["config"]["checksum"]))')"
check "匿名读私有配置 404" "404" "$(curl -sS -o /dev/null -w '%{http_code}' "${MASTER}/api/configs/@${NS_M}/network")"
REVEALED="$(curl -sS "${MASTER}/api/configs/@${NS_M}/network?reveal=1" -H "Authorization: Bearer ${TOK_M}")"
check "reveal 拿到明文" "gateway: 10.20.0.1" "$(printf '%s' "${REVEALED}" | jval config.content | sed -n 2p)"

# 10.3 版本与回滚
printf 'subnet: 10.20.0.0/16\nmtu: 9000\n' >"${WORK}/network.v2.yaml"
V2="$(curlv -X PATCH "${MASTER}/api/configs/@${NS_M}/network" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' \
  -d "$(python3 -c 'import json,sys;print(json.dumps({"content":open(sys.argv[1]).read(),"note":"加 MTU"}))' "${WORK}/network.v2.yaml")")"
check "改内容 → v2" "2" "$(printf '%s' "${V2}" | jval config.revision)"
check "改内容标出新版本" "True" "$(printf '%s' "${V2}" | jval revisionAdded)"
META_ONLY="$(curlv -X PATCH "${MASTER}/api/configs/@${NS_M}/network" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"summary":"只改说明"}')"
check "只改元数据不加版本" "False" "$(printf '%s' "${META_ONLY}" | jval revisionAdded)"
REVS="$(curl -sS "${MASTER}/api/configs/@${NS_M}/network/revisions" -H "Authorization: Bearer ${TOK_M}")"
check "历史有 2 版且当前是 v2" "2" "$(printf '%s' "${REVS}" | jval revisions.0.revision)"
ROLL="$(curlv -X POST "${MASTER}/api/configs/@${NS_M}/network/rollback" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"revision":1}')"
check "回滚 → v3" "3" "$(printf '%s' "${ROLL}" | jval config.revision)"
check "回滚后内容 = v1" "gateway: 10.20.0.1" "$(printf '%s' "${ROLL}" | jval config.content | sed -n 2p)"

# 10.4 敏感配置：静态加密
printf 'wifi_psk=SuperSecret-12345\n' >"${WORK}/psk.env"
SEC="$(curlv -X POST "${MASTER}/api/configs" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' \
  -d "$(python3 -c 'import json,sys;print(json.dumps({"namespace":sys.argv[1],"slug":"wifi-psk","name":"WiFi","kind":"security","format":"env","secret":True,"content":"wifi_psk=SuperSecret-12345\n"}))' "${NS_M}")")"
check "创建敏感配置" "True" "$(printf '%s' "${SEC}" | jval config.secret)"
STORED="$(python3 - "${TMP}/db/master.sqlite" <<'PY'
import sqlite3, sys
db = sqlite3.connect(sys.argv[1])
row = db.execute("select content from config_entries where slug='wifi-psk'").fetchone()
print("sealed" if row and row[0].startswith("enc:v1:") and "SuperSecret" not in row[0] else "plain")
PY
)"
check "库里存的是密文（拿到库也读不出明文）" "sealed" "${STORED}"
check_code "敏感配置不允许公开" 400 -X POST "${MASTER}/api/configs" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' \
  -d "{\"namespace\":\"${NS_M}\",\"slug\":\"psk-pub\",\"name\":\"x\",\"kind\":\"security\",\"secret\":true,\"visibility\":\"public\",\"content\":\"x\"}"

# 10.5 bundle：按环境成组拉取（含 any，默认跳过 secret）
curlv -X POST "${MASTER}/api/configs" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' \
  -d "{\"namespace\":\"${NS_M}\",\"slug\":\"dns-any\",\"name\":\"通用 DNS\",\"kind\":\"network\",\"environment\":\"any\",\"format\":\"text\",\"content\":\"nameserver 10.20.0.53\n\",\"visibility\":\"public\"}" >/dev/null
BUNDLE="$(curl -sS "${MASTER}/api/configs/bundle?namespace=${NS_M}&env=prod" -H "Authorization: Bearer ${TOK_M}")"
check "bundle 含 prod 与 any" "2" "$(printf '%s' "${BUNDLE}" | jval count)"
check "bundle 默认跳过敏感配置" "0" "$(printf '%s' "${BUNDLE}" | python3 -c 'import json,sys; print(sum(1 for c in json.load(sys.stdin)["configs"] if c["secret"]))')"
check "bundle 带建议文件名（含环境后缀）" "1" "$(printf '%s' "${BUNDLE}" | python3 -c 'import json,sys; print(sum(1 for c in json.load(sys.stdin)["configs"] if c["filename"]=="alice-network.prod.yaml"))')"
check_code "匿名 bundle 被拒" 403 "${MASTER}/api/configs/bundle?namespace=${NS_M}&env=prod"

# 10.6 授权：carol 拿到 config 授权后可读、仍不可写
check_code "未授权读私有配置 404" 404 "${MASTER}/api/configs/@${NS_M}/network?reveal=1" -H "Authorization: Bearer ${TOK_C}"
G_CFG="$(curlv -X POST "${MASTER}/api/grants" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"ref":"@carol","kind":"config","note":"看网络配置"}')"
check "授予 config 授权" "config" "$(printf '%s' "${G_CFG}" | jval grant.kind)"
CFG_C="$(curl -sS "${MASTER}/api/configs/@${NS_M}/network?reveal=1" -H "Authorization: Bearer ${TOK_C}")"
check "被授权者可读非公开配置" "subnet: 10.20.0.0/16" "$(printf '%s' "${CFG_C}" | jval config.content | head -1)"
check_code "被授权者不可写配置" 403 -X PATCH "${MASTER}/api/configs/@${NS_M}/network" \
  -H "Authorization: Bearer ${TOK_C}" -H 'Content-Type: application/json' -d '{"content":"hack"}'

# 10.7 凭证作用域：只读 key 不能写；票据可以按需给 Agent 配置权
KEY_RO="$(curlv -X POST "${MASTER}/api/auth/keys" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"label":"cfg-ro","scopes":["config:read"]}')"
SECRET_RO="$(printf '%s' "${KEY_RO}" | jval secret)"
check "config:read key 能读私有配置" "True" \
  "$(curl -sS "${MASTER}/api/configs/@${NS_M}/network" -H "Authorization: Bearer ${SECRET_RO}" | jval config.canRead)"
check_code "config:read key 不能写" 403 -X POST "${MASTER}/api/configs" -H "Authorization: Bearer ${SECRET_RO}" \
  -H 'Content-Type: application/json' -d "{\"namespace\":\"${NS_M}\",\"slug\":\"via-key\",\"name\":\"x\",\"kind\":\"other\",\"content\":\"x\"}"

TICKET_CFG="$(curlv -X POST "${MASTER}/api/access/tickets" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"label":"agent-conf","scopes":["config:read","config:write"]}')"
TKEY="$(printf '%s' "${TICKET_CFG}" | jval ticket.key)"
TSEC="$(printf '%s' "${TICKET_CFG}" | jval secret)"
REDEEM="$(curlv -X POST "${MASTER}/api/access/redeem" -H 'Content-Type: application/json' \
  -d "{\"key\":\"${TKEY}\",\"secret\":\"${TSEC}\"}")"
NTOK="$(printf '%s' "${REDEEM}" | jval token)"
AGENT_CFG="$(curlv -X POST "${MASTER}/api/configs" -H "Authorization: Bearer ${NTOK}" \
  -H 'Content-Type: application/json' \
  -d "{\"namespace\":\"${NS_M}\",\"slug\":\"agent-managed\",\"name\":\"Agent 管的配置\",\"kind\":\"agent\",\"format\":\"json\",\"content\":\"{\\\"model\\\":\\\"qwen3\\\"}\"}")"
check "票据兑换的节点令牌可创建配置（代表签发者）" "@${NS_M}/agent-managed" "$(printf '%s' "${AGENT_CFG}" | jval config.ref)"
PLAIN_TICKET="$(curlv -X POST "${MASTER}/api/access/tickets" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"label":"plain","scopes":["nodes:write"]}')"
PLAIN_REDEEM="$(curlv -X POST "${MASTER}/api/access/redeem" -H 'Content-Type: application/json' \
  -d "{\"key\":\"$(printf '%s' "${PLAIN_TICKET}" | jval ticket.key)\",\"secret\":\"$(printf '%s' "${PLAIN_TICKET}" | jval secret)\"}")"
check_code "无 config:write 的令牌建配置被拒" 403 -X POST "${MASTER}/api/configs" \
  -H "Authorization: Bearer $(printf '%s' "${PLAIN_REDEEM}" | jval token)" \
  -H 'Content-Type: application/json' -d "{\"namespace\":\"${NS_M}\",\"slug\":\"nope\",\"name\":\"x\",\"kind\":\"other\",\"content\":\"x\"}"

# 10.8 归档与删除
curlv -X PATCH "${MASTER}/api/configs/@${NS_M}/dns-any" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"status":"archived"}' >/dev/null
check "归档后从公开目录消失" "0" "$(curl -sS "${MASTER}/api/configs" | jval total)"
check "归档后不再进 bundle" "2" "$(curl -sS "${MASTER}/api/configs/bundle?namespace=${NS_M}&env=prod" -H "Authorization: Bearer ${TOK_M}" | jval count)"
check_code "删除配置（含历史）" 200 -X DELETE "${MASTER}/api/configs/@${NS_M}/agent-managed" -H "Authorization: Bearer ${TOK_M}"
check_code "删除后取不到" 404 "${MASTER}/api/configs/@${NS_M}/agent-managed" -H "Authorization: Bearer ${TOK_M}"
ORPHAN="$(python3 - "${TMP}/db/master.sqlite" <<'PY'
import sqlite3, sys
db = sqlite3.connect(sys.argv[1])
print(db.execute("select count(*) from config_revisions where config_id not in (select id from config_entries)").fetchone()[0])
PY
)"
check "版本历史随条目一起清理" "0" "${ORPHAN}"

say "11. 节点治理：用户 / 节点 / 服务（管理员 + admin key/secret + 审计）"

# 11.1 引导：master 的第一个注册用户自动成为管理员，并自动签发一份机器凭据
AK="$(jval admin.key <"${TMP}/reg-m.json")"
ASEC="$(jval admin.secret <"${TMP}/reg-m.json")"
check "首个注册用户自动获得 admin key（AK-…）" "AK-" "${AK:0:3}"
check_code "管理员会话可进管理面" 200 "${MASTER}/api/admin/overview" -H "Authorization: Bearer ${TOK_M}"
check_code "普通用户会话进管理面被拒" 403 "${MASTER}/api/admin/overview" -H "Authorization: Bearer ${TOK_C}"
check_code "匿名进管理面被拒" 403 "${MASTER}/api/admin/overview"
check_code "admin key/secret 可进管理面" 200 "${MASTER}/api/admin/overview" -H "X-NCC-Admin-Key: ${AK}" -H "X-NCC-Admin-Secret: ${ASEC}"
check_code "admin secret 错误被拒" 401 "${MASTER}/api/admin/overview" -H "X-NCC-Admin-Key: ${AK}" -H "X-NCC-Admin-Secret: deadbeef"
check_code "只给 key 不给 secret 被拒" 400 "${MASTER}/api/admin/overview" -H "X-NCC-Admin-Key: ${AK}"
check "凭据列表里有 bootstrap" "1" "$(curl -sS "${MASTER}/api/admin/keys" -H "X-NCC-Admin-Key: ${AK}" -H "X-NCC-Admin-Secret: ${ASEC}" | jval total)"

# 11.2 造一条「服务」数据：制品侧 kind=api + 节点侧 kind=service
printf '# Hotel API\n\n服务接口（冒烟测试）。\n' >"${WORK}/hotel.api.md"
UP_S="$(curl -sS -X POST "${MASTER}/api/registry/uploads" -H "Authorization: Bearer ${TOK_M}" \
  -H 'X-Filename: hotel.api.md' --data-binary @"${WORK}/hotel.api.md")"
SZ_S="$(wc -c <"${WORK}/hotel.api.md" | tr -d ' ')"
curl -sS -X POST "${MASTER}/api/registry" -H "Authorization: Bearer ${TOK_M}" -H 'Content-Type: application/json' \
  -d "{\"kind\":\"api\",\"name\":\"Hotel API\",\"slug\":\"hotel-api\",\"summary\":\"服务接口\",
  \"status\":\"published\",\"visibility\":\"private\",
  \"storage\":{\"url\":\"$(printf '%s' "${UP_S}" | jval storageUrl)\",\"sha256\":\"$(printf '%s' "${UP_S}" | jval sha256)\",\"size\":${SZ_S}}}" >/dev/null
curl -sS -X POST "${MASTER}/api/nodes/heartbeat" -H "Authorization: Bearer ${TOK_M}" -H 'Content-Type: application/json' \
  -d '{"name":"booking-svc","slug":"booking-svc","kind":"service","region":"上海-内网"}' >"${TMP}/svc-node.json"
SVC_NODE="$(jval node.id <"${TMP}/svc-node.json")"
check "托管了一个 kind=service 节点" "service" "$(jval node.kind <"${TMP}/svc-node.json")"

OV="$(curl -sS "${MASTER}/api/admin/overview" -H "X-NCC-Admin-Key: ${AK}" -H "X-NCC-Admin-Secret: ${ASEC}")"
check "概览：管理员至少 1" "1" "$(printf '%s' "${OV}" | jval counts.admins)"
# 前面几节已经建过节点（默认 kind=service），所以这里比「至少 1」而不是相等。
SVC_TOTAL="$(printf '%s' "${OV}" | jval counts.services.hostedNodes)"
[[ "${SVC_TOTAL}" -ge 1 ]] && good "概览：服务节点至少 1（${SVC_TOTAL}）" || bad "概览：服务节点应 ≥1，实际 ${SVC_TOTAL}"
check "概览：服务条目 1" "1" "$(printf '%s' "${OV}" | jval counts.services.artifacts)"
check "凭据类型是 admin_key" "admin_key" "$(printf '%s' "${OV}" | jval credential.kind)"

check "管理面节点列表看得到私有/离线节点" "1" \
  "$(curl -sS "${MASTER}/api/admin/nodes?q=booking-svc" -H "Authorization: Bearer ${TOK_M}" | jval total)"
SVCS="$(curl -sS "${MASTER}/api/admin/services?q=booking-svc" -H "Authorization: Bearer ${TOK_M}")"
check "服务列表：节点侧命中 1" "1" "$(printf '%s' "${SVCS}" | jval total.nodeServices)"

# 11.3 用户治理：禁用 → 旧令牌立即失效、登录被拒 → 重置密码 → 启用
CAROL_ID="$(curl -sS "${MASTER}/api/admin/users?q=carol" -H "Authorization: Bearer ${TOK_M}" | jval users.0.id)"
ALICE_ID="$(curl -sS "${MASTER}/api/auth/me" -H "Authorization: Bearer ${TOK_M}" | jval user.id)"
check_code "禁用 carol（带原因）" 200 -X PATCH "${MASTER}/api/admin/users/${CAROL_ID}" \
  -H "Authorization: Bearer ${TOK_M}" -H 'Content-Type: application/json' \
  -d '{"disabled":true,"adminNote":"冒烟测试"}'
check_code "被禁用后旧令牌立即失效" 401 "${MASTER}/api/auth/me" -H "Authorization: Bearer ${TOK_C}"
check_code "被禁用后登录被拒（带原因）" 403 -X POST "${MASTER}/api/auth/login" \
  -H 'Content-Type: application/json' -d '{"email":"carol@corp.com","password":"smoke1234"}'
check "禁用原因会带回给本人" "1" \
  "$(curl -sS -X POST "${MASTER}/api/auth/login" -H 'Content-Type: application/json' \
     -d '{"email":"carol@corp.com","password":"smoke1234"}' | grep -c '冒烟测试' || true)"
NEWPW="$(curl -sS -X POST "${MASTER}/api/admin/users/${CAROL_ID}/password" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{}' | jval password)"
check "重置密码返回新密码（服务端生成 12 位）" "12" "${#NEWPW}"
check_code "启用 carol" 200 -X PATCH "${MASTER}/api/admin/users/${CAROL_ID}" \
  -H "Authorization: Bearer ${TOK_M}" -H 'Content-Type: application/json' -d '{"disabled":false}'
check_code "新密码可登录" 200 -X POST "${MASTER}/api/auth/login" \
  -H 'Content-Type: application/json' -d "{\"email\":\"carol@corp.com\",\"password\":\"${NEWPW}\"}"
check_code "不能用 admin 凭据禁用最后一个管理员" 400 -X PATCH "${MASTER}/api/admin/users/${ALICE_ID}" \
  -H "X-NCC-Admin-Key: ${AK}" -H "X-NCC-Admin-Secret: ${ASEC}" -H 'Content-Type: application/json' -d '{"disabled":true}'

# 11.4 服务处理：节点侧摘除 / 制品侧归档（归档不删字节）
check_code "摘除服务节点（ND-…）" 200 -X DELETE "${MASTER}/api/admin/services/${SVC_NODE}" -H "Authorization: Bearer ${TOK_M}"
check "节点已摘除（按名字查不到了）" "0" "$(curl -sS "${MASTER}/api/admin/nodes?q=booking-svc" -H "Authorization: Bearer ${TOK_M}" | jval total)"
check_code "归档服务条目（@ns/slug）" 200 -X DELETE "${MASTER}/api/admin/services/@${NS_M}/hotel-api" -H "Authorization: Bearer ${TOK_M}"
check "条目状态变为 archived" "archived" "$(curl -sS "${MASTER}/api/registry/@${NS_M}/hotel-api" -H "Authorization: Bearer ${TOK_M}" | jval item.status)"
check "归档只改状态，字节仍在" "${SZ_S}" "$(curl -sS "${MASTER}/api/registry/@${NS_M}/hotel-api" -H "Authorization: Bearer ${TOK_M}" | jval item.storage.size)"

# 11.5 审计：每个治理动作都留痕
AUD="$(curl -sS "${MASTER}/api/admin/audit?limit=50" -H "Authorization: Bearer ${TOK_M}")"
for want in user.disable user.password.reset user.enable service.delete service.archive; do
  check "审计含 ${want}" "1" "$(printf '%s' "${AUD}" | grep -c "\"${want}\"" || true)"
done
check "审计记录操作者类型" "1" "$(printf '%s' "${AUD}" | grep -c '"actor"' || true)"

# 11.6 凭据轮换：新 secret 生效、旧 secret 立即失效
ROT="$(curl -sS -X POST "${MASTER}/api/admin/keys/rotate?label=ops" -H "Authorization: Bearer ${TOK_M}")"
AK2="$(printf '%s' "${ROT}" | jval key.key)"
ASEC2="$(printf '%s' "${ROT}" | jval secret)"
check "轮换撤销旧凭据 1 份" "1" "$(printf '%s' "${ROT}" | jval revoked)"
check_code "旧 admin 凭据立即失效" 401 "${MASTER}/api/admin/overview" -H "X-NCC-Admin-Key: ${AK}" -H "X-NCC-Admin-Secret: ${ASEC}"
check_code "新 admin 凭据可用" 200 "${MASTER}/api/admin/overview" -H "X-NCC-Admin-Key: ${AK2}" -H "X-NCC-Admin-Secret: ${ASEC2}"

say "12. 分享链接：临时下载地址（对方不用登录）"

# 12.1 创建：只有「本来能读这条制品」的人才能分享
SHARE="$(curl -sS -X POST "${MASTER}/api/shares" -H "Authorization: Bearer ${TOK_M}" -H 'Content-Type: application/json' \
  -d "{\"ref\":\"@${NS_M}/hotel-api\",\"label\":\"冒烟\",\"uses\":1,\"expiresInDays\":7}")"
TOKEN="$(printf '%s' "${SHARE}" | jval token)"
SHID="$(printf '%s' "${SHARE}" | jval share.id)"
check "创建分享返回 32 位 token" "32" "${#TOKEN}"
check "链接指向 /s/<token>" "1" "$(printf '%s' "${SHARE}" | jval link | grep -c "/s/${TOKEN}$" || true)"
check_code "无读权限的人不能分享（不是提权通道）" 403 -X POST "${MASTER}/api/shares" \
  -H "Authorization: Bearer ${TOK_C}" -H 'Content-Type: application/json' -d '{"ref":"@'"${NS_M}"'/hotel-api"}'

# 12.2 匿名领取：说明页可打开、?meta=1 不计数、raw 计数
check_code "匿名打开说明页" 200 "${MASTER}/s/${TOKEN}"
check_code "匿名取元数据（?meta=1）" 200 "${MASTER}/s/${TOKEN}/raw?meta=1"
check "meta 不消耗次数" "0" "$(curl -sS "${MASTER}/s/${TOKEN}/raw?meta=1" | jval share.uses.used)"
curl -sS -o "${TMP}/shared.md" "${MASTER}/s/${TOKEN}/raw"
check "匿名取到字节且内容一致" "0" "$(cmp -s "${WORK}/hotel.api.md" "${TMP}/shared.md" && echo 0 || echo 1)"
check "已用次数记为 1" "1" "$(curl -sS "${MASTER}/api/shares/info/${TOKEN}" | jval share.uses.used)"
check "限次用尽即失效" "False" "$(curl -sS "${MASTER}/api/shares/info/${TOKEN}" | jval usable)"
check_code "次数用尽后再取（410）" 410 "${MASTER}/s/${TOKEN}/raw"

# 12.3 列表 / 撤销 / 管理员视角
check_code "我的分享列表" 200 "${MASTER}/api/shares?mine=1" -H "Authorization: Bearer ${TOK_M}"
check_code "非管理员看全部分享被拒" 403 "${MASTER}/api/shares?all=1" -H "Authorization: Bearer ${TOK_C}"
check "管理员带 admin 凭据可看全部" "1" \
  "$(curl -sS "${MASTER}/api/shares?all=1" -H "X-NCC-Admin-Key: ${AK2}" -H "X-NCC-Admin-Secret: ${ASEC2}" | jval total)"
check_code "撤销分享" 200 -X DELETE "${MASTER}/api/shares/${SHID}" -H "Authorization: Bearer ${TOK_M}"
check_code "撤销后立即失效（410）" 410 "${MASTER}/s/${TOKEN}/raw"
check_code "别人不能撤我的分享" 403 -X DELETE "${MASTER}/api/shares/${SHID}" -H "Authorization: Bearer ${TOK_C}"

# 12.4 /api/meta 暴露治理面计数与能力
META="$(curl -sS "${MASTER}/api/meta")"
check "meta：admins" "1" "$(printf '%s' "${META}" | jval counts.admins)"
check "meta：有可用 admin 凭据" "True" "$(printf '%s' "${META}" | jval auth.adminKey)"
check "meta：features 含 admin" "1" "$(printf '%s' "${META}" | jval features | grep -c 'admin:' || true)"
check "meta：features 含 share" "1" "$(printf '%s' "${META}" | jval features | grep -c 'share:' || true)"
check "meta：features 含 p2p" "1" "$(printf '%s' "${META}" | jval features | grep -c 'p2p:' || true)"

say "13. 节点侧 P2P：画像 / 真实对打 / 可被打洞入口"
# 13.1 画像：在哪台机器上跑就看哪台（含入口状态与 ICE 配置）。STUN 指向一个没人听的端口，
#     让它按超时快速收场，不依赖外网。
check_code "未登录看画像被拒" 401 "${MASTER}/api/p2p/self"
PSELF="$(curlv "${MASTER}/api/p2p/self" -H "Authorization: Bearer ${TOK_M}")"
check "画像带结论字段" "1" "$(printf '%s' "${PSELF}" | jval profile.verdict | grep -c . || true)"
check "入口默认关" "False" "$(printf '%s' "${PSELF}" | jval serve.on)"
check "画像带 STUN 列表" "1" "$(printf '%s' "${PSELF}" | jval ice.stun | grep -c . || true)"

# 13.2 入口开关（只应答 STUN，不接收业务字节）
SERVE_ON="$(curlv -X POST "${MASTER}/api/p2p/serve" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"on":true}')"
check "开入口" "True" "$(printf '%s' "${SERVE_ON}" | jval serve.on)"
check "入口报出监听地址" "1" "$(printf '%s' "${SERVE_ON}" | jval serve.listen | grep -c ':' || true)"
check "入口报出对端回包计数（反向打洞可见）" "0" "$(printf '%s' "${SERVE_ON}" | jval serve.responsesSeen)"
SERVE_PEER="$(curlv -X POST "${MASTER}/api/p2p/serve" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"on":true,"peer":"127.0.0.1:9"}')"
check "指定反向打洞对端" "127.0.0.1:9" "$(printf '%s' "${SERVE_PEER}" | jval serve.peers.0)"
check "纯 --on 不会抹掉已配对端" "127.0.0.1:9" \
  "$(curlv -X POST "${MASTER}/api/p2p/serve" -H "Authorization: Bearer ${TOK_M}" \
     -H 'Content-Type: application/json' -d '{"on":true}' | jval serve.peers.0)"
check_code "peer 格式不对（400）" 400 -X POST "${MASTER}/api/p2p/serve" -H "Authorization: Bearer ${TOK_M}" \
  -H 'Content-Type: application/json' -d '{"on":true,"peer":"不是地址"}'
check "关入口" "False" \
  "$(curlv -X POST "${MASTER}/api/p2p/serve" -H "Authorization: Bearer ${TOK_M}" \
     -H 'Content-Type: application/json' -d '{"on":false}' | jval serve.on)"

# 13.3 从节点侧真实对打：不传业务字节；拿不到自身映射时应报 503（p2p_probe_failed）而不是 500。
CHECK_CODE="$(curl -sS -o "${TMP}/p2p-check.json" -w '%{http_code}' -X POST "${MASTER}/api/p2p/check" \
  -H "Authorization: Bearer ${TOK_M}" -H 'Content-Type: application/json' \
  -d '{"peer":"127.0.0.1:9","waitSec":1}')"
if [[ "${CHECK_CODE}" == "200" || "${CHECK_CODE}" == "503" ]]; then
  good "真实对打返回 200（可用）/ 503（本地拿不到映射），不是 500"
else
  bad "真实对打返回 ${CHECK_CODE}（期望 200 或 503）"
fi
check "真实对打结果里有 ok 或明确失败码" "1" \
  "$(grep -c '"ok":\|p2p_probe_failed' "${TMP}/p2p-check.json" || true)"

printf '\n\033[1m结果：%d 项通过，%d 项失败\033[0m\n' "${PASS}" "${FAIL}"
[[ "${FAIL}" -eq 0 ]] || {
  echo "---- master.log ----"
  tail -20 "${TMP}/master.log"
  echo "---- worker.log ----"
  tail -20 "${TMP}/worker.log"
  exit 1
}
