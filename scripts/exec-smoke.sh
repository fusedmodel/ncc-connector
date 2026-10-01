#!/usr/bin/env bash
# NCC Remote Cloud Computer（云电脑 = 能接活的 ncc-registry 节点）端到端冒烟。
#
# 验的是这几条（都是刻意的边界，改坏了要有人喊）：
#   · **能不能跑看本机事实**：`/api/exec/kinds` 直接回答每个引擎 enabled/why；节点在
#     `/api/meta` 的 node 里以**自证**形式报出 run:*（不是让调用方猜标签）
#   · **默认只放行 wasm**：process/container 要运维显式开；没放行的引擎**连字节都不收**（403）
#   · **每条任务都要 reason**（R12 同一条规矩）；未登录 401
#   · **取消要真停**：进程组整组回收，状态立刻变 canceled（不是把状态改了就完事）
#   · **超时/截断如实记**：timeout 状态 + 日志截断标记
#   · **CLI `ncc sandbox`**：init（ip/port/key）→ 登记进**既有**沙箱环境表 + 凭据自检；
#     run 挑机器、提交、等待、**退出码跟随远端**；--require 按现探挑不到就明确报
#
# 全程隔离：数据目录 / 库 / blob / HUR_HOME 都在临时目录，端口默认 18409。
# 用法：bash scripts/exec-smoke.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-18409}"
BASE="http://127.0.0.1:${PORT}"
TMP="$(mktemp -d)"
export HUR_HOME="${TMP}/hur"   # 沙箱环境登记（含凭据）落这里，别碰真实的 ~/.harnessuse

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
# 等一个任务进终态，打印它的 status
wait_done() {
  local id="$1" tk="$2" deadline=$((SECONDS + 40))
  while (( SECONDS < deadline )); do
    local st; st="$(curl -sS "${BASE}/api/exec/runs/${id}" -H "Authorization: Bearer ${tk}" | jval run.status)"
    case "${st}" in succeeded|failed|timeout|canceled) printf '%s' "${st}"; return 0;; esac
    sleep 0.3
  done
  printf 'never'
}

if [[ -z "${CLI}" ]]; then echo "找不到 ncc：先在 ncc-cli/cli 里 cargo build，或用 CLI_BIN=/path/to/ncc"; exit 1; fi

say "0. 构建 + 启动云电脑（:${PORT}，放行 wasm+process，runner 用替身）"
# 替身 runner：收到 `hur run <dir> --exec --json` 就打一行 JSON。
# 这样冒烟验的是**调度链路**（收活→跑→日志→退出码），不依赖真 wasm 包；
# 真跑 wasm 的是 `ncc hur run --exec`（它自己有测试）。
cat > "${TMP}/fake-ncc" <<'STUB'
#!/bin/sh
echo "{\"ok\":true,\"engine\":\"wasm\",\"argv\":\"$*\"}"
exit 0
STUB
chmod +x "${TMP}/fake-ncc"

( cd "${ROOT}" && go build -o "${TMP}/ncc-registry" ./cmd/ncc-registry )
NCCR_PORT="${PORT}" NCCR_DATA_DIR="${TMP}/data" NCCR_BLOB_DIR="${TMP}/blobs" \
NCCR_DB_PATH="${TMP}/db/nccr.sqlite" NCCR_NODE_NAME="cloud-1" NCCR_NODE_REGION="机房A" \
NCCR_PUBLIC_URL="${BASE}" NCCR_P2P_STUN="127.0.0.1:9" \
NCCR_EXEC_ALLOW="wasm,process" NCCR_EXEC_RUNNER="${TMP}/fake-ncc" \
NCCR_EXEC_MAX_OUTPUT=200 NCCR_EXEC_TIMEOUT=60s \
NCCR_EXEC_TAGS="hur,hur-sandbox,os" \
  "${TMP}/ncc-registry" >"${TMP}/server.log" 2>&1 &
PID=$!
wait_http "${BASE}/api/health" 30 || { bad "云电脑没起来"; tail -20 "${TMP}/server.log"; exit 1; }
good "云电脑已就绪"

