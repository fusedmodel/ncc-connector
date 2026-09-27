#!/usr/bin/env bash
# 通用记录仓（NCC Store）端到端冒烟。
#
# 验的是一句设计承诺：**集合是声明，记录是数据，服务端不认识业务字段** ——
# 于是新增一类内容（issue / log / …）**不用改服务端**，声明一个集合即可。
#
# 顺带把三条红线钉死在测试里：
#   ① 动态 ≠ 无模式：没声明的字段写不进来、也不能当过滤条件；
#   ② 不可变就是不可变：mutable=false / append_only=true 的集合没有 PUT；
#   ③ CRUD ≠ 授权：能写自己的集合，不代表能读/写别人的。
#
# 数据目录、库文件、blob 全在临时目录里，**不碰你真实的实例**。
# 用法：bash scripts/store-smoke.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-18402}"
BASE="http://127.0.0.1:${PORT}"
TMP="$(mktemp -d)"
export NCCR_PORT="${PORT}"
export NCCR_DATA_DIR="${TMP}/data"
export NCCR_JWT_SECRET="store-smoke"

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
nz() { if [[ "$2" != "0" ]]; then good "$1（退出码 $2）"; else bad "$1：本该失败却退出 0"; fi; }
alive() {
  local deadline=$((SECONDS + 40))
  while (( SECONDS < deadline )); do curl -fsS "${BASE}/api/meta" >/dev/null 2>&1 && return 0; sleep 0.3; done
  return 1
}
# code <方法> <路径> [token] [body] → 打印 HTTP 状态码
code() {
  local m="$1" p="$2" tok="${3:-}" body="${4:-}"
  local args=(-sS -o /dev/null -w '%{http_code}' -X "$m" "${BASE}${p}")
  [[ -n "$tok" ]] && args+=(-H "Authorization: Bearer ${tok}")
  [[ -n "$body" ]] && args+=(-H 'Content-Type: application/json' -d "$body")
  curl "${args[@]}"
}
# bodyof <方法> <路径> [token] [body] → 打印响应体
bodyof() {
  local m="$1" p="$2" tok="${3:-}" body="${4:-}"
  local args=(-sS -X "$m" "${BASE}${p}")
  [[ -n "$tok" ]] && args+=(-H "Authorization: Bearer ${tok}")
  [[ -n "$body" ]] && args+=(-H 'Content-Type: application/json' -d "$body")
  curl "${args[@]}"
}
jget() { python3 -c "import json,sys;d=json.load(sys.stdin);print(eval('d'+sys.argv[1]))" "$1"; }

say "0. 构建并起节点（数据全在临时目录）"
mkdir -p "${TMP}/bin"
( cd "${ROOT}" && go build -o "${TMP}/bin/ncc-registry" ./cmd/ncc-registry )
"${TMP}/bin/ncc-registry" >"${TMP}/node.log" 2>&1 &
PID=$!
alive && good "节点就绪" || { bad "节点起不来（看 ${TMP}/node.log）"; exit 1; }
check "节点声明了 store 能力" "store" \
  "$(bodyof GET /api/meta | python3 -c "import json,sys;c=json.load(sys.stdin)['capabilities'];print('store' if 'store' in c else '缺')")"

say "1. 两个身份（谁的就是谁的）"
A_TOK="$(bodyof POST /api/auth/register "" '{"email":"alice@store.dev","password":"store1234","name":"Alice"}' | jget "['token']")"
B_TOK="$(bodyof POST /api/auth/register "" '{"email":"bob@store.dev","password":"store1234","name":"Bob"}' | jget "['token']")"
[[ -n "${A_TOK}" && -n "${B_TOK}" ]] && good "alice / bob 都登录了" || { bad "注册失败"; exit 1; }
NS_A="$(bodyof GET /api/namespaces/mine "${A_TOK}" | jget "['namespaces'][0]['slug']")"
good "alice 的命名空间：${NS_A}"

say "2. 词表与上限（离线可读）"
OUT="$(bodyof GET /api/store/kinds)"
contains "给了字段类型白名单" "enum:a|b|c" "${OUT}"
contains "把三条红线写在词表里" "没在 index 里声明的字段不能当过滤条件" "${OUT}"
contains "说清了通用仓只存内联文本" "内联文本" "${OUT}"

