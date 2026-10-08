#!/usr/bin/env bash
# NCC State 端到端冒烟：**知识库 kb / 记忆 mem / 检查点 ckpt**（ncc-registry + ncc-cli 一起跑）。
#
# 为什么要单独一个脚本：这三样横跨两个仓库 —— 服务端托管字节与元数据，
# CLI 是 Agent 读写它们的手。只测一边都证明不了"能用"。
#
# 脚本自带启停，端口默认 18391/18392（避开开发实例），并且：
#   · 数据目录 / 库文件 / blob 全在临时目录里；
#   · `NCC_HOME` 指向临时目录 —— **不碰你真实的 ~/.ncc**。
#
# 用法：bash scripts/state-smoke.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-18391}"
BASE="http://127.0.0.1:${PORT}"
TMP="$(mktemp -d)"
WORK="${TMP}/work"
# CLI 在哪：ncc-cli 仓库可能与本仓库并列（../ncc-cli/cli），也可能本仓库就在它里面
# （../cli，子模块形态）。两种都试，也允许用 CLI_BIN 直接指定。
find_cli() {
  if [[ -n "${CLI_BIN:-}" ]]; then printf '%s' "${CLI_BIN}"; return; fi
  for cand in "${ROOT}/../cli/target/debug/ncc" "${ROOT}/../ncc-cli/cli/target/debug/ncc" \
              "${ROOT}/../../cli/target/debug/ncc"; do
    if [[ -x "${cand}" ]]; then printf '%s' "${cand}"; return; fi
  done
  printf ''
}
CLI="$(find_cli)"
if [[ -z "${CLI}" ]]; then
  echo "找不到 ncc 可执行文件：请先在 ncc-cli/cli 里 cargo build，或用 CLI_BIN=/path/to/ncc 指定" >&2
  exit 1
fi

export NCC_HOME="${TMP}/home"

PASS=0
FAIL=0

cleanup() {
  [[ -n "${PID:-}" ]] && kill "${PID}" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT

say()  { printf '\n\033[1m%s\033[0m\n' "$1"; }
good() { printf '  \033[32m✓\033[0m %s\n' "$1"; PASS=$((PASS + 1)); }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$1"; FAIL=$((FAIL + 1)); }

check() {
  if [[ "$2" == "$3" ]]; then good "$1（$3）"; else bad "$1：期望 $2，实际 $3"; fi
}

# contains <描述> <期望子串> <实际>
contains() {
  if [[ "$3" == *"$2"* ]]; then good "$1"; else bad "$1：输出里没有「$2」（实际：$(printf '%s' "$3" | head -c 300)）"; fi
}

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

wait_http() {
  local url="$1" deadline=$((SECONDS + $2))
  while (( SECONDS < deadline )); do
    curl -fsS "$url" >/dev/null 2>&1 && return 0
    sleep 0.3
  done
  return 1
}

mkdir -p "${WORK}"

say "0. 构建 + 启动一个隔离的 ncc-registry（:${PORT}）"
( cd "${ROOT}/rust" && cargo build --release -q --bin ncc-registry && cp target/release/ncc-registry "${TMP}/ncc-registry" )
NCCR_PORT="${PORT}" NCCR_DATA_DIR="${TMP}/data" NCCR_NODE_NAME="state-smoke" \
  NCCR_NODE_REGION="本地" NCCR_P2P_STUN="127.0.0.1:9" \
  "${TMP}/ncc-registry" >"${TMP}/registry.log" 2>&1 &
PID=$!
wait_http "${BASE}/api/meta" 20 || { bad "registry 没起来"; cat "${TMP}/registry.log"; exit 1; }
good "registry 已就绪"

say "1. 两个账号（alice 拥有状态，bob 用来验可见性）"
ALICE="$(curl -sS -X POST "${BASE}/api/auth/register" -H 'Content-Type: application/json' \
  -d '{"email":"alice@corp.com","password":"smoke1234","name":"Alice"}')"
TOK_A="$(printf '%s' "${ALICE}" | jval token)"
BOB="$(curl -sS -X POST "${BASE}/api/auth/register" -H 'Content-Type: application/json' \
  -d '{"email":"bob@corp.com","password":"smoke1234","name":"Bob"}')"
TOK_B="$(printf '%s' "${BOB}" | jval token)"
[[ -n "${TOK_A}" && -n "${TOK_B}" ]] && good "@alice / @bob 已注册" || { bad "注册失败"; exit 1; }

say "2. CLI 登录（--base 指向这个隔离实例）"
"${CLI}" --base "${BASE}" login --email alice@corp.com --password smoke1234 >/dev/null
check "ncc me 认到账号" "alice@corp.com" "$("${CLI}" --base "${BASE}" me | grep -o 'alice@corp.com' | head -1)"

say "3. 知识库：写 / 读 / 检索 / 版本 / 归档"
OUT="$("${CLI}" --base "${BASE}" kb set refund --title "退款流程" \
  --summary "客服退款的标准步骤" --tag support,refund --source "https://wiki/refund" \
  --content "第一步：核对订单号。第二步：确认到账方式。" 2>&1)"
