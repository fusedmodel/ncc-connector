// Package httpapi ncc-registry 的 HTTP 层：认证 / 制品托管 / 节点托管 / 集群。
//
// 响应约定与 NCC 平台/CLI 保持一致，便于同一个 `ncc` 客户端直接对接：
//
//	成功  {"...": ...}                      错误  {"error":{"code","message"}}
//	鉴权  Authorization: Bearer <JWT 或 ncc_ API-Key>
package httpapi

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"log"
	"regexp"
	"strings"
	"sync"
	"time"

	"github.com/gin-gonic/gin"
	"golang.org/x/crypto/bcrypt"

	"github.com/fusedmodel/ncc-registry/config"
	"github.com/fusedmodel/ncc-registry/internal/p2p"
	"github.com/fusedmodel/ncc-registry/internal/secretbox"
	"github.com/fusedmodel/ncc-registry/model"
	"github.com/fusedmodel/ncc-registry/storage"
	"github.com/fusedmodel/ncc-registry/store"
)

// Version 服务版本（/api/meta 与集群上报都用它）。
const Version = "ncc-registry/0.1.1"

// Server 依赖聚合。
type Server struct {
	Cfg  *config.Config
	St   *store.Store
	Blob storage.Storage
	hub  *clusterHub
	// Box 配置内容的静态加密器（secret=true 的配置）。密钥由节点密钥派生，
	// 换节点/丢数据目录就打不开 —— 这是设计意图，不是缺陷。
	Box *secretbox.Box

	// p2pResponder 可选的「可被打洞」入口（NCCR_P2P_SERVE / p2p.serve 打开）。
	// 它只应答 STUN Binding 请求，不接收任何业务字节。
	p2pMu        sync.Mutex
	p2pResponder *p2p.Responder
}

// AuthInfo 认证上下文（JWT 会话 / 节点令牌 / API-Key）。
type AuthInfo struct {
	UserID  string
	Email   string
	Kind    string // user | node | key
	KeyID   string
	NodeID  string // kind=node：票据兑换出来的节点令牌绑定的节点
	Scopes  []string
	Session bool // true = JWT 用户会话（代表用户本人，不受作用域限制）
}

const authCtxKey = "nccr:auth"

var emailRe = regexp.MustCompile(`^[^\s@]+@[^\s@]+\.[^\s@]+$`)

/* ---------------- 密码 ---------------- */

func hashPassword(pw string) (string, error) {
	b, err := bcrypt.GenerateFromPassword([]byte(pw), bcrypt.DefaultCost)
	return string(b), err
}

func verifyPassword(pw, hash string) bool {
	return bcrypt.CompareHashAndPassword([]byte(hash), []byte(pw)) == nil
}

/* ---------------- HS256 JWT（手写，不引入依赖） ---------------- */

func b64url(b []byte) string { return base64.RawURLEncoding.EncodeToString(b) }

// tokenClaims 令牌载荷。kind 决定它能做什么：
//
//	user  用户会话（JWT 登录）：代表用户本人，不受作用域限制
//	node  节点令牌（由接入票据兑换）：只能用票据给的作用域
type tokenClaims struct {
	Sub   string   `json:"sub"`
	Email string   `json:"email,omitempty"`
	Kind  string   `json:"kind"`
	Node  string   `json:"node,omitempty"`
	Scope []string `json:"scope,omitempty"`
	Iat   int64    `json:"iat"`
	Exp   int64    `json:"exp"`
}

func signJWT(secret, sub, email string, ttl time.Duration) string {
	return signClaims(secret, tokenClaims{Sub: sub, Email: email, Kind: "user"}, ttl)
}

func signClaims(secret string, c tokenClaims, ttl time.Duration) string {
	now := time.Now().Unix()
	c.Iat, c.Exp = now, now+int64(ttl.Seconds())
	header := b64url([]byte(`{"alg":"HS256","typ":"JWT"}`))
	raw, err := json.Marshal(c)
	if err != nil {
		return ""
	}
	payload := b64url(raw)
	return header + "." + payload + "." + hmacSHA256(secret, header+"."+payload)
}