say "3. 声明一个集合（issue）——**不改服务端**"
DECL='{"kind":"issue","title":"问题单","summary":"团队的问题与处置","visibility":"private",
       "fields":["title:string!","status:enum:open|closed|triaged","labels:string[]","body:text?search","owner:ref"],
       "index":["status","labels"]}'
check "声明成功" "201" "$(code POST /api/store "${A_TOK}" "${DECL}")"
check "重复声明是更新不是新建" "200" "$(code POST /api/store "${A_TOK}" "${DECL}")"
COL="$(bodyof GET "/api/store?namespace=@${NS_A}" "${A_TOK}")"
check "列出来能看到它" "issue" "$(printf '%s' "${COL}" | jget "['collections'][0]['kind']")"
check "默认可变、留历史" "True" "$(printf '%s' "${COL}" | jget "['collections'][0]['history']")"
# 声明的 JSON 键名是**接口的一部分**：客户端靠它读字段名/类型/词表。
# （真出过这个 bug：Go 的字段名没打 tag，于是 `Name`/`Type` 原样漏进 JSON，
#  而同一份响应的其它键是 camelCase —— 客户端得猜大小写才能读声明。）
check "字段声明的键是 camelCase（接口是契约）" "title" \
  "$(printf '%s' "${COL}" | jget "['collections'][0]['fields'][0]['name']")"
check "类型与必填也在" "True" \
  "$(python3 -c "import json,sys;fs=json.load(sys.stdin)['collections'][0]['fields'];print(next(f['require'] for f in fs if f['name']=='title'))" <<<"${COL}")"
check "枚举词表跟着声明走" "closed" \
  "$(python3 -c "import json,sys;fs=json.load(sys.stdin)['collections'][0]['fields'];print(next(f['enum'] for f in fs if f['name']=='status')[1])" <<<"${COL}")"
check "?search 的字段会被标出来" "True" \
  "$(python3 -c "import json,sys;fs=json.load(sys.stdin)['collections'][0]['fields'];print(next(f['search'] for f in fs if f['name']=='body'))" <<<"${COL}")"

say "4. 记录 CRUD（写 / 幂等 / 改 / 历史 / 归档）"
R1='{"key":"login-timeout","body":"登录偶发超时，怀疑连接池","fields":{"title":"登录超时","status":"open","labels":["bug","backend"],"owner":"@alice"},"tags":["p1"],"note":"初报"}'
check "建一条" "201" "$(code POST /api/store/issue "${A_TOK}" "${R1}")"
OUT="$(bodyof POST /api/store/issue "${A_TOK}" "${R1}")"
check "同内容重复提交算 duplicate，不刷版本" "True" "$(printf '%s' "${OUT}" | jget "['duplicate']")"
check "版本还是 1" "1" "$(printf '%s' "${OUT}" | jget "['record']['revision']")"
R2='{"body":"连接池上限 5，改成 20 后不再复现","fields":{"title":"登录超时","status":"triaged","labels":["bug"]},"note":"定界完成"}'
check "改一条" "200" "$(code PUT /api/store/issue/login-timeout "${A_TOK}" "${R2}")"
OUT="$(bodyof GET /api/store/issue/login-timeout "${A_TOK}")"
check "版本 +1" "2" "$(printf '%s' "${OUT}" | jget "['record']['revision']")"
contains "正文换成了新的" "连接池上限 5" "${OUT}"
OUT="$(bodyof GET /api/store/issue/login-timeout/history "${A_TOK}")"
check "历史里有上一版" "1" "$(printf '%s' "${OUT}" | jget "['history'][0]['revision']")"
contains "历史只记元数据（不存正文副本）" "不存正文副本" "${OUT}"
contains "历史行里没有正文字段" "OK" "$(printf '%s' "${OUT}" | python3 -c "import json,sys;h=json.load(sys.stdin)['history'][0];print('OK' if 'body' not in h else 'HAS_BODY')")"
# 备注**跟着版本走**：rev 1 那行带的是"写 rev 1 时给的备注"，当前版本另有 currentNote。
# （早先实现把"换掉 rev 1 时给的备注"记在 rev 1 头上 —— 于是**建记录时写的备注
#  永远看不到**。这是 e2e 冒烟抓出来的，不是放宽断言糊过去的。）
check "rev 1 的备注就是写它时给的（没被后一次覆盖）" "初报" "$(printf '%s' "${OUT}" | jget "['history'][0]['note']")"
check "当前版本的备注单独给（也没丢）" "定界完成" "$(printf '%s' "${OUT}" | jget "['currentNote']")"