contains "kb set 新建成功" "新建" "${OUT}"
contains "kb set 报出引用" "@alice/refund" "${OUT}"

OUT="$("${CLI}" --base "${BASE}" kb set refund --title "退款流程" \
  --note "补了第三步" --content "第一步：核对订单号。第二步：确认到账方式。第三步：留痕。" 2>&1)"
contains "第二次写入是新版本" "v2" "${OUT}"

OUT="$("${CLI}" --base "${BASE}" kb ls 2>&1)"
contains "kb ls 列到文档" "@alice/refund" "${OUT}"

OUT="$("${CLI}" --base "${BASE}" kb get @alice/refund 2>&1)"
contains "kb get 拿到正文" "第三步：留痕" "${OUT}"

OUT="$("${CLI}" --base "${BASE}" kb get @alice/refund --revision 1 2>&1)"
contains "kb get --revision 1 拿到旧版" "第二步：确认到账方式。" "${OUT}"
if [[ "${OUT}" == *"第三步"* ]]; then bad "旧版里不该有新内容"; else good "旧版正文确实是旧的"; fi

OUT="$("${CLI}" --base "${BASE}" kb history @alice/refund 2>&1)"
contains "kb history 有两版" "v1" "${OUT}"
contains "kb history 记住了变更说明" "补了第三步" "${OUT}"

OUT="$("${CLI}" --base "${BASE}" kb search 退款 2>&1)"
contains "kb search 命中" "@alice/refund" "${OUT}"
contains "kb search 说清是关键词检索" "关键词" "${OUT}"

OUT="$("${CLI}" --base "${BASE}" kb archive @alice/refund 2>&1)"
contains "kb archive 生效" "已归档" "${OUT}"
OUT="$("${CLI}" --base "${BASE}" kb ls 2>&1)"
if [[ "${OUT}" == *"@alice/refund"* ]]; then bad "归档的不该出现在默认列表"; else good "归档的默认不出现"; fi
OUT="$("${CLI}" --base "${BASE}" kb ls --archived 2>&1)"
contains "--archived 能看到归档的" "@alice/refund" "${OUT}"
"${CLI}" --base "${BASE}" kb restore @alice/refund >/dev/null
good "kb restore 恢复"

say "4. 知识库：按**包的声明**拉取（ncc kb pull）"
PKG="${WORK}/pkg"
mkdir -p "${PKG}"
cat >"${PKG}/hur.json" <<'JSON'
{
  "spec": "harness-use-package/v1",
  "id": "refund-agent",
  "name": "退款助手",
  "version": "0.1.0",
  "kind": "agent",
  "state": {
    "kb": [{ "ref": "@alice/refund", "mode": "read" }],
    "memory": { "subject": "self", "ttl_days": 30 },
    "checkpoints": { "enabled": true, "label": "run", "keep_local": 2 }
  },
  "permissions": { "network": ["api.example.com"] }
}
JSON
# 先验：包**没**声明 kb 时，pull 必须明说"没声明"，而不是悄悄成功。
NOPKG="${WORK}/nopkg"
mkdir -p "${NOPKG}"
python3 - "${PKG}/hur.json" "${NOPKG}/hur.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
d.pop("state")
json.dump(d, open(sys.argv[2], "w"), ensure_ascii=False, indent=2)
PY
OUT="$("${CLI}" --base "${BASE}" kb pull --package "${NOPKG}" --out "${WORK}/kb-none" 2>&1 || true)"
contains "没声明 state.kb 时明确拒绝" "没有声明" "${OUT}"
[[ -d "${WORK}/kb-none" ]] && bad "被拒时不该写任何东西" || good "被拒时没有落盘"

