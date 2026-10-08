#!/usr/bin/env bash
# 通信基础设施（连接通道）端到端冒烟：`ncc conn open|exec|push|pull|run|status|close`。
#
# 验的是这几条（都是刻意的边界，改坏了要有人喊）：
#   · **默认关**：NCCR_CONN_ALLOW 没开时，建连一律 403（通道能跑任意命令、写文件 = 最高权限）
#   · **文件面锁在工作目录里**：`..` / 绝对路径一律 400；正常相对路径能推能拉、指纹对得上
#   · **每个动作都要 reason**：建连不需要，exec / push 没有就 400
#   · **复用执行器**：通道上的 exec 与 /api/exec/runs 是同一套（限额 / env 白名单 / 进程组回收），
#     只是记录带上 connId —— 所以 `status` 能看到"这条通道上跑过什么"
#   · **会话语义**：工作目录跨命令保持（push 过去的文件，下一条命令看得见）
#   · **TTL / 关闭**：关掉后 410；过期后也 410（两者分开说）
#   · **CLI 串联**：`ncc conn open --on <已登记的云电脑>` 复用 sandbox init 的地址与凭据
#
# 全程隔离：数据目录 / 库 / HUR_HOME 都在临时目录，端口默认 18411。
# 用法：bash scripts/conn-smoke.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-18411}"
BASE="http://127.0.0.1:${PORT}"
TMP="$(mktemp -d)"
export HUR_HOME="${TMP}/hur"

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
not_contains() { if [[ "$3" == *"$2"* ]]; then bad "$1：不该出现「$2」"; else good "$1"; fi; }

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
start_node() { # $1 = 额外 env（NCCR_CONN_ALLOW=1 之类）
  ( cd "${ROOT}/rust" && cargo build --release -q --bin ncc-registry && cp target/release/ncc-registry "${TMP}/ncc-registry" )
  env NCCR_PORT="${PORT}" NCCR_DATA_DIR="${TMP}/data" NCCR_BLOB_DIR="${TMP}/blobs" \
    NCCR_DB_PATH="${TMP}/db/nccr.sqlite" NCCR_NODE_NAME="cloud-1" NCCR_NODE_REGION="机房A" \
    NCCR_PUBLIC_URL="${BASE}" NCCR_P2P_STUN="127.0.0.1:9" \
    NCCR_EXEC_ALLOW="wasm,process" NCCR_EXEC_MAX_OUTPUT=4000 \
    ${1:-} "${TMP}/ncc-registry" >>"${TMP}/server.log" 2>&1 &
  PID=$!
  wait_http "${BASE}/api/health" 30
}
stop_node() { kill "${PID}" 2>/dev/null || true; wait "${PID}" 2>/dev/null || true; }

if [[ -z "${CLI}" ]]; then echo "找不到 ncc：先在 ncc-cli/cli 里 cargo build，或用 CLI_BIN=/path/to/ncc"; exit 1; fi

say "0. 先验「默认关」：不开 NCCR_CONN_ALLOW 时建连应当被拒"
start_node "" || { bad "节点没起来"; tail -20 "${TMP}/server.log"; exit 1; }
good "节点已就绪"
curl -sS -X POST "${BASE}/api/auth/register" -H 'Content-Type: application/json' \
  -d '{"email":"ops@conn.dev","password":"smoke1234","name":"Ops"}' >"${TMP}/reg.json"
TK="$(jval token <"${TMP}/reg.json")"
[[ -n "${TK}" ]] && good "账号就绪" || { bad "注册失败"; exit 1; }
check_code "默认关：建连 → 403" 403 -X POST "${BASE}/api/conn/connections" \
  -H "Authorization: Bearer ${TK}" -H 'Content-Type: application/json' -d '{"name":"x"}'
contains "403 里说清怎么开" "NCCR_CONN_ALLOW" "$(cat "${BODY_FILE}")"
stop_node

say "1. 打开开关重启：建通道"
start_node "NCCR_CONN_ALLOW=1" || { bad "节点没起来"; exit 1; }
good "节点已就绪（NCCR_CONN_ALLOW=1）"
check "能力面声明了 conn（客户端据此知道有没有这道门）" "yes" \
  "$(curl -sS "${BASE}/api/meta" | python3 -c 'import json,sys; print("yes" if "conn" in json.load(sys.stdin).get("capabilities",[]) else "no")')"