say "5. 红线①：动态 ≠ 无模式"
R3='{"key":"undeclared","body":"x","fields":{"title":"t","severity":"high"}}'
check "没声明的字段写不进来" "400" "$(code POST /api/store/issue "${A_TOK}" "${R3}")"
contains "并告诉它该怎么办" "要么把它加进 fields" "$(bodyof POST /api/store/issue "${A_TOK}" "${R3}")"
R4='{"key":"no-title","body":"x","fields":{"status":"open"}}'
check "声明为必填的字段缺失被拒" "400" "$(code POST /api/store/issue "${A_TOK}" "${R4}")"
contains "说清是哪个字段必填" "是必填的" "$(bodyof POST /api/store/issue "${A_TOK}" "${R4}")"
R5='{"key":"bad-enum","body":"x","fields":{"title":"t","status":"whatever"}}'
check "枚举值不在词表里被拒" "400" "$(code POST /api/store/issue "${A_TOK}" "${R5}")"
check "字段过滤要带 f. 前缀（不然当无效参数拒）" "400" "$(code GET "/api/store/issue?severity=high&namespace=@${NS_A}" "${A_TOK}")"
check "没声明的字段带前缀也不行" "400" "$(code GET "/api/store/issue?f.severity=high&namespace=@${NS_A}" "${A_TOK}")"
contains "并列出能过滤的" "不是这个集合声明的字段" "$(bodyof GET "/api/store/issue?f.severity=high&namespace=@${NS_A}" "${A_TOK}")"
# 声明了字段 ≠ 能按它过滤：只有 index 里列过的才有索引行。
# 这里必须报错 —— 默默返回空会让人以为"真的没有这样的记录"。
check "声明了但没进 index 的字段不能当过滤条件（400，不是空结果）" "400" \
  "$(code GET "/api/store/issue?f.owner=@alice&namespace=@${NS_A}" "${A_TOK}")"
contains "并说清哪些字段能过滤" "能过滤的" \
  "$(bodyof GET "/api/store/issue?f.owner=@alice&namespace=@${NS_A}" "${A_TOK}")"

say "6. 声明的字段能过滤，q 只是子串"
check "按声明字段过滤命中" "1" "$(bodyof GET "/api/store/issue?f.status=triaged&namespace=@${NS_A}" "${A_TOK}" | jget "['total']")"
check "按声明字段过滤不误命中" "0" "$(bodyof GET "/api/store/issue?f.status=closed&namespace=@${NS_A}" "${A_TOK}" | jget "['total']")"
check "按 string[] 里的值过滤" "1" "$(bodyof GET "/api/store/issue?f.labels=bug&namespace=@${NS_A}" "${A_TOK}" | jget "['total']")"
check "q 搜正文" "1" "$(bodyof GET "/api/store/issue?q=连接池&namespace=@${NS_A}" "${A_TOK}" | jget "['total']")"
contains "如实说 q 不是索引检索、也不是向量检索" "不是索引检索" "$(bodyof GET "/api/store/issue?namespace=@${NS_A}" "${A_TOK}")"

say "6b. 关键词匹配：几个词都要出现（整句子串找不到的东西要能找到）"
# 造两条：一条两个词挨着，一条两个词隔开 —— 整句子串只能命中前者，
# 而人找东西时说「登录 超时」时两个词可能隔得很远。
bodyof POST /api/store/issue "${A_TOK}" \
  '{"key":"kw-tight","body":"登录超时","fields":{"title":"登录超时"}}' >/dev/null
bodyof POST /api/store/issue "${A_TOK}" \
  '{"key":"kw-loose","body":"登录页偶尔卡住，最后发现是连接池上限导致的超时","fields":{"title":"登录页面卡顿"}}' >/dev/null