func hmacSHA256(secret, data string) string {
	m := hmac.New(sha256.New, []byte(secret))
	_, _ = m.Write([]byte(data))
	return b64url(m.Sum(nil))
}

func parseJWT(secret, token string) (*AuthInfo, error) {
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		return nil, errors.New("jwt 段数错误")
	}
	expect := hmacSHA256(secret, parts[0]+"."+parts[1])
	if !hmac.Equal([]byte(parts[2]), []byte(expect)) {
		return nil, errors.New("jwt 签名无效")
	}
	raw, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return nil, err
	}
	var claims tokenClaims
	if err := json.Unmarshal(raw, &claims); err != nil {
		return nil, err
	}
	if claims.Sub == "" {
		return nil, errors.New("jwt 缺少主体")
	}
	if claims.Exp < time.Now().Unix() {
		return nil, errors.New("jwt 已过期")
	}
	switch claims.Kind {
	case "user":
		return &AuthInfo{UserID: claims.Sub, Kind: "user"}, nil
	case "node":
		return &AuthInfo{UserID: claims.Sub, Kind: "node", NodeID: claims.Node, Scopes: claims.Scope}, nil
	}
	return nil, errors.New("jwt 类型错误")
}

/* ---------------- 中间件 ---------------- */

func (s *Server) authMiddleware() gin.HandlerFunc {
	return func(c *gin.Context) {
		if h := c.GetHeader("Authorization"); strings.HasPrefix(h, "Bearer ") {
			token := strings.TrimSpace(strings.TrimPrefix(h, "Bearer "))
			if strings.HasPrefix(token, "ncc_") {
				if k, err := s.St.FindKeyBySecret(token); err == nil {
					_ = s.St.TouchApiKey(k.ID)
					if u, err := s.St.FindUserByID(k.UserID); err == nil && !u.Disabled {
						c.Set(authCtxKey, &AuthInfo{
							UserID: u.ID, Email: u.Email, Kind: "key",
							KeyID: k.ID, Scopes: store.ParseList(k.Scopes),
						})
					}
				}
			} else if info, err := parseJWT(s.Cfg.JWTSecret, token); err == nil {
				// 用 JWT 里的 id 回查一次库：被禁用的账号**旧令牌立即失效**
				// （否则「禁用」就只是拦登录，已经登进来的人照样通行）。
				if u, err := s.St.FindUserByID(info.UserID); err == nil && !u.Disabled {
					info.Email = u.Email
					// 只有用户会话代表本人（不受作用域限制）；节点令牌按票据作用域走。
					info.Session = info.Kind == "user"
					c.Set(authCtxKey, info)
				}
			}
		}
		c.Next()
	}
}

func authOf(c *gin.Context) *AuthInfo {
	if v, ok := c.Get(authCtxKey); ok {
		if a, ok := v.(*AuthInfo); ok {
			return a
		}
	}
	return nil
}

// DefaultScopes 新建 key 的默认作用域（除 keys:write，避免「一把 key 再生出更多 key」）。
var DefaultScopes = []string{
	"registry:read", "registry:download", "registry:publish",
	"nodes:read", "nodes:write", "grants:read", "grants:write",
	"config:read", "config:write",
	"trace:read", "trace:write",
	// 三样状态：默认给读写（它们是"我的 Agent 的状态"，新建 key 就是我的）
	"kb:read", "kb:write", "mem:read", "mem:write", "ckpt:read", "ckpt:write",
	// 通用记录仓：一个作用域对**所有集合**生效（集合级的边界靠命名空间归属与认证，不靠作用域爆炸）
	"store:read", "store:write",
	"p2p:read", "p2p:write",
}