say "1. 能力自述：这台到底能跑什么（公开）"
KINDS="$(curl -sS "${BASE}/api/exec/kinds")"
check "wasm 可用（runner 就绪）" "True" "$(printf '%s' "${KINDS}" | jval exec.kinds.0.enabled)"
check "process 可用（运维放行了 + 有 shell）" "True" "$(printf '%s' "${KINDS}" | jval exec.kinds.1.enabled)"
check "container 不可用（没放行）" "False" "$(printf '%s' "${KINDS}" | jval exec.kinds.2.enabled)"
contains "并且说清为什么" "NCCR_EXEC_ALLOW" "$(printf '%s' "${KINDS}" | jval exec.kinds.2.why)"
contains "上限如实报出（日志 200 字节）" "200" "$(printf '%s' "${KINDS}" | jval limits.maxOutputBytes)"
contains "标签报出来（供按标签挑机器）" "hur-sandbox" "$(printf '%s' "${KINDS}" | jval exec.tags)"
META="$(curl -sS "${BASE}/api/meta")"
contains "meta 里声明了 exec 能力面" "exec" "$(printf '%s' "${META}" | jval capabilities)"
contains "节点以自证形式报出 run:wasm" "run:wasm" "$(printf '%s' "${META}" | jval node.capabilitiesVerified)"
not_contains "没放行的 container 不冒充自证" "run:container" "$(printf '%s' "${META}" | jval node.capabilitiesVerified)"

say "2. 鉴权与入参门禁"
curl -sS -X POST "${BASE}/api/auth/register" -H 'Content-Type: application/json' \
  -d '{"email":"ops@cloud.dev","password":"smoke1234","name":"Ops"}' >"${TMP}/reg.json"
TK="$(jval token <"${TMP}/reg.json")"
[[ -n "${TK}" ]] && good "账号就绪" || { bad "注册失败"; exit 1; }
check_code "未登录提交 → 401" 401 -X POST "${BASE}/api/exec/runs" -H 'Content-Type: application/json' -d '{"cmd":"echo x"}'
check_code "缺 reason → 400" 400 -X POST "${BASE}/api/exec/runs" -H "Authorization: Bearer ${TK}" \
  -H 'Content-Type: application/json' -d '{"engine":"process","cmd":"echo x"}'
check_code "没放行的引擎 → 403（连字节都不收）" 403 -X POST "${BASE}/api/exec/runs" -H "Authorization: Bearer ${TK}" \
  -H 'Content-Type: application/json' -d '{"engine":"container","cmd":"echo x","image":"alpine","reason":"smoke"}'
contains "403 里给出下一步（怎么放行）" "NCCR_EXEC_ALLOW" "$(cat "${BODY_FILE}")"
check_code "不认识的引擎 → 400" 400 -X POST "${BASE}/api/exec/runs" -H "Authorization: Bearer ${TK}" \
  -H 'Content-Type: application/json' -d '{"engine":"js","cmd":"echo x","reason":"smoke"}'
check_code "timeoutSec 超过节点上限 → 400" 400 -X POST "${BASE}/api/exec/runs" -H "Authorization: Bearer ${TK}" \
  -H 'Content-Type: application/json' -d '{"engine":"process","cmd":"echo x","reason":"smoke","timeoutSec":9999}'
check_code "别人的任务看不到（管理员也不乱看）→ 404/403" 404 "${BASE}/api/exec/runs/ER-nope" -H "Authorization: Bearer ${TK}"

say "3. 跑一条命令（OS 敏感任务那条路）"
RUN="$(curl -sS -X POST "${BASE}/api/exec/runs" -H "Authorization: Bearer ${TK}" -H 'Content-Type: application/json' \
  -d '{"engine":"process","cmd":"echo ncc-exec-ok","reason":"冒烟：验证 process 引擎"}')"
RID="$(printf '%s' "${RUN}" | jval run.id)"
[[ -n "${RID}" ]] && good "任务已受理：${RID}" || { bad "提交失败：${RUN}"; exit 1; }
ST="$(wait_done "${RID}" "${TK}")"
check "任务成功" "succeeded" "${ST}"
DETAIL="$(curl -sS "${BASE}/api/exec/runs/${RID}" -H "Authorization: Bearer ${TK}")"
check "退出码 0" "0" "$(printf '%s' "${DETAIL}" | jval run.exitCode)"
contains "日志里有输出" "ncc-exec-ok" "$(printf '%s' "${DETAIL}" | jval run.logTail)"
check_code "日志全文可单独取（text/plain）" 200 "${BASE}/api/exec/runs/${RID}/log" -H "Authorization: Bearer ${TK}"
contains "日志响应带状态头" "succeeded" "$(grep -i '^x-ncc-exec-status' "${BODY_FILE}" 2>/dev/null || curl -sS -D - -o /dev/null "${BASE}/api/exec/runs/${RID}/log" -H "Authorization: Bearer ${TK}" | grep -i '^x-ncc-exec-status' || true)"
check "工作目录留在节点上（日志与中间产物都在里面）" "true" \
  "$([[ -n "$(printf '%s' "${DETAIL}" | jval run.workDir)" ]] && echo true || echo false)"