OPEN="$(curl -sS -X POST "${BASE}/api/conn/connections" -H "Authorization: Bearer ${TK}" \
  -H 'Content-Type: application/json' -d '{"name":"deploy","note":"发版用","ttlSec":600}')"
CID="$(printf '%s' "${OPEN}" | jval connection.id)"
WORK="$(printf '%s' "${OPEN}" | jval connection.workDir)"
[[ -n "${CID}" ]] && good "通道已建立：${CID}" || { bad "建连失败：${OPEN}"; exit 1; }
check "状态是 open" "open" "$(printf '%s' "${OPEN}" | jval connection.state)"
check "TTL 按请求生效" "600" "$(printf '%s' "${OPEN}" | jval connection.ttlSec)"
check_code "未登录建连 → 401" 401 -X POST "${BASE}/api/conn/connections" -H 'Content-Type: application/json' -d '{}'
check_code "匿名列举 → 401" 401 "${BASE}/api/conn/connections"

say "2. 通道上执行：复用执行器（命令 / 退出码 / 日志）"
RUN="$(curl -sS -X POST "${BASE}/api/conn/connections/${CID}/exec" -H "Authorization: Bearer ${TK}" \
  -H 'Content-Type: application/json' -d '{"cmd":"echo conn-hello && pwd","reason":"冒烟：通道上跑一条","wait":true}')"
check "命令成功" "succeeded" "$(printf '%s' "${RUN}" | jval exec.status)"
contains "日志里有输出" "conn-hello" "$(printf '%s' "${RUN}" | jval exec.logTail)"
contains "工作目录就是这条通道的目录" "${CID}" "$(printf '%s' "${RUN}" | jval exec.logTail)"
check "执行记录挂在这条通道上" "${CID}" "$(printf '%s' "${RUN}" | jval exec.connId)"
check_code "缺 reason → 400" 400 -X POST "${BASE}/api/conn/connections/${CID}/exec" \
  -H "Authorization: Bearer ${TK}" -H 'Content-Type: application/json' -d '{"cmd":"echo x"}'
check_code "通道上不给 wasm（要包走 sandbox run）→ 400" 400 -X POST "${BASE}/api/conn/connections/${CID}/exec" \
  -H "Authorization: Bearer ${TK}" -H 'Content-Type: application/json' -d '{"cmd":"echo x","engine":"wasm","reason":"x"}'

say "3. 文件面：推 → 目标端执行看得见 → 拉回来"
printf '#!/bin/sh\necho "deployed: $(cat app.txt)"\n' >"${TMP}/deploy.sh"
printf 'v0.3.0\n' >"${TMP}/app.txt"
PUSH1="$(curl -sS -X POST "${BASE}/api/conn/connections/${CID}/files?path=deploy.sh&reason=部署脚本&mode=700" \
  -H "Authorization: Bearer ${TK}" --data-binary @"${TMP}/deploy.sh")"
check "脚本已推到工作目录" "deploy.sh" "$(printf '%s' "${PUSH1}" | jval path)"
check "给了可执行位" "700" "$(printf '%s' "${PUSH1}" | jval mode)"
PUSH2="$(curl -sS -X POST "${BASE}/api/conn/connections/${CID}/files?path=app.txt&reason=版本号" \
  -H "Authorization: Bearer ${TK}" --data-binary @"${TMP}/app.txt")"
SHA_APP="$(printf '%s' "${PUSH2}" | jval sha256)"

EXEC2="$(curl -sS -X POST "${BASE}/api/conn/connections/${CID}/exec" -H "Authorization: Bearer ${TK}" \
  -H 'Content-Type: application/json' -d '{"cmd":"sh deploy.sh","reason":"跑刚推过去的脚本"}')"
check "推过去的文件在目标端能被执行到" "succeeded" "$(printf '%s' "${EXEC2}" | jval exec.status)"
contains "脚本读到了推过去的 app.txt" "deployed: v0.3.0" "$(printf '%s' "${EXEC2}" | jval exec.logTail)"

curl -sS -D "${TMP}/pull.headers" -o "${TMP}/pulled.txt" \
  "${BASE}/api/conn/connections/${CID}/files?path=app.txt" -H "Authorization: Bearer ${TK}"
check "拉回来的内容一致" "v0.3.0" "$(tr -d '\n' <"${TMP}/pulled.txt")"
contains "响应带指纹，可与推的时候对上" "${SHA_APP}" "$(grep -i '^x-ncc-sha256' "${TMP}/pull.headers" | tr -d '\r')"

