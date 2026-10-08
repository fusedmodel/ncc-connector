#!/usr/bin/env bash
# NCC Feedback 端到端冒烟（节点侧；设计见 prd/ncc-feedback.md）。
#
# 验的是这几条链与红线：
#   ① 跨用户：bob 给 alice 的制品说一句 —— alice 收得到，**别人看不到**（默认私有）；
#   ② 跨 Agent：哪个 Agent 说的要记下来（summary 里能看到分布）；
#   ③ 只追加：内容没有"改"这条路；唯一能改的是**处置状态**，且只有目标拥有者能改；
#   ④ 回复也是一条反馈，而且**继承父的可见性**（一条私有对话不会因为有人回一句就公开）；
#   ⑤ 归属解析：解析不出归属时**说清楚**（resolved=false + note），不装作对上；
#   ⑥ 聚合不是排名分：summary 明确说不参与匹配/排序；
#   ⑦ relay 的幂等键（origin+originId）：重复搬运只落一次；
#   ⑧ fail-closed：匿名只看得到 public；没登录不能写；作用域不够写不进去。
#
# 脚本自带启停，端口默认 18406（避开其它冒烟）；数据全在临时目录里。
# 用法：bash scripts/feedback-smoke.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-18406}"
BASE="http://127.0.0.1:${PORT}"
TMP="$(mktemp -d)"
export NCCR_PORT="${PORT}"
export NCCR_DATA_DIR="${TMP}/data"
export NCCR_JWT_SECRET="feedback-smoke"
export NCCR_NODE_NAME="smoke-node"

PASS=0
FAIL=0
PID=""
cleanup() {
  [[ -n "${PID}" ]] && kill "${PID}" 2>/dev/null || true
  wait 2>/dev/null || true
  if [[ -n "${KEEP:-}" ]]; then echo "（KEEP=1）保留 ${TMP}"; else rm -rf "${TMP}"; fi
}
trap cleanup EXIT