OUT="$("${CLI}" --base "${BASE}" kb pull --package "${PKG}" 2>&1)"
contains "pull 读到了声明" "声明了 1 条知识库要求" "${OUT}"
contains "pull 报出记忆声明（不拉，但要说）" "记忆声明" "${OUT}"
contains "pull 报出检查点声明" "检查点声明" "${OUT}"
contains "pull 落盘到 ~/.ncc/kb" "${NCC_HOME}/.ncc/kb" "${OUT}"

CACHE="${NCC_HOME}/.ncc/kb"
[[ -f "${CACHE}/alice/refund.md" ]] && good "文档按 @ns/slug 落成文件" || bad "没找到 ${CACHE}/alice/refund.md"
contains "落盘正文与节点一致" "第三步：留痕" "$(cat "${CACHE}/alice/refund.md")"
[[ -f "${CACHE}/index.json" ]] && good "写了索引（供增量同步）" || bad "没有 index.json"
check "索引记了 spec" "ncc-kb-cache/v1" "$(python3 -c "import json;print(json.load(open('${CACHE}/index.json'))['spec'])")"

OUT="$("${CLI}" --base "${BASE}" kb pull --package "${PKG}" 2>&1)"
contains "第二次 pull 走增量（未变的不重写）" "未变 1" "${OUT}"

say "5. 记忆：写 / 读 / 覆盖 / TTL / gc"
OUT="$("${CLI}" --base "${BASE}" mem set timezone "Asia/Shanghai" --source manual 2>&1)"
contains "mem set 记下" "记下" "${OUT}"
OUT="$("${CLI}" --base "${BASE}" mem get timezone 2>&1)"
contains "mem get 读到值" "Asia/Shanghai" "${OUT}"
contains "mem get 给出分类" "事实" "${OUT}"

OUT="$("${CLI}" --base "${BASE}" mem set timezone "UTC+8" 2>&1)"
contains "同键再写是更新" "更新" "${OUT}"
contains "更新后 Revision+1" "rev 2" "${OUT}"

"${CLI}" --base "${BASE}" mem set tmp-note "这条会过期" --ttl-days 1 >/dev/null
OUT="$("${CLI}" --base "${BASE}" mem ls 2>&1)"
contains "mem ls 列出两条" "timezone" "${OUT}"
contains "mem ls 也列出带 TTL 的" "tmp-note" "${OUT}"

# 把 TTL 推到过去（模拟时间流逝）：读时即视为不存在。
python3 - "${TMP}/data/ncc-registry.db" <<'PY' 2>/dev/null || true
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("UPDATE mem_entries SET expires_at = '2000-01-01 00:00:00+00:00' WHERE key = 'tmp-note'")
c.commit()
PY
OUT="$("${CLI}" --base "${BASE}" mem get tmp-note 2>&1 || true)"
contains "过期后读不到（读时判定）" "没有这条记忆" "${OUT}"
OUT="$("${CLI}" --base "${BASE}" mem ls --expired 2>&1)"
contains "显式要过期项才看得到" "tmp-note" "${OUT}"
OUT="$("${CLI}" --base "${BASE}" mem gc 2>&1)"
contains "gc 真正删掉过期条目" "清掉 1 条" "${OUT}"

OUT="$("${CLI}" --base "${BASE}" mem rm timezone 2>&1)"
contains "mem rm 按 key 删除" "删除" "${OUT}"

say "6. 检查点：打点 / 血缘 / 取回核对 / 清理"
head -c 4096 /dev/urandom >"${WORK}/snap1.bin"
head -c 8192 /dev/urandom >"${WORK}/snap2.bin"
OUT="$("${CLI}" --base "${BASE}" ckpt save --name snap1 --ref @alice/agent --label run \
  --file "${WORK}/snap1.bin" --meta loss=0.42 --summary "第一轮" 2>&1)"