check "整句「登录超时」只命中紧挨着的那条" "1" \
  "$(bodyof GET "/api/store/issue?q=%E7%99%BB%E5%BD%95%E8%B6%85%E6%97%B6&namespace=@${NS_A}" "${A_TOK}" | jget "['total']")"
check "拆成两个词就两条都命中（词可以隔开）" "2" \
  "$(bodyof GET "/api/store/issue?q=%E7%99%BB%E5%BD%95%20%E8%B6%85%E6%97%B6&namespace=@${NS_A}" "${A_TOK}" | jget "['total']")"
check "只给一个词给命中相关的那一条" "2" \
  "$(bodyof GET "/api/store/issue?q=%E8%B6%85%E6%97%B6&namespace=@${NS_A}" "${A_TOK}" | jget "['total']")"
# 排序：命中 key（权重 3）应当排在只有正文命中（权重 1）的前面。
# ⚠️ 用**英文**词做这条断言：记录 key 只允许小写字母数字与 -_.（中文进不去 key）。
bodyof POST /api/store/issue "${A_TOK}" \
  '{"key":"alpha-hit","body":"这条的正文里没有那个词","fields":{"title":"key 命中"}}' >/dev/null
bodyof POST /api/store/issue "${A_TOK}" \
  '{"key":"beta-thing","body":"这条的正文里出现了 alpha 这个词","fields":{"title":"只正文命中"}}' >/dev/null
check "命中 key 的排在只有正文命中的前面（按命中位置加权）" "alpha-hit" \
  "$(bodyof GET "/api/store/issue?q=alpha&namespace=@${NS_A}" "${A_TOK}" | jget "['records'][0]['key']")"
check "关键词里的标点也算分隔符（中英文逗号都行）" "2" \
  "$(bodyof GET "/api/store/issue?q=%E7%99%BB%E5%BD%95%EF%BC%8C%E8%B6%85%E6%97%B6&namespace=@${NS_A}" "${A_TOK}" | jget "['total']")"
contains "说清排序与候选上限（不假装是索引检索）" "候选" \
  "$(bodyof GET "/api/store/issue?namespace=@${NS_A}" "${A_TOK}")"
# 清掉这几条，后面的计数不受影响
for k in kw-tight kw-loose alpha-hit beta-thing; do
  bodyof DELETE "/api/store/issue/${k}?hard=1&namespace=@${NS_A}" "${A_TOK}" >/dev/null
done

say "7. 红线②：不可变就是不可变"
DECL_LOG='{"kind":"run-log","title":"运行日志","append_only":true,"visibility":"private",
           "fields":["step:string!","ok:bool"],"index":["step"]}'
check "声明一个只追加的集合" "201" "$(code POST /api/store "${A_TOK}" "${DECL_LOG}")"
check "追加一条" "201" "$(code POST /api/store/run-log "${A_TOK}" '{"key":"step-001","body":"start","fields":{"step":"boot","ok":true}}')"
check "改它 → 没有这条路径" "400" "$(code PUT /api/store/run-log/step-001 "${A_TOK}" '{"body":"改一下","fields":{"step":"boot","ok":true}}')"
contains "拒的时候说清了为什么" "只追加" "$(bodyof PUT /api/store/run-log/step-001 "${A_TOK}" '{"body":"x","fields":{"step":"a","ok":true}}')"
# 不变量不能只在某一条路由上判 —— 否则**换个动词就绕过去了**。
# （真事：POST 同一个 key 曾把只追加集合静默刷成 rev 2，被 e2e 冒烟抓出来。）
check "拿 POST 也不能改它（不变量不依赖你从哪个门进来）" "400" \
  "$(code POST /api/store/run-log "${A_TOK}" '{"key":"step-001","body":"改一下","fields":{"step":"boot","ok":false}}')"
contains "拒的时候说的是只追加" "只追加" \
  "$(bodyof POST /api/store/run-log "${A_TOK}" '{"key":"step-001","body":"改一下","fields":{"step":"boot","ok":false}}')"
# 但"一模一样地重试一次"是幂等的：没有任何东西被改过，不该报不可变
check "同样的内容重发算幂等重复（重试不报错）" "200" \
  "$(code POST /api/store/run-log "${A_TOK}" '{"key":"step-001","body":"start","fields":{"step":"boot","ok":true}}')"