say "4. 文件面门禁：不许跳出工作目录"
check_code "path=../x → 400" 400 "${BASE}/api/conn/connections/${CID}/files?path=../x" \
  -H "Authorization: Bearer ${TK}" --data-binary @- <<<"x"
check_code "绝对路径 → 400" 400 "${BASE}/api/conn/connections/${CID}/files?path=/etc/passwd" \
  -H "Authorization: Bearer ${TK}" --data-binary @- <<<"x"
check_code "深层 .. 也挡得住 → 400" 400 "${BASE}/api/conn/connections/${CID}/files?path=a/b/../../../../x" \
  -H "Authorization: Bearer ${TK}" --data-binary @- <<<"x"
check_code "推文件缺 reason → 400" 400 "${BASE}/api/conn/connections/${CID}/files?path=ok.txt" \
  -H "Authorization: Bearer ${TK}" --data-binary @- <<<"x"
check "真的没落盘（工作目录里没有 x）" "false" \
  "$([[ -e "${WORK}/x" ]] && echo true || echo false)"
check_code "拉一个不存在的文件 → 404" 404 "${BASE}/api/conn/connections/${CID}/files?path=nope.txt" -H "Authorization: Bearer ${TK}"

say "5. 通道状态：这段通道上跑过什么"
ST="$(curl -sS "${BASE}/api/conn/connections/${CID}" -H "Authorization: Bearer ${TK}")"
check "exec 计数对得上（≥2）" "true" "$([[ "$(printf '%s' "${ST}" | jval connection.execCount)" -ge 2 ]] && echo true || echo false)"
check "上行字节记着（脚本+版本号）" "true" "$([[ "$(printf '%s' "${ST}" | jval connection.bytesUp)" -ge 30 ]] && echo true || echo false)"
contains "列出这条通道上跑过的任务" "conn-hello" "$(printf '%s' "${ST}" | jval connection.execs)"

say "6. CLI：open（复用已登记的云电脑）→ run（推文件 + 跑脚本）→ pull → close"
"${CLI}" sandbox init --url "${BASE}" --key "${TK}" --name cloud-1 --json >/dev/null
CO="$(NCC_HOME="${TMP}/ncc" "${CLI}" conn open --on cloud-1 --name cli-chan --json)"
CLI_ID="$(printf '%s' "${CO}" | jval connection.id)"
[[ -n "${CLI_ID}" ]] && good "CLI 建连成功（复用 sandbox 登记的地址与凭据）：${CLI_ID}" || { bad "CLI 建连失败：${CO}"; exit 1; }
contains "CLI 本地登记了通道" "cli-chan" "$(NCC_HOME="${TMP}/ncc" "${CLI}" conn ls)"
check "通道凭据文件权限 0600" "600" "$(stat -f '%Lp' "${TMP}/ncc/.ncc/connections.json" 2>/dev/null || stat -c '%a' "${TMP}/ncc/.ncc/connections.json")"
not_contains "ls 不回显通道 key" "${TK}" "$(NCC_HOME="${TMP}/ncc" "${CLI}" conn ls)"

printf 'echo "batch ok: $(cat payload.txt)"\n' >"${TMP}/batch.sh"
if NCC_HOME="${TMP}/ncc" "${CLI}" conn run cli-chan --file "${TMP}/app.txt=payload.txt" --script "${TMP}/batch.sh" \
     --reason "冒烟：一批文件 + 脚本" >"${TMP}/cli-run.out" 2>&1; then
  good "conn run 退出码 0"
  contains "批量执行时读到了推过去的文件" "batch ok: v0.3.0" "$(cat "${TMP}/cli-run.out")"
else
  bad "conn run 失败：$(head -c 300 "${TMP}/cli-run.out")"
fi
NCC_HOME="${TMP}/ncc" "${CLI}" conn pull cli-chan payload.txt --to "${TMP}/pulled2.txt" >/dev/null
check "CLI pull 取回一致" "v0.3.0" "$(tr -d '\n' <"${TMP}/pulled2.txt")"
printf 'cli-push\n' >"${TMP}/cli-push.txt"
NCC_HOME="${TMP}/ncc" "${CLI}" conn push cli-chan "${TMP}/cli-push.txt" --to pushed.txt \
  --reason "冒烟：CLI 推单个文件" >/dev/null