check "reason 进了账本" "冒烟：验证 process 引擎" "$(printf '%s' "${DETAIL}" | jval run.reason)"

say "4. 提交一个包（wasm 那条路；runner 是替身）"
PKG_DIR="${TMP}/pkg"
"${CLI}" hur init --kind agent --name "Cloud Demo" --dir "${PKG_DIR}" >/dev/null
"${CLI}" hur build "${PKG_DIR}" >/dev/null
"${CLI}" hur pack "${PKG_DIR}" >/dev/null   # 产包（build 只定 lock；产包才出 dist/*.hur.gz）
HUR_FILE="$(find "${PKG_DIR}/dist" -maxdepth 1 -type f \( -name '*.hur' -o -name '*.hur.gz' \) 2>/dev/null | head -1)"
[[ -f "${HUR_FILE}" ]] && good "本地已产包（$(basename "${HUR_FILE}")）" || { bad "产包失败"; exit 1; }
RUN2="$(curl -sS -X POST "${BASE}/api/exec/runs?engine=wasm&reason=%E5%86%92%E7%83%9F%E5%8C%85" \
  -H "Authorization: Bearer ${TK}" -H 'Content-Type: application/octet-stream' --data-binary @"${HUR_FILE}")"
RID2="$(printf '%s' "${RUN2}" | jval run.id)"
[[ -n "${RID2}" ]] && good "包任务已受理：${RID2}" || { bad "包提交失败：$(head -c 200 <<<"${RUN2}")"; }
if [[ -n "${RID2}" ]]; then
  check "包任务成功" "succeeded" "$(wait_done "${RID2}" "${TK}")"
  contains "runner 看到的确实是解出来的包目录" "/pkg" "$(curl -sS "${BASE}/api/exec/runs/${RID2}" -H "Authorization: Bearer ${TK}" | jval run.logTail)"
fi

say "5. 超时 / 取消 / 截断：如实记"
RUN3="$(curl -sS -X POST "${BASE}/api/exec/runs" -H "Authorization: Bearer ${TK}" -H 'Content-Type: application/json' \
  -d '{"engine":"process","cmd":"sleep 30","reason":"冒烟：超时","timeoutSec":2}')"
RID3="$(printf '%s' "${RUN3}" | jval run.id)"
T0="${SECONDS}"
check "超时任务被判 timeout" "timeout" "$(wait_done "${RID3}" "${TK}")"
ELAPSED=$((SECONDS - T0))
[[ "${ELAPSED}" -le 12 ]] && good "并且是**按时**被终止（${ELAPSED}s，没等满 30s）" || bad "超时没生效（用了 ${ELAPSED}s）"

RUN4="$(curl -sS -X POST "${BASE}/api/exec/runs" -H "Authorization: Bearer ${TK}" -H 'Content-Type: application/json' \
  -d '{"engine":"process","cmd":"sleep 300","reason":"冒烟：取消"}')"
RID4="$(printf '%s' "${RUN4}" | jval run.id)"
sleep 1
curl -sS -X DELETE "${BASE}/api/exec/runs/${RID4}" -H "Authorization: Bearer ${TK}" >/dev/null
check "取消后状态是 canceled" "canceled" "$(wait_done "${RID4}" "${TK}")"
if pgrep -f "sleep 300" >/dev/null 2>&1; then bad "取消没真停：sleep 300 还在跑"; else good "取消真停（进程组整组回收）"; fi

RUN5="$(curl -sS -X POST "${BASE}/api/exec/runs" -H "Authorization: Bearer ${TK}" -H 'Content-Type: application/json' \
  -d '{"engine":"process","cmd":"head -c 2000 /dev/zero | tr \"\\0\" x","reason":"冒烟：截断"}')"