contains "ckpt save 打点成功" "打点" "${OUT}"
ID1="$(printf '%s' "${OUT}" | grep -oE 'CK-[a-z0-9]+-[0-9a-f]+' | head -1)"
[[ -n "${ID1}" ]] && good "拿到检查点 id（${ID1}）" || bad "没解析出 id"

OUT="$("${CLI}" --base "${BASE}" ckpt save --name snap2 --ref @alice/agent --label run \
  --file "${WORK}/snap2.bin" --parent-last 2>&1)"
contains "第二个点自动接上血缘" "打点" "${OUT}"
ID2="$(printf '%s' "${OUT}" | grep -oE 'CK-[a-z0-9]+-[0-9a-f]+' | head -1)"
check "parent 指向第一个点" "${ID1}" "$(curl -sS "${BASE}/api/ckpt/${ID2}" -H "Authorization: Bearer ${TOK_A}" | jval checkpoint.parent)"

OUT="$("${CLI}" --base "${BASE}" ckpt ls --ref @alice/agent 2>&1)"
contains "ckpt ls 列出两个点" "snap1" "${OUT}"
contains "ckpt ls 也列出第二个" "snap2" "${OUT}"

LINEAGE="$("${CLI}" --base "${BASE}" ckpt lineage "${ID2}" --json)"
check "血缘回溯到起点（2 个点）" "${ID1}" "$(printf '%s' "${LINEAGE}" | jval lineage.1.id)"
check "血缘最新在前" "${ID2}" "$(printf '%s' "${LINEAGE}" | jval lineage.0.id)"

OUT="$("${CLI}" --base "${BASE}" ckpt pull "${ID1}" --out "${WORK}/restored.bin" 2>&1)"
contains "取回时核对摘要" "已核对" "${OUT}"
if cmp -s "${WORK}/snap1.bin" "${WORK}/restored.bin"; then good "取回的字节与打点时**逐字节相同**"; else bad "取回的字节不一致"; fi

OUT="$("${CLI}" --base "${BASE}" ckpt prune --ref @alice/agent --keep 1 2>&1)"
contains "prune 保留最新 1 个" "标 pruned 1 个" "${OUT}"
contains "prune 同时删字节" "删掉字节 1 份" "${OUT}"
check "计数只剩 active 的 1 个" "1" "$(curl -sS "${BASE}/api/meta" | jval counts.checkpoints)"
OUT="$("${CLI}" --base "${BASE}" ckpt ls --pruned --ref @alice/agent 2>&1)"
contains "被清理的元数据还在（历史不留空洞）" "snap1" "${OUT}"
check_code "被清理的点的字节取不到了" 403 "${BASE}/api/ckpt/${ID1}/bytes"

# 两步走：先建元数据（声明 size+digest），再传字节；**传错字节必须被拒**
REAL_SUM="$(python3 -c "import hashlib,sys;print('sha256:'+hashlib.sha256(open('${WORK}/snap1.bin','rb').read()).hexdigest())")"
META_ONLY="$(curl -sS -X POST "${BASE}/api/ckpt" -H "Authorization: Bearer ${TOK_A}" -H 'Content-Type: application/json' \
  -d "{\"name\":\"probe\",\"label\":\"manual\",\"ref\":\"@alice/probe\",\"digest\":\"${REAL_SUM}\",\"size\":4096}")"
PROBE_ID="$(printf '%s' "${META_ONLY}" | jval checkpoint.id)"
[[ -n "${PROBE_ID}" ]] && good "元数据点可以只声明摘要与大小（字节后传）" || bad "建元数据点失败：${META_ONLY}"
check_code "传与实际摘要不符的字节 → 400" 400 -X PUT "${BASE}/api/ckpt/${PROBE_ID}/blob" \
  -H "Authorization: Bearer ${TOK_A}" --data-binary "tampered-bytes-not-the-snapshot"
check_code "传正确的字节 → 200" 200 -X PUT "${BASE}/api/ckpt/${PROBE_ID}/blob" \
  -H "Authorization: Bearer ${TOK_A}" --data-binary "@${WORK}/snap1.bin"
