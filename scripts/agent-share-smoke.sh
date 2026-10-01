#!/usr/bin/env bash
# NCC Agent Share（**内网节点侧**）端到端冒烟：`ncc agent share|add|ls|rm` 对着 ncc-registry 跑。
#
# 验的是「同一份客户端、两个产品」这件事，以及节点侧特有的几条边界：
#   · **同形**：`/api/agent-cards*` 的字段名与云端逐字对齐 → 客户端一行不改即可对接本节点
#   · **只存哈希**：token 只存 sha256（本仓规矩，比云端更保守）→ 库里查不到明文，
#     列表也回不出可点的链接（如实说明，而不是编一个打不开的地址）
#   · **一条链接两件事**：装包（~/.ncc/packages）+ 连接节点（连接表）；--no-install/--no-link 各做一半
#   · **字节即事实**：sha256 由服务端从收到的字节算，接受方核对通过才装
#   · **可撤销 / 可过期 / 可限次**：撤销 = 标记 revoked + 删字节（之后 410），三种原因分开说
#   · **名额用完只拦 accept**：读名片 / 取字节仍要能用（否则等于把已拿到名额的人关在门外）
#
# 全程隔离：数据目录 / 库 / blob / 两个用户的 NCC_HOME 都在临时目录，端口默认 18408。
# 用法：bash scripts/agent-share-smoke.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-18408}"
BASE="http://127.0.0.1:${PORT}"
TMP="$(mktemp -d)"
HOME_A="${TMP}/home-a"   # 作者
HOME_B="${TMP}/home-b"   # 接受方

find_cli() {
  if [[ -n "${CLI_BIN:-}" ]]; then printf '%s' "${CLI_BIN}"; return; fi
  for cand in "${ROOT}/../ncc-cli/cli/target/debug/ncc" "${ROOT}/../ncc-cli/cli/target/release/ncc"; do
    [[ -x "${cand}" ]] && { printf '%s' "${cand}"; return; }
  done
  printf ''
}
CLI="$(find_cli)"

PASS=0
FAIL=0
cleanup() {
  [[ -n "${PID:-}" ]] && kill "${PID}" 2>/dev/null || true
  wait 2>/dev/null || true
  if [[ -n "${KEEP:-}" ]]; then echo "（KEEP=1）保留 ${TMP}"; else rm -rf "${TMP}"; fi
}
trap cleanup EXIT

say()  { printf '\n\033[1m%s\033[0m\n' "$1"; }
good() { printf '  \033[32m✓\033[0m %s\n' "$1"; PASS=$((PASS + 1)); }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$1"; FAIL=$((FAIL + 1)); }
check() { if [[ "$2" == "$3" ]]; then good "$1（$3）"; else bad "$1：期望 $2，实际 $3"; fi; }
contains() { if [[ "$3" == *"$2"* ]]; then good "$1"; else bad "$1：输出里没有「$2」（实际：$(printf '%s' "$3" | head -c 240)）"; fi; }

BODY_FILE="$(mktemp)"
check_code() {
  local desc="$1" want="$2"; shift 2
  local got; got="$(curl -sS -o "${BODY_FILE}" -w '%{http_code}' "$@")"
  if [[ "${got}" == "${want}" ]]; then good "${desc}（${got}）"
  else bad "${desc}：期望 ${want}，实际 ${got}  body=$(head -c 200 "${BODY_FILE}")"; fi
}
jval() {
  python3 -c '
import json, sys
d = json.load(sys.stdin)
for p in sys.argv[1].split("."):
    if p == "": continue
    d = d[int(p)] if isinstance(d, list) else d[p]
print(d if not isinstance(d, (dict, list)) else json.dumps(d, ensure_ascii=False))
' "$1"
}
wait_http() {
  local url="$1" deadline=$((SECONDS + $2))
  while (( SECONDS < deadline )); do curl -fsS "$url" >/dev/null 2>&1 && return 0; sleep 0.3; done
  return 1
}
register() {
  curl -sS -X POST "${BASE}/api/auth/register" -H 'Content-Type: application/json' \
    -d "{\"email\":\"$1\",\"password\":\"smoke1234\",\"name\":\"$2\"}" | jval token
}
# 每个用户一个 NCC_HOME：登录态与包落点各自独立
run_a() { NCC_HOME="${HOME_A}" "${CLI}" --base "${BASE}" "$@"; }
run_b() { NCC_HOME="${HOME_B}" "${CLI}" --base "${BASE}" "$@"; }

if [[ -z "${CLI}" ]]; then echo "找不到 ncc：先在 ncc-cli/cli 里 cargo build，或用 CLI_BIN=/path/to/ncc"; exit 1; fi

say "0. 构建 + 启动本节点（:${PORT}）"
( cd "${ROOT}" && go build -o "${TMP}/ncc-registry" ./cmd/ncc-registry )
NCCR_PORT="${PORT}" NCCR_DATA_DIR="${TMP}/data" NCCR_BLOB_DIR="${TMP}/blobs" \
NCCR_DB_PATH="${TMP}/db/nccr.sqlite" NCCR_NODE_NAME="agent-smoke" NCCR_NODE_REGION="测试-内网" \
NCCR_PUBLIC_URL="${BASE}" NCCR_P2P_STUN="127.0.0.1:9" \
  "${TMP}/ncc-registry" >"${TMP}/server.log" 2>&1 &