RID5="$(printf '%s' "${RUN5}" | jval run.id)"
wait_done "${RID5}" "${TK}" >/dev/null
check "日志超上限被截断（如实标记）" "True" \
  "$(curl -sS "${BASE}/api/exec/runs/${RID5}" -H "Authorization: Bearer ${TK}" | jval run.logTruncated)"

say "6. 任务列表：只看自己的"
LIST="$(curl -sS "${BASE}/api/exec/runs" -H "Authorization: Bearer ${TK}")"
check_code "匿名列任务 → 401" 401 "${BASE}/api/exec/runs"
check "我的任务都在（≥5 条）" "true" "$([[ "$(printf '%s' "${LIST}" | jval total)" -ge 5 ]] && echo true || echo false)"
contains "列表里有引擎与状态" "process" "$(printf '%s' "${LIST}" | jval runs.0.engine)"

say "7. CLI：ncc sandbox init / ls / status / run"
NAME="cloud-1"
INIT_OUT="$("${CLI}" sandbox init --url "${BASE}" --key "${TK}" --name "${NAME}" --note "冒烟用云电脑" --default --json)"
check "init 登记成功" "${NAME}" "$(printf '%s' "${INIT_OUT}" | jval environment.id)"
check "init 记下了能跑的引擎（现探的）" "true" \
  "$(printf '%s' "${INIT_OUT}" | python3 -c 'import json,sys; e=json.load(sys.stdin)["engines"]; print("true" if "process" in e and "wasm" in e and "container" not in e else "false")')"
check "凭据已登记" "true" "$(printf '%s' "${INIT_OUT}" | python3 -c 'import json,sys; print("true" if json.load(sys.stdin)["environment"]["auth"]["token"] else "false")')"
ENV_FILE="${HUR_HOME}/environments.json"
check "凭据文件权限 0600（里面有 key）" "600" "$(stat -f '%Lp' "${ENV_FILE}" 2>/dev/null || stat -c '%a' "${ENV_FILE}")"
LS_OUT="$("${CLI}" sandbox ls)"
contains "ls 列出来并显示引擎" "process" "${LS_OUT}"
contains "ls 显示有凭据（但不回显 token）" "有" "${LS_OUT}"
not_contains "ls 不回显 token 明文" "${TK}" "${LS_OUT}"
contains "status 现探到在线" "在线" "$("${CLI}" sandbox status "${NAME}")"
contains "status 报出 HUR 运行时就绪" "就绪" "$("${CLI}" sandbox status "${NAME}")"

if "${CLI}" sandbox run --on "${NAME}" --cmd "echo cli-ok" --reason "冒烟：CLI 提交" >"${TMP}/cli-run.out" 2>&1; then
  good "sandbox run 退出码 0"
  contains "CLI 打回了远端日志" "cli-ok" "$(cat "${TMP}/cli-run.out")"
else
  bad "sandbox run 失败：$(head -c 300 "${TMP}/cli-run.out")"
fi

if "${CLI}" sandbox run --on "${NAME}" --cmd "exit 7" --reason "冒烟：远端非 0 退出" >/dev/null 2>&1; then
  bad "远端退出码非 0 时 CLI 却返回 0（CI 会误判成功）"
else
  good "远端退出码非 0 → CLI 也非 0（CI 能拦住）"
fi

no_such="$("${CLI}" sandbox run --require container --cmd "echo x" --reason "冒烟：没有一台满足" 2>&1 || true)"
contains "挑不到满足条件的机器时明确报" "没有一台云电脑能满足" "${no_such}"
contains "并说明缺什么" "container" "${no_such}"

say "8. init 的拒绝路径（人最容易踩的两条）"
bad_key="$(HUR_HOME="${TMP}/hur-bad" "${CLI}" sandbox init --url "${BASE}" --key "ncc_bogus" --name bad 2>&1 || true)"
contains "错 key 当场报错（不是安静登记）" "凭据自检失败" "${bad_key}"
not_exec="$(HUR_HOME="${TMP}/hur-noexec" "${CLI}" sandbox init --url "${BASE}/nope" --name x 2>&1 || true)"
contains "连不上就明确说连不上" "连不上" "${not_exec}"

say "结果"
echo "  通过 ${PASS} · 失败 ${FAIL}"
[[ "${FAIL}" == "0" ]] || exit 1