check_code "已经传过字节的点不能重复传" 409 -X PUT "${BASE}/api/ckpt/${PROBE_ID}/blob" \
  -H "Authorization: Bearer ${TOK_A}" --data-binary "@${WORK}/snap1.bin"

say "7. 可见性：默认私有 + 显式 state 授权（bob 的视角）"
check_code "bob 读 alice 的私有文档 → 403" 403 \
  "${BASE}/api/kb/@alice/refund" -H "Authorization: Bearer ${TOK_B}"
check_code "bob 指名读 alice 的记忆 → 403" 403 \
  "${BASE}/api/mem/lookup?namespace=alice&subject=self&key=timezone" -H "Authorization: Bearer ${TOK_B}"
check_code "bob 在自己库里查同名 key → 404（不是他的，也不是他的库）" 404 \
  "${BASE}/api/mem/lookup?subject=self&key=timezone" -H "Authorization: Bearer ${TOK_B}"
check_code "匿名读私有文档 → 403" 403 "${BASE}/api/kb/@alice/refund"
check_code "匿名读检查点字节 → 403" 403 "${BASE}/api/ckpt/${ID1}/bytes"
check "bob 的列表里没有 alice 的东西" "0" \
  "$(curl -sS "${BASE}/api/mem?limit=50" -H "Authorization: Bearer ${TOK_B}" | jval total)"

GRANT="$(curl -sS -X POST "${BASE}/api/grants" -H "Authorization: Bearer ${TOK_A}" -H 'Content-Type: application/json' \
  -d '{"ref":"@bob","kind":"state"}')"
if printf '%s' "${GRANT}" | grep -q '"id"'; then good "alice 给 bob 授了 state（${ALICE_ID:0:6}…）"; else bad "授权失败：${GRANT}"; fi
check_code "授权后 bob 能读 alice 的文档" 200 \
  "${BASE}/api/kb/@alice/refund" -H "Authorization: Bearer ${TOK_B}"
check_code "但**不能写**（被授权者只有读）" 403 -X POST "${BASE}/api/kb" \
  -H "Authorization: Bearer ${TOK_B}" -H 'Content-Type: application/json' \
  -d '{"namespace":"alice","slug":"hacked","title":"x","content":"y"}'

say "8. 公共可读面：公开文档匿名可读"
"${CLI}" --base "${BASE}" kb set public-manual --title "公开手册" --public \
  --content "这份谁都能读。" >/dev/null
check_code "匿名读公开文档 → 200" 200 "${BASE}/api/kb/@alice/public-manual"
check "匿名列表只给公开的" "1" "$(curl -sS "${BASE}/api/kb?limit=50" | jval total)"

say "9. 节点自述与词表"
META="$(curl -sS "${BASE}/api/meta")"
contains "自述声明了 kb 能力" '"kbDocs"' "${META}"
contains "自述列了 kb 能力" '"kb"' "${META}"
contains "自述列了 mem 能力" '"mem"' "${META}"
contains "自述列了 ckpt 能力" '"ckpt"' "${META}"
OUT="$("${CLI}" --base "${BASE}" kb kinds 2>&1)"
contains "kb kinds 报类型" "faq" "${OUT}"
contains "kb kinds 说清不是向量检索" "关键词" "${OUT}"
OUT="$("${CLI}" --base "${BASE}" mem kinds 2>&1)"
contains "mem kinds 报种类" "preference" "${OUT}"
contains "mem kinds 说清没有公开档" "没有公开档" "${OUT}"
OUT="$("${CLI}" --base "${BASE}" ckpt kinds 2>&1)"
contains "ckpt kinds 报粒度" "episode" "${OUT}"

say "10. 隔离性自检：真实 ~/.ncc 没被动过"
if [[ "${NCC_HOME}" == "${TMP}/home" ]]; then
  good "NCC_HOME=${NCC_HOME}（临时目录）"
else
  bad "NCC_HOME 不是临时目录，可能污染真实环境"
fi

printf '\n\033[1m结果\033[0m：%d 通过 · %d 失败\n' "${PASS}" "${FAIL}"
printf '（临时目录 %s —— 脚本已自行清理；registry 日志在上面那份 log 里）\n' "${TMP}"
[[ "${FAIL}" -eq 0 ]]