// IndexScopes 索引相关作用域：写 = 接收平台推来的索引，读 = 本地检索与匹配。
//
// 刻意**不进 DefaultScopes**：索引的权威在平台，节点侧只是副本 —— 一把新签的
// key 该能读（检索便宜且无害），但要往节点写索引，得显式给 `index:write`。
var IndexScopes = []string{"index:read", "index:write"}

// NodeTicketScopes 接入票据兑换出的节点令牌默认作用域：
// 能上报自己的心跳、能看/拉公开制品，但**不能发布**别人的条目。
// 要让它管理配置，签发时显式加 --scopes config:read,config:write。
var NodeTicketScopes = []string{
	"nodes:write", "registry:read", "registry:download",
}

// AllScopes 可用作用域目录。
//
// `trace:label` 与 `trace:write` 分开：**采集轨迹**与**给轨迹下判断**是两件事，
// 前者是 Agent 日常要干的（接入票据里显式加 `--scopes trace:write` 即可），
// 后者是一次评测动作，不该顺着默认凭据自动拿到。
var AllScopes = []string{
	"registry:read", "registry:download", "registry:publish",
	"nodes:read", "nodes:write",
	"grants:read", "grants:write",
	"config:read", "config:write",
	"trace:read", "trace:write", "trace:label",
	"kb:read", "kb:write", "mem:read", "mem:write", "ckpt:read", "ckpt:write",
	"store:read", "store:write",
	"p2p:read", "p2p:write",
	"index:read", "index:write",
	// 远程执行（云电脑）：接活与看自己的任务分开 —— 能提交不等于能看别人的任务。
	"exec:read", "exec:write",
	"keys:write",
}

// ValidScope 校验作用域。
func ValidScope(sc string) bool {
	for _, v := range AllScopes {
		if v == sc {
			return true
		}
	}
	return false
}

// scopeImplies 作用域蕴含：写包含读。
func scopeImplies(have, want string) bool {
	if have == want {
		return true
	}
	switch have {
	case "registry:publish":
		return want == "registry:download" || want == "registry:read"
	case "registry:download":
		return want == "registry:read"
	case "nodes:write":
		return want == "nodes:read"
	case "grants:write":
		return want == "grants:read"
	case "config:write":
		return want == "config:read"
	case "trace:write", "trace:label":
		return want == "trace:read"
	case "kb:write":
		return want == "kb:read"
	case "mem:write":
		return want == "mem:read"
	case "ckpt:write":
		return want == "ckpt:read"
	case "store:write":
		return want == "store:read"
	case "index:write":
		return want == "index:read"
	}
	return false
}

func allow(a *AuthInfo, scope string) bool {
	if a == nil {
		return false
	}
	if a.Session {
		return true
	}
	for _, sc := range a.Scopes {
		if scopeImplies(sc, scope) {
			return true
		}
	}
	return false
}

func requireAuth() gin.HandlerFunc {
	return func(c *gin.Context) {
		if authOf(c) == nil {
			fail(c, 401, "unauthorized", "未认证或凭据无效（先 ncc login 或带 API-Key）")
			c.Abort()
			return
		}
		c.Next()
	}
}

func requireScope(scope string) gin.HandlerFunc {
	return func(c *gin.Context) {
		a := authOf(c)
		if a == nil {
			fail(c, 401, "unauthorized", "未认证或凭据无效（先 ncc login 或带 API-Key）")
			c.Abort()
			return
		}
		if !allow(a, scope) {
			fail(c, 403, "scope_required", "当前凭据缺少作用域 "+scope)
			c.Abort()
			return
		}
		c.Next()
	}
}

/* ---------------- 响应 ---------------- */

func fail(c *gin.Context, status int, code, msg string) {
	c.AbortWithStatusJSON(status, gin.H{"error": gin.H{"code": code, "message": msg}})
}