PID=$!
wait_http "${BASE}/api/health" 30 || { bad "节点没起来"; tail -20 "${TMP}/server.log"; exit 1; }
good "节点已就绪"

say "1. 两个账号（作者 alice / 接受方 bob）"
TK_A="$(register alice@agent.dev Alice)"
TK_B="$(register bob@agent.dev Bob)"
[[ -n "${TK_A}" && -n "${TK_B}" ]] && good "两个账号注册成功" || { bad "注册失败"; exit 1; }
mkdir -p "${HOME_A}" "${HOME_B}"
run_a login --email alice@agent.dev --password smoke1234 >/dev/null
run_b login --email bob@agent.dev --password smoke1234 >/dev/null
contains "alice 的 CLI 登录态可用（同一份客户端对着内网节点）" "alice@agent.dev" "$(run_a me)"

say "2. 作者：托管一台 agent 节点 + 做一个 Agent 包"
# 节点上报走本节点自己的口（/api/nodes/heartbeat）。
# 注意：CLI 的 `ncc living …` 属于**平台侧**能力面（节点侧声明的是 `nodes` 而不是 `living`），
# 所以在内网节点上，节点由跑在这台机器上的 Agent/Gateway 上报 —— 冒烟里就直接 POST。
curl -sS -X POST "${BASE}/api/nodes/heartbeat" -H "Authorization: Bearer ${TK_A}" \
  -H 'Content-Type: application/json' >/dev/null \
  -d '{"name":"hotel-agent","kind":"agent","region":"测试-内网","capabilities":["serve:mcp"]}'
NODE_REF="$(curl -sS "${BASE}/api/nodes" -H "Authorization: Bearer ${TK_A}" | python3 -c '
import json, sys
d = json.load(sys.stdin)["nodes"]
if not d: print(""); raise SystemExit
n = d[0]
ns = n.get("namespace")
if isinstance(ns, dict): ns = ns.get("slug", "")
print("@" + str(ns).lstrip("@") + "/" + n.get("slug", ""))
')"
[[ -n "${NODE_REF}" ]] && good "节点已托管（${NODE_REF}）" || { bad "节点没托管上"; exit 1; }

PKG_DIR="${TMP}/pkgs/hotel-agent"
run_a hur init --kind agent --name "Hotel Agent" --role "帮人比价订酒店" --dir "${PKG_DIR}" >/dev/null
run_a hur build "${PKG_DIR}" >/dev/null
good "Agent 包已生成（hur init + hur build）"

say "3. 造名片：包 + 节点 + 访问 key + 限 2 人"
SHARE_OUT="$(run_a agent share "${PKG_DIR}" --node "${NODE_REF}" --name "酒店比价助手" \
  --note "比价 + 下单草稿" --expires 7d --uses 2 --key k7f2 --json)"
CARD_URL="$(printf '%s' "${SHARE_OUT}" | jval url)"
CARD_ID="$(printf '%s' "${SHARE_OUT}" | jval card.id)"
CARD_TOKEN="$(printf '%s' "${CARD_URL}" | sed 's#.*/a/##')"
CARD_SHA="$(printf '%s' "${SHARE_OUT}" | jval card.agent.sha256)"
AGENT_ID="$(printf '%s' "${SHARE_OUT}" | jval card.agent.id)"
[[ -n "${CARD_TOKEN}" && -n "${CARD_SHA}" ]] && good "名片已生成：${CARD_ID}" || { bad "名片生成失败：${SHARE_OUT}"; exit 1; }
check "包里读出来的是 agent" "agent" "$(printf '%s' "${SHARE_OUT}" | jval card.agent.profile)"
check "key 只回显这一次" "k7f2" "$(printf '%s' "${SHARE_OUT}" | jval key)"
check "节点写进了名片" "${NODE_REF}" "$(printf '%s' "${SHARE_OUT}" | jval card.node.ref)"

say "4. 只存哈希：库里查不到 token 明文"
HASH_CHECK="$(python3 - "${TMP}/db/nccr.sqlite" "${CARD_TOKEN}" <<'PY'
import hashlib, sqlite3, sys
db, token = sys.argv[1], sys.argv[2]
con = sqlite3.connect(db)
row = con.execute("select token_hash from agent_cards limit 1").fetchone()
want = hashlib.sha256(token.encode()).hexdigest()
print(f"{row[0]} {want}")
PY
)"
GOT_HASH="${HASH_CHECK%% *}"
WANT_HASH="${HASH_CHECK##* }"
check "token 存的是 sha256(token)" "${WANT_HASH}" "${GOT_HASH}"
check "库里没有 token 明文（token_hash 不等于明文）" "true" "$([[ "${GOT_HASH}" != "${CARD_TOKEN}" ]] && echo true || echo false)"