check "而且没刷版本" "True" \
  "$(bodyof POST /api/store/run-log "${A_TOK}" '{"key":"step-001","body":"start","fields":{"step":"boot","ok":true}}' | jget "['duplicate']")"

say "8. 红线③：CRUD ≠ 授权"
check "bob 写不了 alice 的集合" "403" "$(code POST "/api/store/issue?namespace=@${NS_A}" "${B_TOK}" '{"key":"evil","body":"x","fields":{"title":"t"}}')"
check "匿名读私有集合的记录 → 读不到内容" "0" "$(bodyof GET "/api/store/issue?namespace=@${NS_A}" "" | jget "['total']")"
check "匿名读私有记录 → 403" "403" "$(code GET "/api/store/issue/login-timeout?namespace=@${NS_A}" "")"
check "alice 自己能读" "200" "$(code GET /api/store/issue/login-timeout "${A_TOK}")"

say "9. 公开档要**逐个集合**决定（记录自己说 public 不算）"
check "私有集合里想写公开记录 → 拒" "400" "$(code POST /api/store/issue "${A_TOK}" '{"key":"pub","body":"x","fields":{"title":"t"},"visibility":"public"}')"
DECL_PUB='{"kind":"notice","title":"公告","visibility":"public","fields":["title:string!","level:enum:info|warn"],"index":["level"]}'
check "声明一个公开集合" "201" "$(code POST /api/store "${A_TOK}" "${DECL_PUB}")"
check "写一条公开记录" "201" "$(code POST /api/store/notice "${A_TOK}" '{"key":"deploy-1","body":"今晚 22:00 发版","fields":{"title":"发版通知","level":"info"},"visibility":"public"}')"
check "匿名读得到" "200" "$(code GET "/api/store/notice/deploy-1?namespace=@${NS_A}" "")"
contains "内容也对" "今晚 22:00" "$(bodyof GET "/api/store/notice/deploy-1?namespace=@${NS_A}" "")"

say "10. 过期与清理（读时判过期，清理是另一个动作）"
check "写一条已过期的" "201" "$(code POST /api/store/notice "${A_TOK}" '{"key":"stale","body":"旧的","fields":{"title":"过期公告","level":"info"},"visibility":"public","expires_at":"2020-01-01T00:00:00Z"}')"
check "默认不列出过期项" "1" "$(bodyof GET "/api/store/notice?namespace=@${NS_A}" "${A_TOK}" | jget "['total']")"
check "想看就显式要" "2" "$(bodyof GET "/api/store/notice?expired=1&namespace=@${NS_A}" "${A_TOK}" | jget "['total']")"
check "真清理" "1" "$(bodyof POST /api/store/gc "${A_TOK}" '{"namespace":"@'"${NS_A}"'","collection":"notice"}' | jget "['removed']")"

say "11. 归档 ≠ 删除（与既有四类同一口径）"
check "归档一条" "200" "$(code DELETE /api/store/issue/login-timeout "${A_TOK}")"
check "默认不再列出" "0" "$(bodyof GET "/api/store/issue?namespace=@${NS_A}" "${A_TOK}" | jget "['total']")"
check "归档的还在" "1" "$(bodyof GET "/api/store/issue?archived=1&namespace=@${NS_A}" "${A_TOK}" | jget "['total']")"
check "归档集合（记录仍在）" "200" "$(code DELETE /api/store/notice "${A_TOK}")"
contains "归档文案说清不是删" "记录还在" "$(bodyof DELETE "/api/store/notice?namespace=@${NS_A}" "${A_TOK}")"

say "12. 幂等与并发（后写的别静默盖掉先写的）"
code POST /api/store/issue "${A_TOK}" '{"key":"race","body":"v1","fields":{"title":"并发","status":"open"}}' >/dev/null
check "带过期 revision 改 → 409（不是静默覆盖）" "409" \
  "$(code PUT /api/store/issue/race "${A_TOK}" '{"body":"v2","fields":{"title":"并发","status":"open"},"revision":99}')"