// httpErr 带语义的错误：状态码与错误码随错误走，不用靠字符串猜。
type httpErr struct {
	status int
	code   string
	msg    string
}

func (e *httpErr) Error() string { return e.msg }

func errWith(status int, code, msg string) error {
	return &httpErr{status: status, code: code, msg: msg}
}

func errStatus(err error) int {
	var he *httpErr
	if errors.As(err, &he) {
		return he.status
	}
	return 500
}

func errCode(err error) string {
	var he *httpErr
	if errors.As(err, &he) {
		return he.code
	}
	return "internal"
}

// failErr 直接按错误语义输出。
func failErr(c *gin.Context, err error) { fail(c, errStatus(err), errCode(err), err.Error()) }

func ok(c *gin.Context, status int, data gin.H) { c.JSON(status, data) }

func timeNow() string { return time.Now().Format(time.RFC3339) }

func jsonMarshal(v any) ([]byte, error) { return json.Marshal(v) }

// logf 集群/分发的运行日志（这些事在机器之间发生，得留下痕迹）。
func logf(format string, args ...any) { log.Printf(format, args...) }

func isConflict(err error) bool {
	return err != nil && strings.Contains(strings.ToLower(err.Error()), "unique")
}

/* ---------------- 视图 ---------------- */

func userJSON(u *model.User) gin.H {
	return gin.H{
		"id": u.ID, "email": u.Email, "name": u.Name,
		"plan": u.Plan, "createdAt": u.CreatedAt,
		// 治理身份：CLI / 控制台据此决定要不要显示管理入口。
		"isAdmin": u.IsAdmin, "disabled": u.Disabled,
	}
}

func nsJSON(n *model.Namespace, isOwner bool) gin.H {
	return gin.H{
		"id": n.ID, "slug": n.Slug, "name": n.Name, "type": n.Type,
		"visibility": n.Visibility, "owner": isOwner, "createdAt": n.CreatedAt,
	}
}

func parseJSONAny(s string) any {
	if strings.TrimSpace(s) == "" {
		return nil
	}
	var v any
	if err := json.Unmarshal([]byte(s), &v); err != nil {
		return nil
	}
	return v
}

func artifactJSON(r *store.ArtifactRow) gin.H {
	out := gin.H{
		"id": r.ID, "kind": r.Kind, "name": r.Name, "slug": r.Slug, "version": r.Version,
		"summary": r.Summary, "tags": store.ParseList(r.Tags),
		"visibility": r.Visibility, "status": r.Status,
		"manifest": parseJSONAny(r.Manifest),
		"storage": gin.H{
			"provider": r.StorageProvider, "url": r.StorageURL,
			"sha256": r.SHA256, "size": r.Size,
		},
		"downloads": r.Downloads,
		"namespace": gin.H{"id": r.NamespaceID, "slug": r.NsSlug, "name": r.NsName},
		"ref":       "@" + r.NsSlug + "/" + r.Slug + "@" + r.Version,
		"origin":    r.Origin,
		"via":       gin.H{"kind": "self"},
		"createdAt": r.CreatedAt, "updatedAt": r.UpdatedAt,
	}
	if r.IsReplica() {
		out["replicaOf"] = r.OriginRef
	}
	return out
}

// nodeOnline 由 TTL 判断在线（不落库，避免心跳写放大）。
func nodeOnline(last time.Time, ttl time.Duration) bool {
	return time.Since(last) <= ttl
}