say "5. 点到点：匿名能看这一条，但看不到「有哪些」"
check_code "带 key 匿名读名片" 200 "${BASE}/api/agent-cards/${CARD_TOKEN}?key=k7f2"
check_code "不带 key → 403" 403 "${BASE}/api/agent-cards/${CARD_TOKEN}"
check_code "错 key → 403" 403 "${BASE}/api/agent-cards/${CARD_TOKEN}?key=wrong"
check_code "**没有全站列表**：匿名列名片 → 401" 401 "${BASE}/api/agent-cards"
check_code "作者本人不需要 key" 200 "${BASE}/api/agent-cards/${CARD_TOKEN}" -H "Authorization: Bearer ${TK_A}"
PAGE_HEADERS="$(curl -sS -D - -o /dev/null "${BASE}/a/${CARD_TOKEN}?key=k7f2")"
contains "落地页带 noindex" "noindex" "${PAGE_HEADERS}"
contains "落地页给出收下命令" "ncc agent add" "$(curl -sS "${BASE}/a/${CARD_TOKEN}?key=k7f2")"

curl -sS -D "${TMP}/blob.headers" -o "${TMP}/card.hur" "${BASE}/api/agent-cards/${CARD_TOKEN}/blob?key=k7f2"
check "下载字节的 sha256 == 名片指纹" "${CARD_SHA}" "$(shasum -a 256 "${TMP}/card.hur" | awk '{print $1}')"
contains "字节响应带 X-NCC-Sha256" "${CARD_SHA}" "$(grep -i '^x-ncc-sha256' "${TMP}/blob.headers" | tr -d '\r')"

say "6. bob 收下：装包 + 连接节点（一条命令，同一份客户端）"
if run_b agent add "${CARD_URL}#k7f2" --json >"${TMP}/add.json" 2>"${TMP}/add.err"; then
  good "ncc agent add 成功"
  ADD="$(cat "${TMP}/add.json")"
  check "包落盘（记录 id）" "${AGENT_ID}" "$(printf '%s' "${ADD}" | jval install.id)"
  check "安装记录里的 sha256 == 名片指纹" "${CARD_SHA}" "$(printf '%s' "${ADD}" | jval install.sha256)"
  PKG_PATH="$(printf '%s' "${ADD}" | jval install.path)"
  [[ -f "${PKG_PATH}/_install.json" ]] && good "~/.ncc/packages 下有登记文件" || bad "没找到安装登记：${PKG_PATH}"
  LINKED="$(printf '%s' "${ADD}" | python3 -c 'import json,sys; print("true" if json.load(sys.stdin).get("linkId") else "false")')"
  if [[ "${LINKED}" == "true" ]]; then
    good "节点已收进连接表（link.id 与云端同义）"
  else
    bad "节点没连上：$(printf '%s' "${ADD}" | jval linkError)"
  fi
  LINK_LABEL="$(curl -sS "${BASE}/api/nodes" -H "Authorization: Bearer ${TK_B}" | python3 -c '
import json, sys
d = json.load(sys.stdin).get("linked") or []
# 节点侧把 Name 标签放在 node.link.label 里（平台侧是顶层 label）—— 冒烟只认节点侧这份。
print((d[0].get("link") or {}).get("label", "") if d else "")
')"
  check "连接标签用的是名片上的名字" "酒店比价助手" "${LINK_LABEL}"
else
  bad "ncc agent add 失败：$(head -c 300 "${TMP}/add.err")"
fi

say "7. 只做一半 + 名额用完只拦 accept"
half="$(run_b agent add "${CARD_URL}#k7f2" --no-install --no-link 2>&1 || true)"
contains "--no-install --no-link 只验名片不做动作" "📇" "${half}"
contains "如实提醒收下之后仍需授权" "ncc grant" "${half}"
third="$(run_b agent add "${CARD_URL}#k7f2" --no-install --no-link 2>&1 || true)"
contains "额度用完后 add 被拒" "收下次数已用完" "${third}"
check_code "额度用完后仍能取字节（不把人关在门外）" 200 "${BASE}/api/agent-cards/${CARD_TOKEN}/blob?key=k7f2"

say "8. 撤销：标记 revoked + 删字节"
contains "作者侧 ls 看得到用量" "${CARD_ID}" "$(run_a agent ls)"
contains "ls 如实说明链接只存哈希" "只存 token 哈希" "$(run_a agent ls)"
run_a agent rm "${CARD_ID}" >/dev/null
check_code "撤销后读名片 → 410 revoked" 410 "${BASE}/api/agent-cards/${CARD_TOKEN}?key=k7f2"
contains "410 里说明了是撤销" "撤销" "$(cat "${BODY_FILE}")"
check_code "撤销后取字节 → 410" 410 "${BASE}/api/agent-cards/${CARD_TOKEN}/blob?key=k7f2"
check "撤销是**删掉字节**：blob 里已没有名片对象" "0" \
  "$(find "${TMP}/blobs" -path '*agent-cards*' -type f 2>/dev/null | wc -l | tr -d ' ')"
contains "作者侧 ls 里仍看得到它（已撤销）" "已撤销" "$(run_a agent ls)"

say "结果"
echo "  通过 ${PASS} · 失败 ${FAIL}"
[[ "${FAIL}" == "0" ]] || exit 1