say()  { printf '\n\033[1m%s\033[0m\n' "$1"; }
good() { printf '  \033[32m✓\033[0m %s\n' "$1"; PASS=$((PASS + 1)); }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$1"; FAIL=$((FAIL + 1)); }
check() { if [[ "$2" == "$3" ]]; then good "$1（$3）"; else bad "$1：期望 $2，实际 $3"; fi; }
contains() { if [[ "$3" == *"$2"* ]]; then good "$1"; else bad "$1：输出里没有「$2」（实际：$(printf '%s' "$3" | head -c 300)）"; fi; }
not_contains() { if [[ "$3" == *"$2"* ]]; then bad "$1：不该出现「$2」（实际：$(printf '%s' "$3" | head -c 300)）"; else good "$1"; fi; }
alive() {
  local deadline=$((SECONDS + 40))
  while (( SECONDS < deadline )); do curl -fsS "${BASE}/api/meta" >/dev/null 2>&1 && return 0; sleep 0.3; done
  return 1
}
# code <方法> <路径> [token] [body] [额外头...] → 打印 HTTP 状态码
#
# ⚠️ 不要用 `shift 4`：只给两个参数时 shift 会失败，剩下的 $@ 会把 GET/路径
# 再拼到 curl 参数里（报 "URL rejected: No host part"）。用数组下标取额外参数。
code() {
  local m="$1" p="$2" tok="${3:-}" body="${4:-}"
  local args=(-sS -o /dev/null -w '%{http_code}' -X "$m" "${BASE}${p}")
  [[ -n "$tok" ]] && args+=(-H "Authorization: Bearer ${tok}")
  [[ -n "$body" ]] && args+=(-H 'Content-Type: application/json' -d "$body")
  if [[ $# -gt 4 ]]; then args+=("${@:5}"); fi
  curl "${args[@]}"
}
# bodyof <方法> <路径> [token] [body] [额外头...] → 打印响应体
bodyof() {
  local m="$1" p="$2" tok="${3:-}" body="${4:-}"
  local args=(-sS -X "$m" "${BASE}${p}")
  [[ -n "$tok" ]] && args+=(-H "Authorization: Bearer ${tok}")
  [[ -n "$body" ]] && args+=(-H 'Content-Type: application/json' -d "$body")
  if [[ $# -gt 4 ]]; then args+=("${@:5}"); fi
  curl "${args[@]}"
}
jget() { python3 -c "import json,sys;d=json.load(sys.stdin);print(eval('d'+sys.argv[1]))" "$1"; }
# fb <方法> <路径> [token] [body] → 上面两个的简写（反馈面默认带 JSON 头）

say "0. 构建并起节点（数据全在临时目录）"
mkdir -p "${TMP}/bin"
( cd "${ROOT}/rust" && cargo build --release -q --bin ncc-registry && cp target/release/ncc-registry "${TMP}/bin/ncc-registry" )
"${TMP}/bin/ncc-registry" >"${TMP}/node.log" 2>&1 &
PID=$!
alive && good "节点就绪" || { bad "节点起不来（看 ${TMP}/node.log）"; tail -20 "${TMP}/node.log"; exit 1; }

check "节点声明了 feedback 能力" "feedback" \
  "$(bodyof GET /api/meta | python3 -c "import json,sys;c=json.load(sys.stdin)['capabilities'];print('feedback' if 'feedback' in c else '缺')")"

KINDS="$(bodyof GET /api/feedback/kinds)"
check "词表给了 aboutKind 七档" "7" "$(printf '%s' "${KINDS}" | jget "['aboutKinds'].__len__()")"
contains "三条红线跟代码里是同一份" "默认私有" "${KINDS}"
contains "上限写在服务端（不是客户端猜）" "maxBody" "${KINDS}"

say "1. 两个身份 + 一个制品（谁的东西就是谁的）"
A_TOK="$(bodyof POST /api/auth/register "" '{"email":"alice@fb.dev","password":"feedback1234","name":"Alice"}' | jget "['token']")"
B_TOK="$(bodyof POST /api/auth/register "" '{"email":"bob@fb.dev","password":"feedback1234","name":"Bob"}' | jget "['token']")"
[[ -n "${A_TOK}" && -n "${B_TOK}" ]] && good "alice / bob 都登录了" || { bad "注册失败"; exit 1; }
NS_A="$(bodyof GET /api/namespaces/mine "${A_TOK}" | jget "['namespaces'][0]['slug']")"
printf 'echo hello\n' >"${TMP}/tool.sh"
UP="$(curl -sS -X POST "${BASE}/api/registry/uploads" -H "Authorization: Bearer ${A_TOK}" \
  -H 'X-Filename: tool.sh' --data-binary @"${TMP}/tool.sh")"
URL="$(printf '%s' "${UP}" | jget "['storageUrl']")"
SHA="$(printf '%s' "${UP}" | jget "['sha256']")"
ITEM="$(bodyof POST /api/registry "${A_TOK}" '{"kind":"skill","name":"Alice Tool","slug":"alice-tool","version":"0.1.0","summary":"冒烟用","status":"published","visibility":"public","storage":{"url":"'"${URL}"'","sha256":"'"${SHA}"'","size":12}}')"
REF="$(printf '%s' "${ITEM}" | jget "['item']['ref']")"
[[ -n "${REF}" ]] && good "alice 发布了一个制品：${REF}" || { bad "发布失败：${ITEM}"; exit 1; }

say "2. 跨用户：bob 说一句 → 只有 alice 与该说的人看得到"
W="$(bodyof POST /api/feedback "${B_TOK}" '{"aboutKind":"artifact","aboutRef":"'"${REF}"'","kind":"report","body":"装完后 run 报 ENOENT","tags":["install","bug"],"traceRef":"TR-abc"}')"
FB_ID="$(printf '%s' "${W}" | jget "['feedback']['id']")"
check "写成功（201 的 body 里有 id）" "true" "$([[ -n "${FB_ID}" ]] && echo true || echo false)"
check "归属解析成功（写进 owner）" "True" "$(printf '%s' "${W}" | jget "['resolved']")"
check "作者记的是 bob，不是 alice" "Bob" "$(printf '%s' "${W}" | jget "['feedback']['author']['handle']")"
check "默认私有" "private" "$(printf '%s' "${W}" | jget "['feedback']['visibility']")"
check "默认待处理" "open" "$(printf '%s' "${W}" | jget "['feedback']['status']")"
check "链路里记了本机这一跳" "node:smoke-node" \
  "$(printf '%s' "${W}" | jget "['feedback']['hops'].__str__().replace(' ','').replace('[','').replace(']','').replace(chr(39),'')")"
check "引用不搬内容：traceRef 记下来了" "TR-abc" "$(printf '%s' "${W}" | jget "['feedback']['traceRef']")"
check "匿名看不到私有的那条" "0" "$(bodyof GET "/api/feedback?aboutRef=${REF}" | jget "['total']")"
check "作者自己看得到" "1" "$(bodyof GET "/api/feedback?aboutRef=${REF}" "${B_TOK}" | jget "['total']")"
check "目标拥有者（alice）也看得到" "1" "$(bodyof GET "/api/feedback?aboutRef=${REF}" "${A_TOK}" | jget "['total']")"
check "收件箱里就是它" "${FB_ID}" "$(bodyof GET "/api/feedback?owner=me" "${A_TOK}" | jget "['feedback'][0]['id']")"
check "别人的收件箱里没有" "0" "$(bodyof GET "/api/feedback?owner=me" "${B_TOK}" | jget "['total']")"
check "标了 toMe（客户端不用自己猜）" "True" "$(bodyof GET "/api/feedback/${FB_ID}" "${A_TOK}" | jget "['feedback']['toMe']")"
check "标了 mine（作者视角）" "True" "$(bodyof GET "/api/feedback/${FB_ID}" "${B_TOK}" | jget "['feedback']['mine']")"
not_contains "未登录的 get 不泄露存在性" "ENOENT" "$(bodyof GET "/api/feedback/${FB_ID}")"

say "3. 跨 Agent：谁说的要记下来（人 / Agent / 两者一起）"
BY_AGENT="$(bodyof POST /api/feedback "${B_TOK}" \
  '{"aboutKind":"artifact","aboutRef":"'"${REF}"'","kind":"praise","body":"跑得通","agent":"claude-code"}' \
  -H 'NCC-Agent: claude-code')"
check "Agent 身份记在 agent 字段" "claude-code" "$(printf '%s' "${BY_AGENT}" | jget "['feedback']['agent']")"
SUM_AG="$(bodyof GET "/api/feedback/summary?aboutRef=${REF}" "${A_TOK}")"
check "聚合里看得到 Agent 分布" "claude-code" \
  "$(printf '%s' "${SUM_AG}" | jget "['summary']['agents'].__str__()" | sed "s/[{}']//g" | cut -d: -f1)"
check "（人自己）也单独一档" "true" \
  "$(printf '%s' "${SUM_AG}" | jget "['summary']['agents'].__str__()" | grep -q '人自己' && echo true || echo false)"

say "4. 公开与私有：公开要作者显式说"
PUB="$(bodyof POST /api/feedback "${B_TOK}" \
  '{"aboutKind":"artifact","aboutRef":"'"${REF}"'","kind":"request","body":"希望支持 --quiet","visibility":"public"}')"
PUB_ID="$(printf '%s' "${PUB}" | jget "['feedback']['id']")"
check "显式公开后匿名可见（只看得到公开的那部分）" "1" \
  "$(bodyof GET "/api/feedback?aboutRef=${REF}&visibility=public" | jget "['total']")"
check "匿名视图里没有私有的东西" "False" \
  "$(bodyof GET "/api/feedback?aboutRef=${REF}" | python3 -c "
import json,sys
d=json.load(sys.stdin)
print(any(x['visibility']!='public' for x in d['feedback']))")"
check "alice 看得到全部三条" "3" "$(bodyof GET "/api/feedback?aboutRef=${REF}" "${A_TOK}" | jget "['total']")"

say "5. 只追加：内容没有「改」这条路，能改的只有处置状态"
check "没有 PUT（内容不可改）" "404" "$(code PUT "/api/feedback/${FB_ID}" "${A_TOK}" '{"body":"改一下"}')"
check "PATCH 只认 status：乱给字段被拒" "400" "$(code PATCH "/api/feedback/${FB_ID}" "${A_TOK}" '{"body":"改一下"}')"
check "别人改不了处置状态（不是他的东西）" "403" "$(code PATCH "/api/feedback/${FB_ID}" "${B_TOK}" '{"status":"resolved"}')"
check "目标拥有者能改" "200" "$(code PATCH "/api/feedback/${FB_ID}" "${A_TOK}" '{"status":"resolved"}')"
check "改完状态真的变了（但正文没动）" "resolved:装完后 run 报 ENOENT" \
  "$(bodyof GET "/api/feedback/${FB_ID}" "${A_TOK}" | python3 -c "
import json,sys
f=json.load(sys.stdin)['feedback']
print(f['status']+':'+f['body'])")"
check "非法状态被拒" "400" "$(code PATCH "/api/feedback/${FB_ID}" "${A_TOK}" '{"status":"fixed"}')"

say "6. 回复也是一条反馈：继承父的可见性"
REP="$(bodyof POST "/api/feedback/${FB_ID}/reply" "${A_TOK}" '{"body":"v0.1.1 修了"}')"
REP_ID="$(printf '%s' "${REP}" | jget "['feedback']['id']")"
check "回复的父指针指对了" "${FB_ID}" "$(printf '%s' "${REP}" | jget "['feedback']['parentId']")"
check "回复继承了私有（没法把一段私有对话回成公开的）" "private" "$(printf '%s' "${REP}" | jget "['feedback']['visibility']")"
check "回复继承了被反馈的东西（不用重复说）" "${REF}" "$(printf '%s' "${REP}" | jget "['feedback']['aboutRef']")"
check "回复不进主线列表（主线还是 3 条）" "3" "$(bodyof GET "/api/feedback?aboutRef=${REF}" "${A_TOK}" | jget "['total']")"
check "get 里按时间正序展开回复" "${REP_ID}" \
  "$(bodyof GET "/api/feedback/${FB_ID}" "${A_TOK}" | jget "['replies'][0]['id']")"
check "主线那条上标了回复数" "1" "$(bodyof GET "/api/feedback/${FB_ID}" "${A_TOK}" | jget "['feedback']['replies']")"
check "匿名回复被拒（先登录）" "401" "$(code POST "/api/feedback/${PUB_ID}/reply" "" '{"body":"匿名路过"}')"

say "7. 归属解析：解析不出就说清楚，不装作对上"
# 心跳：先拿到响应体，再解出 id —— 分两步写（不要在多行续行里塞管道 + 引号，
# 那是本项目踩过的老坑：看着一样，解析出来却不是你以为的那个）。
HB="$(bodyof POST /api/nodes/heartbeat "${A_TOK}" '{"name":"alice-machine","kind":"service","capabilities":["serve:mcp"]}')"
NODE_ID="$(printf '%s' "${HB}" | jget "['node']['id']")"
[[ -n "${NODE_ID}" ]] && good "alice 登记了一台节点：${NODE_ID}" || { bad "登记节点失败：${HB}"; exit 1; }
ONNODE="$(bodyof POST /api/feedback "${B_TOK}" '{"aboutKind":"node","aboutRef":"'"${NODE_ID}"'","kind":"report","body":"盘快满了"}')"
check "对节点说的：解析到了拥有者" "True" "$(printf '%s' "${ONNODE}" | jget "['resolved']")"
check "于是 alice 收得到（她拥有那台机器）" "1" \
  "$(bodyof GET "/api/feedback?aboutKind=node&aboutRef=${NODE_ID}" "${A_TOK}" | jget "['total']")"
ORPHAN="$(bodyof POST /api/feedback "${B_TOK}" \
  '{"aboutKind":"artifact","aboutRef":"@nobody/ghost","kind":"report","body":"找不到的东西"}')"
check "对不存在的东西说的：resolved=false" "False" "$(printf '%s' "${ORPHAN}" | jget "['resolved']")"
contains "而且说清了原因（私有反馈只有自己看得到）" "对不上" "$(printf '%s' "${ORPHAN}" | jget "['note']")"
PROF="$(bodyof POST /api/feedback "${B_TOK}" \
  '{"aboutKind":"profile","aboutRef":"@someone","kind":"praise","body":"名片不错"}')"
contains "名片在 hub 上这件事也说清了" "hub" "$(printf '%s' "${PROF}" | jget "['note']")"

say "8. 聚合：说清它不是排名分"
SUM="$(bodyof GET "/api/feedback/summary?owner=me" "${A_TOK}")"
check "总数按主线算（4 条：3 条关于制品 + 1 条关于节点）" "4" "$(printf '%s' "${SUM}" | jget "['summary']['count']")"
check "公开的先数出来" "1" "$(printf '%s' "${SUM}" | jget "['summary']['publicCount']")"
check "私有的也数" "3" "$(printf '%s' "${SUM}" | jget "['summary']['privateCount']")"
check "没有打分时 scored=0（0 分 ≠ 没打分）" "0" "$(printf '%s' "${SUM}" | jget "['summary']['scored']")"
contains "明确写了不是排序用的分数" "不参与任何匹配" "$(printf '%s' "${SUM}" | jget "['note']")"
RATE='{"aboutKind":"artifact","aboutRef":"'"${REF}"'","kind":"rating","body":"自己的东西自己夸"}'
RATE5='{"aboutKind":"artifact","aboutRef":"'"${REF}"'","kind":"rating","score":5,"body":"自己的东西自己夸"}'
RATE9='{"aboutKind":"artifact","aboutRef":"'"${REF}"'","kind":"rating","score":9}'
SELF="$(bodyof POST /api/feedback "${A_TOK}" "${RATE5}")"
check "带分的能写进去" "5" "$(printf '%s' "${SELF}" | jget "['feedback']['score']")"
check "kind=rating 不带分被拒（要么给分要么别叫 rating）" "400" "$(code POST /api/feedback "${A_TOK}" "${RATE}")"
check "打分越界被拒" "400" "$(code POST /api/feedback "${A_TOK}" "${RATE9}")"
SUM2="$(bodyof GET "/api/feedback/summary?aboutRef=${REF}" "${A_TOK}")"
check "打了分之后 scored=1、均分 5" "1:5" \
  "$(printf '%s' "${SUM2}" | python3 -c "
import json,sys;s=json.load(sys.stdin)['summary'];print(str(s['scored'])+':'+str(round(s['scoreAvg'])))")"
check "自己给自己记的单独数出来（让读的人能扣掉）" "1" "$(printf '%s' "${SUM2}" | jget "['summary']['selfCount']")"

say "9. relay：从别处搬上来的重复搬运只落一次"
MOVED='{"aboutKind":"topic","aboutRef":"内网反馈搬运","kind":"report","body":"来自 office 节点","visibility":"public","origin":"node:office","originId":"FB-below-1"}'
R1="$(bodyof POST /api/feedback "${B_TOK}" "${MOVED}")"
check "第一条搬进来了" "True" "$(printf '%s' "${R1}" | jget "['feedback']['id'] != ''")"
check "来源记在 origin 上" "node:office" "$(printf '%s' "${R1}" | jget "['feedback']['origin']")"
check "搬运的链路里除了 node 还有 cli 那一跳" "cli:node:office,node:smoke-node" \
  "$(printf '%s' "${R1}" | jget "['feedback']['hops'].__str__().replace(' ','').replace('[','').replace(']','').replace(chr(39),'')")"
R2="$(bodyof POST /api/feedback "${B_TOK}" "${MOVED}")"
check "第二条算重复（200 duplicated，不报错）" "True" "$(printf '%s' "${R2}" | jget "['duplicated']")"
check "库里只有一条" "1" \
  "$(bodyof GET "/api/feedback?aboutKind=topic&aboutRef=内网反馈搬运" | jget "['total']")"

say "10. 健壮性与门禁"
check "没登录不能写" "401" "$(code POST /api/feedback "" '{"aboutKind":"topic","aboutRef":"x","body":"hi"}')"
check "不认识的 kind 被拒" "400" \
  "$(code POST /api/feedback "${B_TOK}" '{"aboutKind":"topic","aboutRef":"x","kind":"rant","body":"hi"}')"
check "又没正文又没分被拒（空话没意义）" "400" \
  "$(code POST /api/feedback "${B_TOK}" '{"aboutKind":"topic","aboutRef":"x"}')"
check "不认识的 aboutKind 被拒" "400" \
  "$(code POST /api/feedback "${B_TOK}" '{"aboutKind":"universe","aboutRef":"x","body":"hi"}')"
check "列表查询里的 aboutKind 也校验" "400" "$(code GET "/api/feedback?aboutKind=universe")"
# 超长正文：先把 body 用 python 生成好（不要在参数里嵌两三层命令替换 + 引号）
LONG_BODY="$(python3 -c 'import json;print(json.dumps({"aboutKind":"topic","aboutRef":"long","body":"x"*5000}))')"
check "正文过长被拒" "400" "$(code POST /api/feedback "${B_TOK}" "${LONG_BODY}")"
# 作用域：只给 registry 的 key 写不了反馈（说得出口这件事本身也要授权）
KEY="$(bodyof POST /api/auth/keys "${B_TOK}" \
  '{"label":"only-registry","scopes":["registry:read","registry:download"]}' | jget "['secret']")"
[[ -n "${KEY}" ]] && good "签了一把只读作用域的 key" || bad "签 key 失败"
check "作用域不够 → 403（不是 401、也不是默默成功）" "403" \
  "$(code POST /api/feedback "${KEY}" '{"aboutKind":"topic","aboutRef":"x","body":"hi"}')"
check "但读公开的还是可以的" "200" "$(code GET "/api/feedback?visibility=public" "${KEY}")"

say "结果"
printf '  通过 %s · 失败 %s\n' "${PASS}" "${FAIL}"
[[ "${FAIL}" -eq 0 ]]