func nodeJSON(r *store.NodeRow, ttl time.Duration) gin.H {
	link := gin.H{"linked": false}
	if r.LinkID != "" {
		link = gin.H{"linked": true, "linkId": r.LinkID, "label": r.LinkLabel}
	}
	return gin.H{
		"id": r.ID, "slug": r.Slug, "name": r.Name, "kind": r.Kind, "region": r.Region,
		"url": r.URL, "os": r.OS, "arch": r.Arch, "version": r.Version, "agent": r.Agent,
		// 归一后再展示：老节点上报的 `mcp` 在界面上也显示成 `serve:mcp`，
		// 这样「我声明了什么」与「别人按什么搜我」永远对得上。
		"capabilities": model.NormalizeOffers(store.ParseList(r.Capabilities)),
		// 自证能力单独一份：`?can=<id>@verified` 就是拿它筛的。
		"capabilitiesVerified": model.NormalizeOffers(store.ParseList(r.OffersVerified)),
		"visibility":           r.Visibility,
		"status":               map[bool]string{true: "online", false: "offline"}[nodeOnline(r.LastSeen, ttl)],
		"online":               nodeOnline(r.LastSeen, ttl),
		"lastSeen":             r.LastSeen,
		"namespace":            gin.H{"slug": r.NsSlug, "name": r.NsName},
		"owner":                gin.H{"id": r.OwnerID, "name": r.OwnerName},
		"link":                 link,
	}
}

func workerJSON(w *model.ClusterWorker, ttl time.Duration) gin.H {
	return gin.H{
		"id": w.ID, "name": w.Name, "url": w.URL, "version": w.Version, "role": "worker",
		"region": w.Region, "capabilities": store.ParseList(w.Capabilities),
		"artifacts": w.Artifacts, "nodes": w.Nodes, "users": w.Users,
		"online": nodeOnline(w.LastSeen, ttl), "lastSeen": w.LastSeen, "firstSeen": w.FirstSeen,
	}
}

// selfNodeJSON 本节点的身份（集群与 /api/meta 都用它）。
func (s *Server) selfNodeJSON(artifacts, nodes, users int64) gin.H {
	// 远程执行的引擎是**本机事实**（runner 在不在、运维放行了什么）——以自证的形式
	// 报出去。这样「按能力挑机器」（`ncc nodes discover --can run:remote`）挑到的
	// 是**真的能跑**的机器，而不是"标签上写着能跑"的机器。
	offers := []string{}
	verified := []string{}
	cap := s.Exec()
	execOn := false
	for _, k := range cap.Kinds {
		if k.Enabled {
			execOn = true
			verified = append(verified, "run:"+k.ID)
		}
	}
	if execOn {
		offers = append(offers, "run:remote") // 本机接活
	}
	return gin.H{
		"id": s.Cfg.NodeID, "name": s.Cfg.NodeName, "url": s.Cfg.PublicURL,
		"role": s.Cfg.Role, "version": Version, "region": s.Cfg.NodeRegion,
		"artifacts": artifacts, "nodes": nodes, "users": users,
		"online": true, "lastSeen": time.Now(),
		// 提供能力：声明 + 自证（自证 = 由本机硬事实推导，见 model.ParseOfferQuery）
		"capabilities":         model.NormalizeOffers(offers),
		"capabilitiesVerified": model.NormalizeOffers(verified),
		"tags":                 cap.Tags,
	}
}

// Counts 本节点的规模指标。
func (s *Server) Counts() (artifacts, nodes, users int64) {
	artifacts, _ = s.St.CountArtifacts()
	nodes, _ = s.St.CountNodes()
	users, _ = s.St.CountUsers()
	return
}

/* ---------------- 引用解析 ---------------- */

// parseArtifactRef 解析制品引用：A-… 形式的 id 或 @ns/slug（可带 @version）。
func parseArtifactRef(ref string) (id, nsSlug, slug string) {
	ref = strings.TrimSpace(ref)
	if ref == "" {
		return "", "", ""
	}
	if strings.HasPrefix(ref, "@") {
		body := strings.TrimPrefix(ref, "@")
		if i := strings.LastIndex(body, "@"); i > 0 { // 去掉版本
			body = body[:i]
		}
		if i := strings.Index(body, "/"); i > 0 {
			return "", body[:i], body[i+1:]
		}
		return "", "", body
	}
	return ref, "", ""
}