contains "CLI push 后目标端立刻看得见" "cli-push" \
  "$(NCC_HOME="${TMP}/ncc" "${CLI}" conn exec cli-chan "cat pushed.txt" --reason "冒烟：读刚推的文件")"
if NCC_HOME="${TMP}/ncc" "${CLI}" conn exec cli-chan "exit 5" --reason "冒烟：远端非 0 退出" >/dev/null 2>&1; then
  bad "远端退出码非 0 时 CLI 却返回 0（CI 会误判成功）"
else
  good "远端退出码非 0 → CLI 也非 0"
fi
contains "status 看得到这条通道上跑过什么" "exit 5" "$(NCC_HOME="${TMP}/ncc" "${CLI}" conn status cli-chan)"
CLI_WORK="$(curl -sS "${BASE}/api/conn/connections/${CLI_ID}" -H "Authorization: Bearer ${TK}" | jval connection.workDir)"
NCC_HOME="${TMP}/ncc" "${CLI}" conn close cli-chan --purge >/dev/null
check_code "关闭后 exec → 410" 410 -X POST "${BASE}/api/conn/connections/${CLI_ID}/exec" \
  -H "Authorization: Bearer ${TK}" -H 'Content-Type: application/json' -d '{"cmd":"echo x","reason":"x"}'
contains "410 里说是关闭" "已关闭" "$(cat "${BODY_FILE}")"
check "状态是 closed（与 expired 分开说）" "closed" \
  "$(curl -sS "${BASE}/api/conn/connections/${CLI_ID}" -H "Authorization: Bearer ${TK}" | jval connection.state)"
check "close --purge 真删了工作目录" "false" "$([[ -e "${CLI_WORK}" ]] && echo true || echo false)"
check "另一条通道不受影响（各删各的）" "true" "$([[ -e "${WORK}" ]] && echo true || echo false)"
CLI_ST="$(NCC_HOME="${TMP}/ncc" "${CLI}" conn status cli-chan)"
contains "关掉之后账本还能看（status 显示 closed）" "closed" "${CLI_ST}"
contains "账本里还留着这条通道上跑过什么" "exit 5" "${CLI_ST}"
contains "关掉的原因写在提示里" "被关闭" "${CLI_ST}"
contains "本地登记默认留着（ls 里显示 closed，不是删掉）" "closed" "$(NCC_HOME="${TMP}/ncc" "${CLI}" conn ls)"
NCC_HOME="${TMP}/ncc" "${CLI}" conn close cli-chan --forget >/dev/null
not_contains "要清掉本地登记得显式 --forget" "cli-chan" "$(NCC_HOME="${TMP}/ncc" "${CLI}" conn ls)"

say "7. TTL 到期：另一条通道自然过期"
SHORT="$(curl -sS -X POST "${BASE}/api/conn/connections" -H "Authorization: Bearer ${TK}" \
  -H 'Content-Type: application/json' -d '{"name":"ttl","ttlSec":1}')"
CID2="$(printf '%s' "${SHORT}" | jval connection.id)"
sleep 2
check_code "过期后 exec → 410" 410 -X POST "${BASE}/api/conn/connections/${CID2}/exec" \
  -H "Authorization: Bearer ${TK}" -H 'Content-Type: application/json' -d '{"cmd":"echo x","reason":"x"}'
contains "410 里说过期（与已关闭分开）" "过期" "$(cat "${BODY_FILE}")"
check_code "过期后列举还是能看到（不会被默默删掉）" 200 "${BASE}/api/conn/connections" -H "Authorization: Bearer ${TK}"
check "列表里状态是 expired（不是 closed —— 两件事分开说）" "expired" \
  "$(curl -sS "${BASE}/api/conn/connections" -H "Authorization: Bearer ${TK}" | python3 -c '
import json,sys
d=json.load(sys.stdin)["connections"]
print(next((c["state"] for c in d if c["id"]==sys.argv[1]), "missing"))
' "${CID2}")"
check "过期的不再显示成 open（不能假装可用）" "false" \
  "$(curl -sS "${BASE}/api/conn/connections" -H "Authorization: Bearer ${TK}" | python3 -c '
import json,sys
d=json.load(sys.stdin)["connections"]
print(str(next((c["state"]=="open" for c in d if c["id"]==sys.argv[1]), True)).lower())
' "${CID2}")"

say "结果"
echo "  通过 ${PASS} · 失败 ${FAIL}"
[[ "${FAIL}" == "0" ]] || exit 1