contains "并说清该怎么办" "别人先改了" "$(bodyof PUT /api/store/issue/race "${A_TOK}" '{"body":"v2","fields":{"title":"并发","status":"open"},"revision":99}')"
check "带对的 revision 就能改" "200" \
  "$(code PUT /api/store/issue/race "${A_TOK}" '{"body":"v2","fields":{"title":"并发","status":"open"},"revision":1}')"

say "21b. 控制台也看得到（人不用记命令）"
# 控制台是节点自带的单页（`/api/...` + 嵌入式 HTML）：能编译不等于能看 ——
# 所以断言页面里真的有这个区块，以及它依赖的接口在。
contains "页面里有记录仓区块" "内容（通用记录仓" "$(curl -fsS "${BASE}/")"
contains "页面里说清只看得到公开的" "控制台不带凭据" "$(curl -fsS "${BASE}/")"
contains "接口给了内置目录（没列出来就是一张骗人的清单）" "builtins" "$(curl -fsS "${BASE}/api/store")"
check "内置目录四类都在" "kb,mem,ckpt,trace" \
  "$(curl -fsS "${BASE}/api/store" | python3 -c 'import json,sys;print(",".join(b["kind"] for b in json.load(sys.stdin)["builtins"]))')"
contains "内置目录带着字段声明（清单本身就是说明）" "subject:string!" "$(curl -fsS "${BASE}/api/store")"

say "22. 口径收敛：五类内容共用同一份不变量（一处常量，五处返回）"
# 这不是“文案一致”的洁癖：归档/删、读不到/没有、CRUD/授权、过期语义、动态/无模式 ——
# 这几条是**同一个产品的同一条口径**。各写一遍，三个月后必有一条不一样，
# 然后用户就得记住“kb 是这样、mem 是那样”。所以断言五份**逐字相同**。
for ep in kb mem ckpt traces store; do
  curl -fsS "${BASE}/api/${ep}/kinds" > "${TMP}/kinds-${ep}.json" 2>/dev/null || echo '{}' > "${TMP}/kinds-${ep}.json"
done
python3 - "${TMP}" <<'PY'
import json, sys, os
base = sys.argv[1]
eps = ["kb", "mem", "ckpt", "traces", "store"]
ref = None
bad = []
for e in eps:
    try:
        d = json.load(open(os.path.join(base, f"kinds-{e}.json")))
    except Exception as ex:
        bad.append(f"{e}: 读不出来({ex})")
        continue
    inv = d.get("invariants")
    if not inv:
        bad.append(f"{e}: 没有 invariants")
        continue
    if ref is None:
        ref = inv
    elif inv != ref:
        bad.append(f"{e}: 与 {eps[0]} 不一致")
print("OK" if not bad else "BAD:" + "; ".join(bad))
PY
check "五个 kinds 都带同一份不变量" "OK" "$(python3 - "${TMP}" <<'PY'
import json, sys, os
base = sys.argv[1]
eps = ["kb", "mem", "ckpt", "traces", "store"]
ref = None
bad = []
for e in eps:
    try:
        d = json.load(open(os.path.join(base, f"kinds-{e}.json")))
    except Exception as ex:
        bad.append(f"{e}:bad")
        continue
    inv = d.get("invariants")
    if not inv:
        bad.append(f"{e}:missing")
    elif ref is None:
        ref = inv
    elif inv != ref:
        bad.append(f"{e}:differs")
print("OK" if not bad else "BAD:" + ";".join(bad))
PY
)"
check "不变量条数（归档/读不到/授权/过期/动态）" "5" \
  "$(python3 - "${TMP}/kinds-store.json" <<'PY'
import json, sys
print(len(json.load(open(sys.argv[1]))["invariants"]))
PY
)"
contains "归档 ≠ 删除 这条在每一类里都写着" "归档 ≠ 删除" "$(cat "${TMP}/kinds-mem.json")"
contains "读不到 ≠ 没有 也在" "读不到 ≠ 没有" "$(cat "${TMP}/kinds-ckpt.json")"
contains "CRUD ≠ 授权 也在" "CRUD ≠ 授权" "$(cat "${TMP}/kinds-traces.json")"

printf '\n\033[1m结果：%d 通过 / %d 失败\033[0m\n' "${PASS}" "${FAIL}"
[[ "${FAIL}" == "0" ]]
